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
    ArrayExpressionElement, ArrowFunctionExpression, AssignmentTarget, BindingPattern, Expression,
    ForStatementInit, FormalParameters, Function, ModuleDeclaration, ModuleExportName, Program,
    PropertyKey, SimpleAssignmentTarget, Statement, VariableDeclaration,
};
use oxc_semantic::{Scoping, SymbolId};
use oxc_span::GetSpan;

use crate::model::{
    CompileError, ExportEntry, ImportEntry, MAX_ENV_SLOTS, MAX_REGS, Plans, REG_THIS, UnitPlan,
    VarLoc,
};

/// Entry point: produce finalized layout plans for a whole program.
pub fn collect(
    program: &Program<'_>,
    scoping: &Scoping,
    is_strict: bool,
) -> Result<Plans, CompileError> {
    collect_inner(program, scoping, is_strict, false)
}

/// Module-mode collection: top-level `var` stays module-scoped.
pub fn collect_module(
    program: &Program<'_>,
    scoping: &Scoping,
    is_strict: bool,
) -> Result<Plans, CompileError> {
    collect_inner(program, scoping, is_strict, true)
}

fn collect_inner(
    program: &Program<'_>,
    scoping: &Scoping,
    is_strict: bool,
    is_module: bool,
) -> Result<Plans, CompileError> {
    let mut c = Collector {
        scoping,
        plans: Plans::default(),
        strict_stack: Vec::new(),
        ref_sites: Vec::new(),
        unit_stack: Vec::new(),
        super_allowed_stack: Vec::new(),
        super_call_allowed_stack: Vec::new(),
        private_name_scopes: Vec::new(),
        walking_params: false,
        early_error: None,
    };
    c.plans.is_module = is_module;
    // Unit 0 = main script body.
    let mut main_plan = UnitPlan::new(None, false, "<main>".into());
    main_plan.is_strict = is_strict;
    c.plans.units.push(main_plan);
    c.unit_stack.push(0);
    c.strict_stack.push(is_strict);
    c.super_allowed_stack.push(false);
    c.super_call_allowed_stack.push(false);
    c.stmt_list(&program.body);
    if let Some(err) = c.early_error {
        return Err(err);
    }
    let mut plans = c.plans;
    plans.ref_sites = c.ref_sites;
    // Strict-mode binding-name checks for declarations: `eval`/`arguments`
    // are never valid binding names in strict code, and the FutureReservedWords
    // (`implements`, `interface`, `let`, `package`, `private`, `protected`,
    // `public`, `static`, `yield`) are reserved (ES §12.1.1, §12.6.2).
    for unit in plans.units.iter() {
        if !unit.is_strict {
            continue;
        }
        for &sym in &unit.decl_order {
            let name = scoping.symbol_name(sym);
            if name == "eval" || name == "arguments" {
                return Err(CompileError {
                    message: format!("SyntaxError: '{name}' is not a valid binding in strict mode"),
                    span: None,
                });
            }
            if is_strict_reserved_word(name) {
                return Err(CompileError {
                    message: format!("SyntaxError: '{name}' is a reserved word in strict mode"),
                    span: None,
                });
            }
        }
    }
    finalize(&mut plans)?;
    Ok(plans)
}

/// The strict-mode-only reserved words: binding any of these in strict mode
/// code is an early SyntaxError (ES §12.1.1).
fn is_strict_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
    )
}

/// Best-effort key text for a class element or object-literal property key
/// (diagnostics/hint only).
fn static_key_or_default(key: &PropertyKey<'_>) -> String {
    crate::expr::static_key_text(key).unwrap_or_else(|| "<computed>".to_string())
}

/// `ClassElementName` text for a private name (`#x` → `#x`); `None` for
/// public/computed keys.
fn private_name_text(key: &oxc_ast::ast::PropertyKey<'_>) -> Option<String> {
    match key {
        oxc_ast::ast::PropertyKey::PrivateIdentifier(p) => Some(format!("#{}", p.name)),
        _ => None,
    }
}

/// Gathers the private names declared by a class body (`#x` fields, methods,
/// accessors — static and instance alike). The class's private environment
/// holds all of them regardless of placement.
fn declared_private_names(c: &oxc_ast::ast::Class<'_>) -> Vec<String> {
    let mut names = Vec::new();
    for el in &c.body.body {
        match el {
            oxc_ast::ast::ClassElement::MethodDefinition(m) => {
                if let Some(n) = private_name_text(&m.key) {
                    names.push(n);
                }
            }
            oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                if let Some(n) = private_name_text(&p.key) {
                    names.push(n);
                }
            }
            _ => {}
        }
    }
    names
}

/// `ContainsArguments` (ES §15.7.1): true when the expression textually
/// references the `arguments` identifier. Used for the field-initializer
/// early error.
fn contains_arguments(e: &Expression<'_>) -> bool {
    // A nested ordinary function/class introduces its own `arguments`, so the
    // search stops there; arrows inherit the enclosing binding and recurse.
    fn walk_expr(e: &Expression<'_>) -> bool {
        match e {
            Expression::Identifier(id) => id.name.as_str() == "arguments",
            Expression::FunctionExpression(_) => false,
            Expression::ClassExpression(ce) => ce
                .heritage
                .as_ref()
                .is_some_and(|h| walk_expr(&h.expression)),
            Expression::ArrowFunctionExpression(a) => match a.get_function_body() {
                Some(body) => body.statements.iter().any(walk_stmt),
                None => a.get_expression().is_some_and(walk_expr),
            },
            Expression::BinaryExpression(b) => walk_expr(&b.left) || walk_expr(&b.right),
            Expression::LogicalExpression(l) => walk_expr(&l.left) || walk_expr(&l.right),
            Expression::UnaryExpression(u) => walk_expr(&u.argument),
            Expression::UpdateExpression(u) => match &u.argument {
                oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                    id.name.as_str() == "arguments"
                }
                _ => false,
            },
            Expression::AssignmentExpression(a) => {
                let lhs = match &a.left {
                    AssignmentTarget::AssignmentTargetIdentifier(id) => {
                        id.name.as_str() == "arguments"
                    }
                    AssignmentTarget::ArrayAssignmentTarget(arr) => arr
                        .elements
                        .iter()
                        .flatten()
                        .any(|el| match el {
                            oxc_ast::ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
                                target_is_arguments(&d.binding) || walk_expr(&d.init)
                            }
                            other => other
                                .as_simple_assignment_target()
                                .is_some_and(simple_target_is_arguments),
                        }),
                    _ => false,
                };
                lhs || walk_expr(&a.right)
            }
            Expression::ConditionalExpression(c) => {
                walk_expr(&c.test) || walk_expr(&c.consequent) || walk_expr(&c.alternate)
            }
            Expression::SequenceExpression(s) => s.expressions.iter().any(walk_expr),
            Expression::CallExpression(c) => {
                walk_expr(&c.callee)
                    || c.arguments.iter().any(|a| match a {
                        oxc_ast::ast::Argument::SpreadElement(s) => walk_expr(&s.argument),
                        other => other.as_expression().is_some_and(walk_expr),
                    })
            }
            Expression::NewExpression(n) => {
                walk_expr(&n.callee)
                    || n.arguments.iter().any(|a| match a {
                        oxc_ast::ast::Argument::SpreadElement(s) => walk_expr(&s.argument),
                        other => other.as_expression().is_some_and(walk_expr),
                    })
            }
            Expression::TemplateLiteral(t) => t.expressions.iter().any(walk_expr),
            Expression::TaggedTemplateExpression(t) => {
                walk_expr(&t.tag) || t.quasi.expressions.iter().any(walk_expr)
            }
            Expression::ComputedMemberExpression(c) => {
                walk_expr(&c.object) || walk_expr(&c.expression)
            }
            Expression::StaticMemberExpression(s) => walk_expr(&s.object),
            Expression::PrivateFieldExpression(p) => walk_expr(&p.object),
            Expression::ParenthesizedExpression(p) => walk_expr(&p.expression),
            Expression::ObjectExpression(o) => o.properties.iter().any(|p| match p {
                oxc_ast::ast::ObjectPropertyKind::ObjectProperty(op) => walk_expr(&op.value),
                oxc_ast::ast::ObjectPropertyKind::SpreadProperty(s) => walk_expr(&s.argument),
            }),
            Expression::ArrayExpression(arr) => arr.elements.iter().any(|el| match el {
                ArrayExpressionElement::SpreadElement(s) => walk_expr(&s.argument),
                ArrayExpressionElement::Elision(_) => false,
                other => other.as_expression().is_some_and(walk_expr),
            }),
            Expression::YieldExpression(y) => y.argument.as_ref().is_some_and(walk_expr),
            Expression::AwaitExpression(a) => walk_expr(&a.argument),
            Expression::PrivateInExpression(p) => walk_expr(&p.right),
            _ => false,
        }
    }
    fn simple_target_is_arguments(t: &SimpleAssignmentTarget<'_>) -> bool {
        matches!(
            t,
            SimpleAssignmentTarget::AssignmentTargetIdentifier(id) if id.name.as_str() == "arguments"
        )
    }
    fn target_is_arguments(t: &oxc_ast::ast::AssignmentTarget<'_>) -> bool {
        t.as_simple_assignment_target()
            .is_some_and(simple_target_is_arguments)
    }
    fn walk_stmt(s: &Statement<'_>) -> bool {
        match s {
            Statement::ExpressionStatement(e) => walk_expr(&e.expression),
            Statement::ReturnStatement(r) => r.argument.as_ref().is_some_and(walk_expr),
            Statement::IfStatement(i) => walk_expr(&i.test) || walk_stmt(&i.consequent),
            Statement::BlockStatement(b) => b.body.iter().any(walk_stmt),
            Statement::VariableDeclaration(v) => v
                .declarations
                .iter()
                .any(|d| d.init.as_ref().is_some_and(walk_expr)),
            Statement::ThrowStatement(t) => walk_expr(&t.argument),
            Statement::FunctionDeclaration(_) => false,
            _ => false,
        }
    }
    walk_expr(e)
}

