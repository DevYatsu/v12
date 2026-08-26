//! Pass A — static capture analysis and storage layout.
//!
//! Walks the oxc AST once, mirroring the emitter's traversal shape exactly,
//! and records three things:
//!
//! 1. **Declarations** — every bound `SymbolId` with its declaring function
//!    unit, in source order (params first).
//! 2. **References** — `(symbol, referencing-unit)` pairs. Capture is
//!    resolved afterwards: a symbol referenced from any unit other than its
//!    home escapes into the home unit's heap Environment. Deferring the join makes use-before-declaration (hoisting)
//!    a non-issue without scope pre-scans.
//! 3. **Nested units** — functions/arrows get program function indices keyed
//!    by node span so the emitter can find them again.
//!
//! Identifier identity comes straight from oxc (`BindingIdentifier::
//! symbol_id`, `IdentifierReference::reference_id`), so no
//! shadowing logic lives here: distinct bindings are distinct symbols by
//! construction.
//!
//! Unsupported constructs are skipped here deliberately; the emitter rejects
//! them with a `CompileError`.

use oxc_ast::ast::{
    ArrayExpressionElement, ArrowFunctionExpression, BindingPattern, Expression, ForStatementInit,
    Function, Program, PropertyKey, SimpleAssignmentTarget, Statement, VariableDeclaration,
};
use oxc_semantic::{Scoping, SymbolId};
use oxc_span::GetSpan;

use crate::model::{MAX_ENV_SLOTS, MAX_REGS, Plans, REG_THIS, UnitPlan, VarLoc};

/// Entry point: produce finalized layout plans for a whole program.
pub fn collect(program: &Program<'_>, scoping: &Scoping) -> Plans {
    let mut c = Collector {
        scoping,
        plans: Plans::default(),
        ref_sites: Vec::new(),
        unit_stack: Vec::new(),
    };
    // Unit 0 = main script body.
    c.plans
        .units
        .push(UnitPlan::new(None, false, "<main>".into()));
    c.unit_stack.push(0);
    c.stmt_list(&program.body);
    let mut plans = c.plans;
    plans.ref_sites = c.ref_sites;
    finalize(&mut plans);
    plans
}

struct Collector<'s> {
    scoping: &'s Scoping,
    plans: Plans,
    /// `(symbol, referencing unit)` pairs; joined against homes in `finalize`.
    ref_sites: Vec<(SymbolId, usize)>,
    unit_stack: Vec<usize>,
}

impl<'s> Collector<'s> {
    fn cur_unit(&self) -> usize {
        *self.unit_stack.last().expect("unit stack underflow")
    }

    fn declare(&mut self, sym: SymbolId) {
        let unit = self.cur_unit();
        self.plans.home_of.insert(sym, unit);
        self.plans.units[unit].decl_order.push(sym);
    }

    fn note_ref(&mut self, sym: SymbolId) {
        self.ref_sites.push((sym, self.cur_unit()));
    }

    fn ref_symbol(&self, rid: Option<oxc_semantic::ReferenceId>) -> Option<SymbolId> {
        rid.and_then(|rid| self.scoping.get_reference(rid).symbol_id())
    }

    /// Registers a non-arrow function unit and walks params + body inside it.
    ///
    /// `declare_name_here` is `Some(())` for declarations (name binds in the
    /// enclosing unit) and `None` for expressions (named expressions bind
    /// their own name inside themselves).
    fn fn_unit(&mut self, f: &Function<'_>, declare_name_in_enclosing: bool, hint: &str) -> usize {
        if declare_name_in_enclosing
            && let Some(id) = &f.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.declare(sym);
        }
        let parent = self.cur_unit();
        let idx = self.plans.units.len();
        let name_hint = match f.id.as_ref() {
            Some(id) => format!("{hint}:{}", id.name),
            None => hint.to_string(),
        };
        self.plans
            .units
            .push(UnitPlan::new(Some(parent), false, name_hint));
        self.plans.fn_index.insert(f.span(), idx);
        self.unit_stack.push(idx);

        // Params register first so `decl_order[..param_count]` really is the
        // parameter list; the named-function-expression's own name follows.
        for p in &f.params.items {
            self.binding_pattern(&p.pattern);
        }
        if let Some(rest) = &f.params.rest {
            self.binding_pattern(&rest.rest.argument);
        }
        // Params are exactly the declarations registered so far in this unit.
        let param_count = self.plans.units[idx].decl_order.len();
        self.plans.units[idx].param_count = param_count;

        // Named function *expressions* bind their own name inside themselves,
        // after the params (it is an ordinary local of the body).
        if !declare_name_in_enclosing
            && let Some(id) = &f.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.declare(sym);
        }

        if let Some(body) = f.body.as_deref() {
            self.stmt_list(&body.statements);
        }
        self.unit_stack.pop();
        idx
    }

    fn arrow_unit(&mut self, a: &ArrowFunctionExpression<'_>) -> usize {
        let parent = self.cur_unit();
        let idx = self.plans.units.len();
        self.plans
            .units
            .push(UnitPlan::new(Some(parent), true, format!("<arrow>{}", idx)));
        self.plans.fn_index.insert(a.span(), idx);
        self.unit_stack.push(idx);
        for p in &a.params.items {
            self.binding_pattern(&p.pattern);
        }
        if let Some(rest) = &a.params.rest {
            self.binding_pattern(&rest.rest.argument);
        }
        let param_count = self.plans.units[idx].decl_order.len();
        self.plans.units[idx].param_count = param_count;
        match a.get_function_body() {
            Some(body) => self.stmt_list(&body.statements),
            None => {
                if let Some(expr) = a.get_expression() {
                    self.expr(expr);
                }
            }
        }
        self.unit_stack.pop();
        idx
    }

    fn stmt_list(&mut self, stmts: &[Statement<'_>]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Statement<'_>) {
        match s {
            Statement::BlockStatement(b) => self.stmt_list(&b.body),
            Statement::ExpressionStatement(e) => self.expr(&e.expression),
            Statement::IfStatement(i) => {
                self.expr(&i.test);
                self.stmt(&i.consequent);
                if let Some(alt) = &i.alternate {
                    self.stmt(alt);
                }
            }
            Statement::WhileStatement(w) => {
                self.expr(&w.test);
                self.stmt(&w.body);
            }
            Statement::DoWhileStatement(d) => {
                self.stmt(&d.body);
                self.expr(&d.test);
            }
            Statement::ForStatement(f) => {
                if let Some(init) = &f.init {
                    match init {
                        ForStatementInit::VariableDeclaration(v) => self.var_decl(v),
                        other => {
                            if let Some(expr) = other.as_expression() {
                                self.expr(expr);
                            }
                        }
                    }
                }
                if let Some(t) = &f.test {
                    self.expr(t);
                }
                if let Some(u) = &f.update {
                    self.expr(u);
                }
                self.stmt(&f.body);
            }
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    self.expr(arg);
                }
            }
            Statement::ThrowStatement(t) => self.expr(&t.argument),
            Statement::TryStatement(t) => {
                self.stmt_list(&t.block.body);
                if let Some(h) = &t.handler {
                    if let Some(param) = &h.param {
                        self.binding_pattern(&param.pattern);
                    }
                    self.stmt_list(&h.body.body);
                }
                if let Some(fin) = &t.finalizer {
                    self.stmt_list(&fin.body);
                }
            }
            Statement::LabeledStatement(l) => self.stmt(&l.body),
            Statement::FunctionDeclaration(f) => {
                self.fn_unit(f, true, "<fn>");
            }
            Statement::VariableDeclaration(v) => self.var_decl(v),
            // break / continue / empty / debugger carry no references.
            _ => {}
        }
    }

    fn var_decl(&mut self, v: &VariableDeclaration<'_>) {
        for d in &v.declarations {
            self.binding_pattern(&d.id);
            if let Some(init) = &d.init {
                self.expr(init);
            }
        }
    }

    fn binding_pattern(&mut self, p: &BindingPattern<'_>) {
        match p {
            BindingPattern::BindingIdentifier(id) => {
                if let Some(sym) = id.symbol_id.get() {
                    self.declare(sym);
                }
            }
            BindingPattern::ObjectPattern(o) => {
                for prop in &o.properties {
                    self.binding_pattern(&prop.value);
                }
                if let Some(rest) = &o.rest {
                    self.binding_pattern(&rest.argument);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    self.binding_pattern(el);
                }
                if let Some(rest) = &a.rest {
                    self.binding_pattern(&rest.argument);
                }
            }
            BindingPattern::AssignmentPattern(ap) => {
                self.binding_pattern(&ap.left);
                self.expr(&ap.right);
            }
        }
    }

    fn expr(&mut self, e: &Expression<'_>) {
        match e {
            Expression::Identifier(id) => {
                if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                    self.note_ref(sym);
                }
            }
            Expression::ThisExpression(_) => {
                // Arrows observe the nearest non-arrow unit's `this`; that
                // unit must thread it through its Environment.
                let mut owner = self.cur_unit();
                while self.plans.units[owner].is_arrow {
                    owner = self.plans.units[owner]
                        .parent
                        .expect("arrow below main unit");
                }
                self.plans.units[owner].needs_this = true;
            }
            Expression::FunctionExpression(f) => {
                self.fn_unit(f, false, "<fnexpr>");
            }
            Expression::ArrowFunctionExpression(a) => {
                self.arrow_unit(a);
            }
            Expression::BinaryExpression(b) => {
                self.expr(&b.left);
                self.expr(&b.right);
            }
            Expression::LogicalExpression(l) => {
                self.expr(&l.left);
                self.expr(&l.right);
            }
            Expression::UnaryExpression(u) => self.expr(&u.argument),
            Expression::UpdateExpression(_) => {
                let t = u_target(e);
                self.simple_target(t);
            }
            Expression::AssignmentExpression(a) => {
                if let Some(simple) = a.left.as_simple_assignment_target() {
                    self.simple_target(simple);
                }
                self.expr(&a.right);
            }
            Expression::ConditionalExpression(c) => {
                self.expr(&c.test);
                self.expr(&c.consequent);
                self.expr(&c.alternate);
            }
            Expression::SequenceExpression(sq) => {
                for x in &sq.expressions {
                    self.expr(x);
                }
            }
            Expression::CallExpression(c) => {
                self.expr(&c.callee);
                for arg in &c.arguments {
                    if let Some(x) = arg.as_expression() {
                        self.expr(x);
                    }
                }
            }
            Expression::ComputedMemberExpression(c) => {
                self.expr(&c.object);
                self.expr(&c.expression);
            }
            Expression::StaticMemberExpression(s) => self.expr(&s.object),
            Expression::PrivateFieldExpression(p) => self.expr(&p.object),
            Expression::ObjectExpression(o) => {
                for prop_kind in &o.properties {
                    if let Some(p) = prop_kind.as_property() {
                        match &p.key {
                            PropertyKey::StaticIdentifier(_)
                            | PropertyKey::PrivateIdentifier(_) => {}
                            key => {
                                if let Some(kx) = key.as_expression() {
                                    self.expr(kx);
                                }
                            }
                        }
                        self.expr(&p.value);
                    }
                }
            }
            Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    match el {
                        ArrayExpressionElement::SpreadElement(_)
                        | ArrayExpressionElement::Elision(_) => {}
                        _ => {
                            if let Some(x) = el.as_expression() {
                                self.expr(x);
                            }
                        }
                    }
                }
            }
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            // Substitution-free templates compile to plain strings; anything
            // else is rejected by the emitter.
            Expression::TemplateLiteral(_) => {}
            _ => {}
        }
    }

    fn simple_target(&mut self, t: &SimpleAssignmentTarget<'_>) {
        match t {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                    self.note_ref(sym);
                }
            }
            SimpleAssignmentTarget::ComputedMemberExpression(c) => {
                self.expr(&c.object);
                self.expr(&c.expression);
            }
            SimpleAssignmentTarget::StaticMemberExpression(s) => self.expr(&s.object),
            _ => {}
        }
    }
}