/// `IsSimpleParameterList` (ES §14.1.13): every formal is a plain
/// `BindingIdentifier` with no initializer and there is no rest parameter.
fn is_simple_parameter_list(params: &FormalParameters<'_>) -> bool {
    params.rest.is_none()
        && params.items.iter().all(|p| {
            p.initializer.is_none() && matches!(p.pattern, BindingPattern::BindingIdentifier(_))
        })
}

struct Collector<'s> {
    scoping: &'s Scoping,
    plans: Plans,
    strict_stack: Vec<bool>,
    /// `(symbol, referencing unit)` pairs; joined against homes in `finalize`.
    ref_sites: Vec<(SymbolId, usize)>,
    unit_stack: Vec<usize>,
    /// Per-unit `super` legality: `true` inside a class constructor, method,
    /// or field initializer. A nested ordinary function pushes `false`; a
    /// nested arrow inherits the enclosing value (ES §15.7.1). `super` outside
    /// any such context is an early SyntaxError.
    super_allowed_stack: Vec<bool>,
    /// Per-unit `super()` *call* legality: `true` only directly inside a
    /// derived constructor. `super()` in a method, a base-class constructor,
    /// or a field initializer is an early SyntaxError (ES §15.7.1).
    super_call_allowed_stack: Vec<bool>,
    /// Lexically-nested class private-name environments, innermost last. A
    /// `#x` reference is valid iff some entry contains `#x`; private names are
    /// only visible inside the class body that declares them (ES §15.7.1
    /// `AllPrivateNamesValid`).
    private_name_scopes: Vec<Vec<String>>,
    /// `true` while walking a formal-parameter list (including defaults and
    /// nested arrow parameters). A `YieldExpression` or `AwaitExpression`
    /// appearing directly in a parameter list is always an early SyntaxError,
    /// because parameters evaluate before the function is resumable
    /// (ES §14.1.2, §14.2.1, §14.4).
    walking_params: bool,
    /// First early-error found during the walk; aborts collection after the
    /// current subtree so one bad construct yields one diagnostic.
    early_error: Option<CompileError>,
}

impl<'s> Collector<'s> {
    // `cur_unit` runs with a non-empty `unit_stack` (pushed at every unit
    // entry, popped at exit); audited invariant.
    #[allow(clippy::expect_used)]
    fn cur_unit(&self) -> usize {
        *self.unit_stack.last().expect("unit stack underflow")
    }

    fn declare(&mut self, sym: SymbolId) {
        let unit = self.cur_unit();
        self.plans.home_of.insert(sym, unit);
        self.plans.units[unit].decl_order.push(sym);
    }

    /// Validates a `#name` reference against the active lexical class scopes.
    /// `class Outer { #x; m() { this.#x } }` resolves; a reference to a name
    /// declared only by a nested class, or by no class at all, is an early
    /// error (`AllPrivateNamesValid`).
    fn check_private_name_ref(&mut self, name: &str, span: oxc_span::Span) {
        if self.private_name_scopes.is_empty() {
            // Not inside any class body: `obj.#x` is always invalid.
            self.record_early_error(
                span,
                format!("SyntaxError: private name '{name}' is not declared in an enclosing class"),
            );
            return;
        }
        let declared = self
            .private_name_scopes
            .iter()
            .any(|scope| scope.iter().any(|n| n == name));
        if !declared {
            self.record_early_error(
                span,
                format!("SyntaxError: private name '{name}' is not declared in an enclosing class"),
            );
        }
    }

    /// Records the first early error encountered while walking; later walks
    /// still run (cheaply) but the whole collect aborts on return.
    fn record_early_error(&mut self, span: oxc_span::Span, message: impl Into<String>) {
        if self.early_error.is_none() {
            self.early_error = Some(CompileError {
                message: message.into(),
                span: Some((span.start, span.end)),
            });
        }
    }

    /// ES §14.1.2 / §14.2.1: a function with a non-simple parameter list may
    /// not carry a `"use strict"` directive — the directive is an early error
    /// then, because the parameters and the strict body cannot both bind.
    /// `strict_span` is the span to report (the function/arrow body).
    fn check_use_strict_with_non_simple_params(
        &mut self,
        params: &FormalParameters<'_>,
        has_own_use_strict: bool,
        span: oxc_span::Span,
    ) {
        if has_own_use_strict && !is_simple_parameter_list(params) {
            self.record_early_error(
                span,
                "SyntaxError: 'use strict' directive not allowed in a function with a \
                 non-simple parameter list",
            );
        }
    }

    fn note_ref(&mut self, sym: SymbolId) {
        self.ref_sites.push((sym, self.cur_unit()));
    }

    fn ref_symbol(&self, rid: Option<oxc_semantic::ReferenceId>) -> Option<SymbolId> {
        rid.and_then(|rid| self.scoping.get_reference(rid).symbol_id())
    }

    /// Marks the nearest non-arrow function unit as needing an `arguments`
    /// object when `name` is an unbound `arguments` reference. Arrows inherit
    /// their outer function's `arguments`, mirroring the `this` handling
    /// above; the main unit has none, so top-level references stay global.
    fn mark_arguments(&mut self, name: &str, rid: Option<oxc_semantic::ReferenceId>) {
        if name != "arguments" || self.ref_symbol(rid).is_some() {
            return;
        }
        let mut owner = self.cur_unit();
        while self.plans.units[owner].is_arrow {
            owner = self.plans.units[owner]
                .parent
                .expect("arrow below main unit");
        }
        if owner != 0 {
            self.plans.units[owner].needs_arguments = true;
        }
    }

    /// Registers an object-literal method (`{ m() {…} }`, getters/setters
    /// included): a non-arrow function whose HomeObject makes `super.x` legal.
    fn enter_object_method(&mut self, p: &oxc_ast::ast::ObjectProperty<'_>) {
        if let Expression::FunctionExpression(f) = &p.value {
            let hint = static_key_or_default(&p.key);
            self.fn_unit_ext(f, false, &format!("<objmethod>:{hint}"), true);
        }
    }

    /// Registers a non-arrow function unit and walks params + body inside it.
    ///
    /// `declare_name_here` is `Some(())` for declarations (name binds in the
    /// enclosing unit) and `None` for expressions (named expressions bind
    /// their own name inside themselves).
    fn fn_unit(&mut self, f: &Function<'_>, declare_name_in_enclosing: bool, hint: &str) -> usize {
        self.fn_unit_ext(f, declare_name_in_enclosing, hint, false)
    }

    /// [`Self::fn_unit`] with an explicit HomeObject flag: `allows_super` is
    /// `true` for object-literal methods, whose function has a HomeObject and
    /// may reference `super.x` (ES §13.2.5).
    fn fn_unit_ext(
        &mut self,
        f: &Function<'_>,
        declare_name_in_enclosing: bool,
        hint: &str,
        allows_super: bool,
    ) -> usize {
        if declare_name_in_enclosing
            && let Some(id) = &f.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.declare(sym);
            if self.cur_unit() == 0 && !self.plans.is_module {
                self.plans.global_vars.insert(sym);
            }
        }
        let parent = self.cur_unit();
        let idx = self.plans.units.len();
        let name_hint = match f.id.as_ref() {
            Some(id) => format!("{hint}:{}", id.name),
            None => hint.to_string(),
        };
        let parent_strict = *self.strict_stack.last().unwrap_or(&false);
        let own_strict = f.body.as_deref().is_some_and(|b| {
            b.directives
                .iter()
                .any(|d| d.expression.value == "use strict")
        });
        let is_strict = parent_strict || own_strict;
        self.check_use_strict_with_non_simple_params(&f.params, own_strict, f.span());
        let mut plan = UnitPlan::new(Some(parent), false, name_hint);
        plan.is_strict = is_strict;
        plan.allows_super = allows_super;
        // A named function's `name` own property (installed at closure alloc):
        // without this the name is unobservable (`f.name === undefined`).
        if let Some(id) = f.id.as_ref() {
            plan.function_name = Some(id.name.to_string());
        }
        self.plans.units.push(plan);
        self.plans.fn_index.insert(f.span(), idx);
        self.unit_stack.push(idx);
        self.strict_stack.push(is_strict);
        // An ordinary (non-arrow) function is a new `super` scope: `super` is
        // not allowed inside it even when it is textually nested in a method —
        // unless it is an object-literal method with its own HomeObject. It may
        // never call `super()` (that is constructor-only).
        self.super_allowed_stack.push(allows_super);
        self.super_call_allowed_stack.push(false);

        // Params register first; they occupy the incoming-argument window.
        // `register_formals` walks them under `walking_params` to reject
        // `yield`/`await` in defaults; the body below is a fresh context.
        self.register_formals(idx, &f.params);