fn u_target<'a>(e: &'a Expression<'a>) -> &'a SimpleAssignmentTarget<'a> {
    match e {
        Expression::UpdateExpression(u) => &u.argument,
        _ => unreachable!("collect update target on non-update expression"),
    }
}

/// Assigns concrete storage after the walk: captured symbols escape into
/// their home unit's Environment, everything else gets sequential registers.
///
/// Layout order per unit is deterministic: declaration order (params first)
/// for both env slots and registers; the synthetic `this` slot (when an
/// arrow-descendant reads it) trails all named slots.
fn finalize(plans: &mut Plans) {
    // 1. Captures: referenced-from-outside ⇒ escapes.
    let sites = std::mem::take(&mut plans.ref_sites);
    let homes = &plans.home_of;
    plans.captured = sites
        .into_iter()
        .filter(|(sym, from)| homes.get(sym).is_some_and(|home| home != from))
        .map(|(sym, _)| sym)
        .collect();

    // 2. Per-unit storage layout.
    for ui in 0..plans.units.len() {
        let (escapes_here, needs_this) = {
            let unit = &plans.units[ui];
            let escapes = unit
                .decl_order
                .iter()
                .any(|s| plans.captured.contains(s) && homes.get(s).is_some_and(|h| *h == ui));
            (escapes, unit.needs_this)
        };
        {
            let unit = &mut plans.units[ui];
            unit.has_env = escapes_here || needs_this;
        }

        let mut slot: u8 = 0;
        let mut reg: u8 = REG_THIS + 1;
        let decl_count = plans.units[ui].decl_order.len();
        for i in 0..decl_count {
            let sym = plans.units[ui].decl_order[i];
            let is_home = homes.get(&sym).is_some_and(|h| *h == ui);
            if !is_home {
                continue;
            }
            if plans.captured.contains(&sym) {
                plans.units[ui].env_slots.insert(sym, slot);
                plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                slot += 1;
            } else {
                plans.units[ui].vars.insert(sym, VarLoc::Reg(reg));
                reg += 1;
            }
        }
        let unit = &mut plans.units[ui];
        if unit.needs_this && unit.has_env {
            unit.this_slot = Some(slot);
            slot += 1;
        }
        unit.env_slot_count = slot;
        unit.locals_end = reg;
        assert!(
            reg < MAX_REGS && slot < MAX_ENV_SLOTS,
            "register/slot overflow"
        );
    }
}