        // Named function *expressions* bind their own name inside themselves,
        // after the params (it is an ordinary local of the body).
        if !declare_name_in_enclosing
            && let Some(id) = &f.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.declare(sym);
        }

        // The body is a fresh function context: clear `walking_params` so a
        // nested generator/async function declared inside an *outer* parameter
        // default can use `yield`/`await` in its own body. `register_formals`
        // already restored the caller's value for the outer walk to resume.
        let prev_params = self.walking_params;
        self.walking_params = false;
        if let Some(body) = f.body.as_deref() {
            self.stmt_list(&body.statements);
        }
        self.walking_params = prev_params;
        self.unit_stack.pop();
        self.strict_stack.pop();
        self.super_allowed_stack.pop();
        self.super_call_allowed_stack.pop();
        idx
    }

    fn arrow_unit(&mut self, a: &ArrowFunctionExpression<'_>) -> usize {
        let parent = self.cur_unit();
        let idx = self.plans.units.len();
        let parent_strict = *self.strict_stack.last().unwrap_or(&false);
        let own_strict = match a.get_function_body() {
            Some(body) => body
                .directives
                .iter()
                .any(|d| d.expression.value == "use strict"),
            None => false,
        };
        let is_strict = parent_strict || own_strict;
        self.check_use_strict_with_non_simple_params(&a.params, own_strict, a.span);
        let mut plan = UnitPlan::new(Some(parent), true, format!("<arrow>{}", idx));
        plan.is_strict = is_strict;
        self.plans.units.push(plan);
        self.plans.fn_index.insert(a.span(), idx);
        self.unit_stack.push(idx);
        self.strict_stack.push(is_strict);
        // An arrow inherits the enclosing `super` scope (ES §15.7.1), both for
        // property references and for the `super()` call form.
        let inherited_super = *self.super_allowed_stack.last().unwrap_or(&false);
        let inherited_super_call = *self.super_call_allowed_stack.last().unwrap_or(&false);
        self.plans.units[idx].allows_super = inherited_super;
        self.super_allowed_stack.push(inherited_super);
        self.super_call_allowed_stack.push(inherited_super_call);
        // `register_formals` walks the arrow's parameters as a parameter list
        // (rejecting `yield`/`await` in defaults) and restores the flag. The
        // body is a fresh function context.
        let prev_params = self.walking_params;
        self.register_formals(idx, &a.params);
        self.walking_params = false;
        match a.get_function_body() {
            Some(body) => self.stmt_list(&body.statements),
            None => {
                if let Some(expr) = a.get_expression() {
                    self.expr(expr);
                }
            }
        }
        self.walking_params = prev_params;
        self.unit_stack.pop();
        self.strict_stack.pop();
        self.super_allowed_stack.pop();
        self.super_call_allowed_stack.pop();
        idx
    }

    fn stmt_list(&mut self, stmts: &[Statement<'_>]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Statement<'_>) {
        // Module declarations are also Statement variants via `INHERIT(ModuleDeclaration)`.
        // Handle them first so they do not fall through to the `_` ignore case.
        if let Some(md) = s.as_module_declaration() {
            match md {
                ModuleDeclaration::ImportDeclaration(d) => self.import_decl(d),
                ModuleDeclaration::ExportAllDeclaration(d) => self.export_all_decl(d),
                ModuleDeclaration::ExportDefaultDeclaration(d) => self.export_default_decl(d),
                ModuleDeclaration::ExportDeclaration(d) => self.export_decl(d),
                ModuleDeclaration::ExportNamedDeclaration(d) => self.export_named_decl(d),
                ModuleDeclaration::ExportFromDeclaration(d) => self.export_from_decl(d),
                ModuleDeclaration::TSExportAssignment(_) => {}
                ModuleDeclaration::TSNamespaceExportDeclaration(_) => {}
            }
            return;
        }
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
            Statement::ForInStatement(f) => {
                // `for (const k in obj)`: declare the left binding pattern
                // (the loop variable), then walk the right and body.
                match &f.left {
                    oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) => {
                        let is_const =
                            matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Const);
                        for d in &v.declarations {
                            if is_const && let Some(sym) = binding_symbol(&d.id) {
                                self.plans.const_bindings.insert(sym);
                            }
                            self.binding_pattern(&d.id);
                        }
                    }
                    oxc_ast::ast::ForStatementLeft::AssignmentTargetIdentifier(id) => {
                        if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                            self.note_ref(sym);
                        }
                        self.mark_arguments(id.name.as_str(), id.reference_id.get());
                    }
                    _ => {}
                }
                self.expr(&f.right);
                self.stmt(&f.body);
            }
            Statement::ForOfStatement(f) => {
                // `for (const x of iterable)`: the left binding pattern is a
                // fresh per-iteration binding — declare it in the current
                // unit so `access()` resolves a real register, not r0.
                match &f.left {
                    oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) => {
                        let is_const =
                            matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Const);
                        for d in &v.declarations {
                            if is_const && let Some(sym) = binding_symbol(&d.id) {
                                self.plans.const_bindings.insert(sym);
                            }
                            self.binding_pattern(&d.id);
                        }
                    }
                    oxc_ast::ast::ForStatementLeft::AssignmentTargetIdentifier(id) => {
                        if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                            self.note_ref(sym);
                        }
                        self.mark_arguments(id.name.as_str(), id.reference_id.get());
                    }
                    _ => {}
                }
                self.expr(&f.right);
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
            Statement::ClassDeclaration(c) => {
                self.class_unit(c);
            }
            Statement::VariableDeclaration(v) => self.var_decl(v),
            Statement::SwitchStatement(s) => {
                self.expr(&s.discriminant);
                for case in &s.cases {
                    if let Some(t) = &case.test {
                        self.expr(t);
                    }
                    self.stmt_list(&case.consequent);
                }
            }
            Statement::WithStatement(w) => {
                self.expr(&w.object);
                self.stmt(&w.body);
            }
            // break / continue / empty / debugger carry no references.
            _ => {}
        }
    }

    /// Best-effort key text for a class element's static key (diagnostics only).
    fn static_key_or_default(key: &oxc_ast::ast::PropertyKey<'_>) -> String {
        static_key_or_default(key)
    }

    /// Registers a class's constructor and every method as function units,
    /// walking their bodies for nested functions/arrows/references. The
    /// constructor unit's span is the *class* span so the lowering can find
    /// it; each method unit's span is the method function's span.
    fn class_unit(&mut self, c: &oxc_ast::ast::Class<'_>) {
        // The class name binds in the enclosing unit. All class definitions
        // are strict mode code (ES §15.7.1/§15.8.1), so the name itself must
        // also be a valid strict binding: `class implements {}` is an error.
        if let Some(id) = &c.id
            && let Some(sym) = id.symbol_id.get()
        {
            let name = self.scoping.symbol_name(sym).to_string();
            if is_strict_reserved_word(&name) || name == "eval" || name == "arguments" {
                self.record_early_error(
                    id.span,
                    format!("SyntaxError: '{name}' is not a valid class name"),
                );
            }
            self.declare(sym);
            if self.cur_unit() == 0 && !self.plans.is_module {
                self.plans.global_vars.insert(sym);
            }
        }
        // Static Semantics: Early Errors — a class may declare at most one
        // `constructor`, and every private name it uses must be bound by the
        // class body (§15.7.1 `PrototypePropertyNameList`,
        // `AllPrivateNamesValid`).
        let mut ctor_count = 0usize;
        for el in &c.body.body {
            if let oxc_ast::ast::ClassElement::MethodDefinition(m) = el
                && m.kind == oxc_ast::ast::MethodDefinitionKind::Constructor
                && !m.r#static
            {
                ctor_count += 1;
            }
        }
        if ctor_count > 1 {
            self.record_early_error(c.span, "SyntaxError: a class may only have one constructor");
        }
        // Duplicate private names: `#x` may be declared once (get/set accessor
        // pairs share a name; a further declaration is an error).
        let mut seen_private: Vec<(String, bool, bool)> = Vec::new();
        for el in &c.body.body {
            let (name, is_accessor, is_get) = match el {
                oxc_ast::ast::ClassElement::MethodDefinition(m) => {
                    let Some(n) = private_name_text(&m.key) else {
                        continue;
                    };
                    let is_get = m.kind == oxc_ast::ast::MethodDefinitionKind::Get;
                    let is_set = m.kind == oxc_ast::ast::MethodDefinitionKind::Set;
                    (n, is_get || is_set, is_get)
                }
                oxc_ast::ast::ClassElement::PropertyDefinition(p) => {
                    let Some(n) = private_name_text(&p.key) else {
                        continue;
                    };
                    (n, false, false)
                }
                _ => continue,
            };
            if let Some(existing) = seen_private.iter_mut().find(|(n, _, _)| *n == name) {
                // A get/set pair is the only legal repeat: one get and one set.
                if existing.1 && is_accessor && existing.2 != is_get {
                    existing.1 = false; // pair complete — a third is an error
                    continue;
                }
                self.record_early_error(
                    el.span(),
                    format!("SyntaxError: duplicate private name '{name}'"),
                );
                continue;
            }
            seen_private.push((name, is_accessor, is_get));
        }
        // Walk the heritage expression for references. The heritage evaluates
        // in the *enclosing* context, before this class's private environment
        // exists, so the new scope is pushed only after it.
        if let Some(h) = &c.heritage {
            self.expr(&h.expression);
        }
        // Every `#name` referenced anywhere in the class body must be declared
        // by this class or an enclosing one.
        self.private_name_scopes.push(declared_private_names(c));
        let scope_guard = self.private_name_scopes.len();
        // The constructor unit. Class code is always strict: force `is_strict`
        // regardless of the surrounding unit (the class body is its own strict
        // function context).
        let parent = self.cur_unit();
        let idx = self.plans.units.len();
        let mut plan = UnitPlan::new(
            Some(parent),
            false,
            format!(
                "<class>{}",
                c.id.as_ref().map(|i| i.name.as_str()).unwrap_or("")
            ),
        );
        plan.is_strict = true;
        plan.allows_super = true;
        self.plans.units.push(plan);
        self.plans.fn_index.insert(c.span, idx);
        // The constructor's params/body come from the explicit `constructor`
        // element; register them in the constructor unit.
        self.unit_stack.push(idx);
        self.strict_stack.push(true);
        self.super_allowed_stack.push(true);
        // `super()` may only be called in a constructor of a derived class.
        self.super_call_allowed_stack.push(c.heritage.is_some());
        let ctor_el = c.body.body.iter().find_map(|el| match el {
            oxc_ast::ast::ClassElement::MethodDefinition(m)
                if m.kind == oxc_ast::ast::MethodDefinitionKind::Constructor =>
            {
                Some(m)
            }
            _ => None,
        });
        if let Some(m) = ctor_el {
            // The explicit constructor is a Function; register its params and
            // walk its body, and note references inside it. A `"use strict"`
            // directive is redundant here but still subject to the
            // use-strict-with-non-simple-params early error.
            let ctor_strict = m.value.body.as_deref().is_some_and(|b| {
                b.directives
                    .iter()
                    .any(|d| d.expression.value == "use strict")
            });
            self.check_use_strict_with_non_simple_params(
                &m.value.params,
                ctor_strict,
                m.value.span,
            );
            let prev_params = self.walking_params;
            self.register_formals(idx, &m.value.params);
            self.walking_params = false;
            if let Some(body) = m.value.body.as_deref() {
                self.stmt_list(&body.statements);
            }
            self.walking_params = prev_params;
        }
        if let Some(id) = &c.id {
            self.plans.units[idx].function_name = Some(id.name.to_string());
        }
        self.unit_stack.pop();
        self.super_allowed_stack.pop();
        self.super_call_allowed_stack.pop();
        // Computed class-element names (`class { [expr]() {} }`,
        // `[expr] = v`) evaluate in the class's strict, private-aware context.
        for el in &c.body.body {
            match el {
                oxc_ast::ast::ClassElement::MethodDefinition(m) if m.computed => {
                    if let Some(kx) = m.key.as_expression() {
                        self.unit_stack.push(idx);
                        self.expr(kx);
                        self.unit_stack.pop();
                    }
                }
                oxc_ast::ast::ClassElement::PropertyDefinition(p) if p.computed => {
                    if let Some(kx) = p.key.as_expression() {
                        self.unit_stack.push(idx);
                        self.expr(kx);
                        self.unit_stack.pop();
                    }
                }
                _ => {}
            }
        }
        // Walk field initializers for `this`/`super`/captures (e.g. `#x = () => this.#y`).
        // The initializer runs in the class's strict context; `super` property
        // access is legal there but a `super()` *call* is not (checked in the
        // emitter, which has the parent-heritage information).
        for el in &c.body.body {
            if let oxc_ast::ast::ClassElement::PropertyDefinition(p) = el
                && let Some(init) = &p.value
            {
                // Instance field initializers conceptually run in constructor;
                // attribute any `this` inside them to the constructor unit so
                // `this` slot is planned. We push constructor idx as current.
                // A field initializer may read `super.x` but may never call
                // `super()` (ES §15.7.1: `Initializer Contains SuperCall`) nor
                // reference `arguments` (`Initializer ContainsArguments`).
                if contains_arguments(init) {
                    self.record_early_error(
                        init.span(),
                        "SyntaxError: 'arguments' is not allowed in a class field initializer",
                    );
                }
                self.unit_stack.push(idx);
                self.super_allowed_stack.push(true);
                self.super_call_allowed_stack.push(false);
                self.expr(init);
                self.super_call_allowed_stack.pop();
                self.super_allowed_stack.pop();
                self.unit_stack.pop();
            }
        }
        // Each non-constructor method is its own unit.
        for el in &c.body.body {
            if let oxc_ast::ast::ClassElement::MethodDefinition(m) = el
                && m.kind != oxc_ast::ast::MethodDefinitionKind::Constructor
            {
                let midx = self.plans.units.len();
                let mut mplan = UnitPlan::new(
                    Some(parent),
                    false,
                    format!("<method>{}", Self::static_key_or_default(&m.key)),
                );
                let prefix = match m.kind {
                    oxc_ast::ast::MethodDefinitionKind::Get => "get ",
                    oxc_ast::ast::MethodDefinitionKind::Set => "set ",
                    _ => "",
                };
                mplan.function_name =
                    crate::expr::static_key_text(&m.key).map(|k| format!("{prefix}{k}"));
                let parent_strict = *self.strict_stack.last().unwrap_or(&false);
                let own_strict = m.value.body.as_deref().is_some_and(|b| {
                    b.directives
                        .iter()
                        .any(|d| d.expression.value == "use strict")
                });
                // Class methods are always strict code (ES §10.2.1), so the
                // surrounding `parent_strict`/`own_strict` values are not
                // consulted; a redundant `"use strict"` directive still
                // triggers the non-simple-parameter early error below.
                let _ = (parent_strict, own_strict);
                mplan.is_strict = true;
                mplan.static_method = m.r#static;
                mplan.allows_super = true;
                self.check_use_strict_with_non_simple_params(
                    &m.value.params,
                    own_strict,
                    m.value.span,
                );
                self.plans.units.push(mplan);
                self.plans.fn_index.insert(m.value.span, midx);
                self.unit_stack.push(midx);
                self.strict_stack.push(true);
                self.super_allowed_stack.push(true);
                // A method (including a getter/setter) may use `super.x` but
                // may not call `super()` — that is constructor-only.
                self.super_call_allowed_stack.push(false);
                let prev_params = self.walking_params;
                self.register_formals(midx, &m.value.params);
                self.walking_params = false;
                if let Some(body) = m.value.body.as_deref() {
                    self.stmt_list(&body.statements);
                }
                self.walking_params = prev_params;
                self.unit_stack.pop();
                self.strict_stack.pop();
                self.super_allowed_stack.pop();
                self.super_call_allowed_stack.pop();
            }
        }
        // Pop the class-body strict and private-name frames pushed above.
        self.strict_stack.pop();
        debug_assert_eq!(self.private_name_scopes.len(), scope_guard);
        self.private_name_scopes.pop();
    }

    fn import_decl(&mut self, d: &oxc_ast::ast::ImportDeclaration<'_>) {
        let specifier = d.source.value.to_string();
        let span = Some((d.span.start, d.span.end));
        if let Some(specs) = &d.specifiers {
            if specs.is_empty() {
                // `import {} from "x"` – no bindings, but still a module dependency.
                self.plans.imports.push(ImportEntry {
                    specifier: specifier.clone(),
                    imported: String::new(),
                    local: None,
                    span,
                });
            }
            for s in specs {
                match s {
                    oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(sp) => {
                        let imported = module_export_name_to_string(&sp.imported);
                        let local_sym = sp.local.symbol_id.get();
                        if let Some(sym) = local_sym {
                            self.declare(sym);
                        }
                        self.plans.imports.push(ImportEntry {
                            specifier: specifier.clone(),
                            imported,
                            local: local_sym,
                            span: Some((sp.span.start, sp.span.end)),
                        });
                    }
                    oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(sp) => {
                        let local_sym = sp.local.symbol_id.get();
                        if let Some(sym) = local_sym {
                            self.declare(sym);
                        }
                        self.plans.imports.push(ImportEntry {
                            specifier: specifier.clone(),
                            imported: "default".to_string(),
                            local: local_sym,
                            span: Some((sp.span.start, sp.span.end)),
                        });
                    }
                    oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(sp) => {
                        let local_sym = sp.local.symbol_id.get();
                        if let Some(sym) = local_sym {
                            self.declare(sym);
                        }
                        self.plans.imports.push(ImportEntry {
                            specifier: specifier.clone(),
                            imported: "*".to_string(),
                            local: local_sym,
                            span: Some((sp.span.start, sp.span.end)),
                        });
                    }
                }
            }
        } else {
            // `import "./side.js"` – side-effect only.
            self.plans.imports.push(ImportEntry {
                specifier,
                imported: String::new(),
                local: None,
                span,
            });
        }
    }

    fn export_decl(&mut self, d: &oxc_ast::ast::ExportDeclaration<'_>) {
        let span = Some((d.span.start, d.span.end));
        match &d.declaration {
            oxc_ast::ast::Declaration::VariableDeclaration(v) => {
                // Record each binding as an export.
                for decl in &v.declarations {
                    if let Some(sym) = binding_symbol(&decl.id) {
                        let exported = ident_name_of_binding(&decl.id)
                            .unwrap_or_else(|| self.scoping.symbol_name(sym).to_string());
                        self.plans.exports.push(ExportEntry {
                            specifier: None,
                            local: Some(sym),
                            exported,
                            span,
                        });
                    }
                }
                self.var_decl(v);
            }
            oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
                if let Some(id) = &f.id
                    && let Some(sym) = id.symbol_id.get()
                {
                    let exported = id.name.to_string();
                    self.plans.exports.push(ExportEntry {
                        specifier: None,
                        local: Some(sym),
                        exported,
                        span,
                    });
                }
                self.fn_unit(f, true, "<fn>");
            }
            oxc_ast::ast::Declaration::ClassDeclaration(c) => {
                if let Some(id) = &c.id
                    && let Some(sym) = id.symbol_id.get()
                {
                    let exported = id.name.to_string();
                    self.plans.exports.push(ExportEntry {
                        specifier: None,
                        local: Some(sym),
                        exported,
                        span,
                    });
                }
            }
            _ => {}
        }
    }

    fn export_named_decl(&mut self, d: &oxc_ast::ast::ExportNamedDeclaration<'_>) {
        let span = Some((d.span.start, d.span.end));
        for sp in &d.specifiers {
            let local = module_export_name_to_symbol(&sp.local, self.scoping);
            let exported = module_export_name_to_string(&sp.exported);
            self.plans.exports.push(ExportEntry {
                specifier: None,
                local,
                exported,
                span,
            });
            if let Some(sym) = local {
                self.note_ref(sym);
            }
        }
    }

    fn export_from_decl(&mut self, d: &oxc_ast::ast::ExportFromDeclaration<'_>) {
        let specifier = d.source.value.to_string();
        let span = Some((d.span.start, d.span.end));
        for sp in &d.specifiers {
            let exported = module_export_name_to_string(&sp.exported);
            let local = module_export_name_to_symbol(&sp.local, self.scoping);
            self.plans.exports.push(ExportEntry {
                specifier: Some(specifier.clone()),
                local,
                exported,
                span,
            });
        }
    }

    fn export_default_decl(&mut self, d: &oxc_ast::ast::ExportDefaultDeclaration<'_>) {
        let span = Some((d.span.start, d.span.end));
        let local_sym: Option<SymbolId> = match &d.declaration {
            oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                f.id.as_ref().and_then(|id| id.symbol_id.get())
            }
            oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                c.id.as_ref().and_then(|id| id.symbol_id.get())
            }
            _ => None,
        };
        if let Some(sym) = local_sym {
            // The binding is declared at module top level if it has a name.
            // Ensure it is counted as a declaration for layout; `fn_unit`
            // will declare it when we walk the inner function.
            if !self.plans.home_of.contains_key(&sym) {
                self.declare(sym);
            }
        }
        self.plans.exports.push(ExportEntry {
            specifier: None,
            local: local_sym,
            exported: "default".to_string(),
            span,
        });
        // Walk the inner declaration for capture analysis where applicable.
        match &d.declaration {
            oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                // `export default function foo(){}` – function name binds
                // inside itself for declaration forms, but for default export
                // the name is not visible outside; treat as expression.
                self.fn_unit(f, false, "<default fn>");
            }
            oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                let _ = c;
            }
            oxc_ast::ast::ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {}
            _ => {
                // Expression form: `export default 1` – no extra decls.
                if let Some(expr) = d.declaration.as_expression() {
                    self.expr(expr);
                }
            }
        }
    }

    fn export_all_decl(&mut self, d: &oxc_ast::ast::ExportAllDeclaration<'_>) {
        let specifier = d.source.value.to_string();
        let span = Some((d.span.start, d.span.end));
        let exported = d
            .exported
            .as_ref()
            .map(|n| module_export_name_to_string(n))
            .unwrap_or_else(|| "*".to_string());
        self.plans.exports.push(ExportEntry {
            specifier: Some(specifier),
            local: None,
            exported,
            span,
        });
    }

    fn var_decl(&mut self, v: &VariableDeclaration<'_>) {
        let is_const = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Const);
        let is_var_at_top = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Var)
            && self.cur_unit() == 0
            && !self.plans.is_module;
        for d in &v.declarations {
            if is_const {
                if let Some(sym) = binding_symbol(&d.id) {
                    self.plans.const_bindings.insert(sym);
                } else {
                    self.collect_const_bindings(&d.id);
                }
            }
            if is_var_at_top {
                self.collect_global_bindings(&d.id);
            }
            self.binding_pattern(&d.id);
            if let Some(init) = &d.init {
                self.expr(init);
            }
        }
    }

    fn collect_global_bindings(&mut self, pat: &BindingPattern<'_>) {
        match pat {
            BindingPattern::BindingIdentifier(id) => {
                if let Some(sym) = id.symbol_id.get() {
                    self.plans.global_vars.insert(sym);
                }
            }
            BindingPattern::ObjectPattern(o) => {
                for prop in &o.properties {
                    self.collect_global_bindings(&prop.value);
                }
                if let Some(rest) = &o.rest {
                    self.collect_global_bindings(&rest.argument);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    self.collect_global_bindings(el);
                }
                if let Some(rest) = &a.rest {
                    self.collect_global_bindings(&rest.argument);
                }
            }
            BindingPattern::AssignmentPattern(ap) => {
                self.collect_global_bindings(&ap.left);
            }
        }
    }

    fn collect_const_bindings(&mut self, pat: &BindingPattern<'_>) {
        match pat {
            BindingPattern::BindingIdentifier(id) => {
                if let Some(sym) = id.symbol_id.get() {
                    self.plans.const_bindings.insert(sym);
                }
            }
            BindingPattern::ObjectPattern(o) => {
                for prop in &o.properties {
                    self.collect_const_bindings(&prop.value);
                }
                if let Some(rest) = &o.rest {
                    self.collect_const_bindings(&rest.argument);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    self.collect_const_bindings(el);
                }
                if let Some(rest) = &a.rest {
                    self.collect_const_bindings(&rest.argument);
                }
            }
            BindingPattern::AssignmentPattern(ap) => {
                self.collect_const_bindings(&ap.left);
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

    /// Registers one function's formal parameters: walks every binding leaf
    /// (so captures and default-RHS references are collected) and records the
    /// top-level arity separately from the leaf count.
    fn register_formals(&mut self, idx: usize, params: &FormalParameters<'_>) {
        // Every caller is evaluating a parameter list; defaults in particular
        // must reject `yield`/`await` (ES §14.1.2/§14.2.1/§14.4). Nested
        // functions inside a default save and restore this flag themselves.
        let prev_params = self.walking_params;
        self.walking_params = true;
        for p in &params.items {
            self.binding_pattern(&p.pattern);
            // oxc stores a top-level default (`a = 1`, `[a] = []`) on the
            // `FormalParameter`, not inside the binding pattern, so walk the
            // initializer to collect its references/captures too.
            if let Some(init) = &p.initializer {
                self.expr(init);
            }
        }
        if let Some(rest) = &params.rest {
            self.binding_pattern(&rest.rest.argument);
        }
        let formal_idents = params
            .items
            .iter()
            .map(|p| binding_symbol(&p.pattern))
            .collect();
        let rest_ident = params
            .rest
            .as_ref()
            .and_then(|r| binding_symbol(&r.rest.argument));
        self.plans.units[idx].arity = params.items.len();
        self.plans.units[idx].formal_idents = formal_idents;
        self.plans.units[idx].rest_ident = rest_ident;
        self.plans.units[idx].has_rest = params.rest.is_some();
        self.plans.units[idx].expected_args = expected_args(&params.items);
        self.walking_params = prev_params;
    }

    // Arrow units always have a parent (the compiler guarantees a non-arrow
    // main unit exists); audited invariant.
    #[allow(clippy::expect_used)]
    fn expr(&mut self, e: &Expression<'_>) {
        match e {
            Expression::Identifier(id) => {
                // ES §12.1.1/§12.6.2: strict-mode code may not use a
                // FutureReservedWord as an `IdentifierReference`, so a bare
                // `package`/`yield`/`let` reference is an early SyntaxError.
                // Property keys and member names are `IdentifierName`, not
                // references, and never reach this arm.
                if *self.strict_stack.last().unwrap_or(&false)
                    && is_strict_reserved_word(id.name.as_str())
                {
                    self.record_early_error(
                        id.span,
                        format!(
                            "SyntaxError: '{name}' is a reserved word in strict mode",
                            name = id.name
                        ),
                    );
                    return;
                }
                if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                    self.note_ref(sym);
                }
                self.mark_arguments(id.name.as_str(), id.reference_id.get());
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
            Expression::Super(sup) => {
                // ES §15.7.1: `SuperProperty`/`SuperCall` are only legal in a
                // class method (or an arrow lexically inside one). Anywhere
                // else — a plain function, the top level, a `extends` clause —
                // is an early SyntaxError.
                if !*self.super_allowed_stack.last().unwrap_or(&false) {
                    self.record_early_error(sup.span, "`super` outside a class method");
                    return;
                }
                // `super` resolves through the class env captured by the
                // nearest enclosing method/constructor unit.
                let mut owner = self.cur_unit();
                while self.plans.units[owner].is_arrow {
                    owner = self.plans.units[owner]
                        .parent
                        .expect("arrow below main unit");
                }
                self.plans.units[owner].uses_super = true;
            }
            Expression::FunctionExpression(f) => {
                self.fn_unit(f, false, "<fnexpr>");
            }
            Expression::ArrowFunctionExpression(a) => {
                self.arrow_unit(a);
            }
            Expression::ClassExpression(c) => {
                self.class_unit(c);
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
                self.walk_assignment_target(&a.left);
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
                // ES §15.7.1: `SuperCall` is only legal in a derived-class
                // constructor. Report before walking the callee so the
                // specialized message wins over the generic one.
                if c.callee.is_super() && !*self.super_call_allowed_stack.last().unwrap_or(&false) {
                    self.record_early_error(
                        c.span,
                        "`super()` is only allowed in a derived class constructor",
                    );
                    return;
                }
                self.expr(&c.callee);
                for arg in &c.arguments {
                    if let Some(x) = arg.as_expression() {
                        self.expr(x);
                    } else if let oxc_ast::ast::Argument::SpreadElement(s) = arg {
                        self.expr(&s.argument);
                    }
                }
            }
            Expression::NewExpression(n) => {
                self.expr(&n.callee);
                for arg in &n.arguments {
                    if let Some(x) = arg.as_expression() {
                        self.expr(x);
                    } else if let oxc_ast::ast::Argument::SpreadElement(s) = arg {
                        self.expr(&s.argument);
                    }
                }
            }
            Expression::ChainExpression(_ch) => {
                // chain contains nested functions only via arguments; ignore detailed walk
            }
            Expression::PrivateInExpression(p) => {
                self.check_private_name_ref(&format!("#{}", p.left.name), p.span);
                self.expr(&p.right);
            }
            Expression::TemplateLiteral(t) => {
                for q in &t.quasis {
                    let _ = q;
                }
                for e in &t.expressions {
                    self.expr(e);
                }
            }
            Expression::TaggedTemplateExpression(t) => {
                self.expr(&t.tag);
                for e in &t.quasi.expressions {
                    self.expr(e);
                }
            }
            Expression::YieldExpression(y) => {
                // A `YieldExpression` in a formal-parameter list is always an
                // early error: parameters evaluate before the generator is
                // resumable (ES §14.4). In a non-generator context oxc parses
                // `yield` as an `IdentifierReference`, so this arm only fires
                // for genuine yield expressions.
                if self.walking_params {
                    self.record_early_error(
                        y.span,
                        "SyntaxError: yield expression not allowed in a parameter list",
                    );
                }
                if let Some(arg) = &y.argument {
                    self.expr(arg);
                }
            }
            Expression::AwaitExpression(a) => {
                // Likewise `await` in a formal-parameter list (ES §14.2.1).
                if self.walking_params {
                    self.record_early_error(
                        a.span,
                        "SyntaxError: await expression not allowed in a parameter list",
                    );
                }
                self.expr(&a.argument);
            }
            Expression::ComputedMemberExpression(c) => {
                self.expr(&c.object);
                self.expr(&c.expression);
            }
            Expression::StaticMemberExpression(s) => self.expr(&s.object),
            Expression::PrivateFieldExpression(p) => {
                self.check_private_name_ref(&format!("#{}", p.field.name), p.span);
                self.expr(&p.object);
            }
            Expression::ObjectExpression(o) => {
                // Annex B.3.1: two `PropertyDefinition : PropertyName :
                // AssignmentExpression` entries for `__proto__` are an early
                // error. Shorthand (`{__proto__}`) and methods/accessors do not
                // count toward the pair.
                let mut proto_colon_defs: Vec<oxc_span::Span> = Vec::new();
                for prop_kind in &o.properties {
                    if let Some(p) = prop_kind.as_property()
                        && !p.computed
                        && !p.shorthand
                        && p.kind == oxc_ast::ast::PropertyKind::Init
                        && crate::expr::static_key_text(&p.key).as_deref() == Some("__proto__")
                    {
                        proto_colon_defs.push(p.key.span());
                    }
                }
                if proto_colon_defs.len() > 1 {
                    let span = proto_colon_defs[1];
                    self.record_early_error(
                        span,
                        "SyntaxError: duplicate '__proto__' property in object literal",
                    );
                }
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
                        // An object-literal method (`{ m() { super.x } }`,
                        // getters/setters included) has a HomeObject, so it
                        // may use `super.x` (ES §13.2.5); it may still not
                        // call `super()`. Shorthand `{m}` and `{m: fn}` carry
                        // no HomeObject and are walked normally.
                        let is_method_like = p.method
                            || p.kind == oxc_ast::ast::PropertyKind::Get
                            || p.kind == oxc_ast::ast::PropertyKind::Set;
                        if is_method_like
                            && matches!(&p.value, Expression::FunctionExpression(f) if f.r#type == oxc_ast::ast::FunctionType::FunctionExpression)
                        {
                            self.enter_object_method(p);
                        } else {
                            self.expr(&p.value);
                        }
                    }
                }
            }
            Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    match el {
                        ArrayExpressionElement::SpreadElement(s) => self.expr(&s.argument),
                        ArrayExpressionElement::Elision(_) => {}
                        _ => {
                            if let Some(x) = el.as_expression() {
                                self.expr(x);
                            }
                        }
                    }
                }
            }
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            Expression::ImportExpression(i) => self.expr(&i.source),
            _ => {}
        }
    }

    fn walk_assignment_target(&mut self, target: &oxc_ast::ast::AssignmentTarget<'_>) {
        if let Some(simple) = target.as_simple_assignment_target() {
            self.simple_target(simple);
            return;
        }
        // Complex destructuring assignment targets: recurse into nested
        // patterns so references, captures, and nested functions inside
        // defaults are all registered (emission mirrors this recursion in
        // `destructure_assign`, including `lower_default` initializers).
        match target {
            oxc_ast::ast::AssignmentTarget::ArrayAssignmentTarget(arr) => {
                for el in arr.elements.iter().flatten() {
                    if let oxc_ast::ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(
                        d,
                    ) = el
                    {
                        // `[a = init]` / `[[a] = init]`: the default runs at
                        // the use site, so its references belong here.
                        self.walk_assignment_target(&d.binding);
                        self.expr(&d.init);
                    } else if let Some(simple) = el.as_simple_assignment_target() {
                        self.simple_target(simple);
                    } else if let Some(inner) = el.as_assignment_target() {
                        self.walk_assignment_target(inner);
                    }
                }
                if let Some(rest) = &arr.rest {
                    self.walk_assignment_target(&rest.target);
                }
            }
            oxc_ast::ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                for prop in &obj.properties {
                    match prop {
                        oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(id) => {
                            // `{ yield } = {}`: the shorthand is an
                            // IdentifierReference, so a strict-mode
                            // FutureReservedWord is an early error.
                            if *self.strict_stack.last().unwrap_or(&false)
                                && is_strict_reserved_word(id.binding.name.as_str())
                            {
                                self.record_early_error(
                                    id.binding.span,
                                    format!(
                                        "SyntaxError: '{name}' is a reserved word in strict mode",
                                        name = id.binding.name
                                    ),
                                );
                            }
                            if let Some(sym) = self.ref_symbol(id.binding.reference_id.get()) {
                                self.note_ref(sym);
                            }
                            self.mark_arguments(
                                id.binding.name.as_str(),
                                id.binding.reference_id.get(),
                            );
                            if let Some(init) = &id.init {
                                self.expr(init);
                            }
                        }
                        oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            if let oxc_ast::ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(
                                d,
                            ) = &p.binding
                            {
                                // `{y: z = init}`: default runs at the use site.
                                self.walk_assignment_target(&d.binding);
                                self.expr(&d.init);
                            } else if let Some(simple) =
                                p.binding.as_simple_assignment_target()
                            {
                                self.simple_target(simple);
                            } else if let Some(inner) = p.binding.as_assignment_target() {
                                self.walk_assignment_target(inner);
                            }
                        }
                    }
                }
                if let Some(rest) = &obj.rest {
                    self.walk_assignment_target(&rest.target);
                }
            }
            _ => {}
        }
    }

    fn simple_target(&mut self, t: &SimpleAssignmentTarget<'_>) {
        match t {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                // A strict-mode assignment/update target is an
                // `IdentifierReference`, so a FutureReservedWord is an early
                // error there too (`public = 42`).
                if *self.strict_stack.last().unwrap_or(&false)
                    && is_strict_reserved_word(id.name.as_str())
                {
                    self.record_early_error(
                        id.span,
                        format!(
                            "SyntaxError: '{name}' is a reserved word in strict mode",
                            name = id.name
                        ),
                    );
                    return;
                }
                if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                    self.note_ref(sym);
                }
                self.mark_arguments(id.name.as_str(), id.reference_id.get());
            }
            SimpleAssignmentTarget::ComputedMemberExpression(c) => {
                self.expr(&c.object);
                self.expr(&c.expression);
            }
            SimpleAssignmentTarget::StaticMemberExpression(s) => self.expr(&s.object),
            SimpleAssignmentTarget::PrivateFieldExpression(p) => {
                // `this.#x = v`: the write target must also resolve.
                self.check_private_name_ref(&format!("#{}", p.field.name), p.span);
                self.expr(&p.object);
            }
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

fn module_export_name_to_string(name: &ModuleExportName<'_>) -> String {
    match name {
        ModuleExportName::IdentifierName(id) => id.name.to_string(),
        ModuleExportName::IdentifierReference(r) => r.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

fn module_export_name_to_symbol(
    name: &ModuleExportName<'_>,
    scoping: &Scoping,
) -> Option<SymbolId> {
    match name {
        ModuleExportName::IdentifierReference(r) => r
            .reference_id
            .get()
            .and_then(|rid| scoping.get_reference(rid).symbol_id()),
        ModuleExportName::IdentifierName(_) | ModuleExportName::StringLiteral(_) => None,
    }
}

fn binding_symbol(p: &oxc_ast::ast::BindingPattern<'_>) -> Option<SymbolId> {
    match p {
        oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.symbol_id.get(),
        _ => None,
    }
}

/// `ExpectedArgumentCount` (ES 14.1.6): the number of formal parameters up
/// to the rest parameter or the first parameter with an initializer. A
/// top-level default lives on `FormalParameter::initializer` in oxc (the
/// pattern stays bare); a destructuring default nested inside a pattern does
/// not stop the count.
fn expected_args(items: &[oxc_ast::ast::FormalParameter<'_>]) -> usize {
    let mut count = 0;
    for p in items {
        if p.initializer.is_some() {
            break;
        }
        count += 1;
    }
    count
}

fn ident_name_of_binding(p: &oxc_ast::ast::BindingPattern<'_>) -> Option<String> {
    match p {
        oxc_ast::ast::BindingPattern::BindingIdentifier(id) => Some(id.name.to_string()),
        _ => None,
    }
}

/// Assigns concrete storage after the walk: captured symbols escape into
/// their home unit's Environment, everything else gets sequential registers.
///
/// Layout order per unit is deterministic: declaration order (params first)
/// for both env slots and registers; the synthetic `this` slot (when an
/// arrow-descendant reads it) trails all named slots.
///
/// Register and slot counters are u16 (functions above 255 registers/slots
/// escape through the wide operand encodings); overflow past
/// [`MAX_REGS`]/[`MAX_ENV_SLOTS`] is reported as a `CompileError` with
/// message `"too many functions/constants"` instead of panicking so negative
/// tests can observe a compile failure.
fn finalize(plans: &mut Plans) -> Result<(), CompileError> {
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

        let mut slot: u16 = 0;
        let mut reg: u16 = REG_THIS.checked_add(1).ok_or_else(|| CompileError {
            message: "too many functions/constants".into(),
            span: Some((0, 0)),
        })?;
        let (arity, has_rest, rest_ident) = {
            let u = &plans.units[ui];
            (u.arity, u.has_rest, u.rest_ident)
        };
        // 1. Reserve the incoming formal window unconditionally: `r{i+1}`
        //    carries formal `i` from the call ABI. A simple-identifier formal
        //    takes that register (or an env slot when captured — the register
        //    is still consumed because the ABI writes it). A pattern formal
        //    leaves it as scratch for the prologue destructure.
        for i in 0..arity {
            let this_reg = reg;
            reg = reg.checked_add(1).ok_or_else(|| CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            })?;
            let Some(sym) = plans.units[ui].formal_idents.get(i).copied().flatten() else {
                continue;
            };
            if !homes.get(&sym).is_some_and(|h| *h == ui) {
                continue;
            }
            if plans.captured.contains(&sym) {
                plans.units[ui].env_slots.insert(sym, slot);
                plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                slot = slot.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
            } else {
                plans.units[ui].vars.insert(sym, VarLoc::Reg(this_reg));
            }
        }
        // 2. Reserve the rest register at `r{arity+1}` (the ABI tail).
        if has_rest {
            let rest_reg = reg;
            reg = reg.checked_add(1).ok_or_else(|| CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            })?;
            if let Some(sym) = rest_ident
                && homes.get(&sym).is_some_and(|h| *h == ui)
            {
                if plans.captured.contains(&sym) {
                    plans.units[ui].env_slots.insert(sym, slot);
                    plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                    slot = slot.checked_add(1).ok_or_else(|| CompileError {
                        message: "too many functions/constants".into(),
                        span: Some((0, 0)),
                    })?;
                } else {
                    plans.units[ui].vars.insert(sym, VarLoc::Reg(rest_reg));
                }
            }
        }
        // 3. Everything else (pattern leaves, body declarations, named
        //    function-expression self-bindings) above the reserved window.
        let decl_count = plans.units[ui].decl_order.len();
        for i in 0..decl_count {
            let sym = plans.units[ui].decl_order[i];
            // Formals/rest already placed above.
            if plans.units[ui].vars.contains_key(&sym) {
                continue;
            }
            let is_home = homes.get(&sym).is_some_and(|h| *h == ui);
            if !is_home {
                continue;
            }
            // Top-level `var`/`function` bindings alias the global object
            // (scripts only; modules keep their own scope).
            if ui == 0 && !plans.is_module && plans.global_vars.contains(&sym) {
                plans.units[ui].vars.insert(sym, VarLoc::Global);
                continue;
            }
            if plans.captured.contains(&sym) {
                plans.units[ui].env_slots.insert(sym, slot);
                plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                slot = slot.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
            } else {
                plans.units[ui].vars.insert(sym, VarLoc::Reg(reg));
                reg = reg.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
            }
        }
        let unit = &mut plans.units[ui];
        if unit.needs_this && unit.has_env {
            unit.this_slot = Some(slot);
            slot = slot.checked_add(1).ok_or_else(|| CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            })?;
        }
        unit.env_slot_count = slot;
        unit.locals_end = reg;
        // `checked_add` above already rejects overflow; this bounds check
        // rejects programs that *fit* u16 arithmetic but exceed the ISA's
        // usable register/slot ranges (the last value is reserved by the
        // wide-operand escape).
        #[allow(clippy::absurd_extreme_comparisons)]
        if reg >= MAX_REGS || slot >= MAX_ENV_SLOTS {
            return Err(CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            });
        }
    }
    Ok(())
}
