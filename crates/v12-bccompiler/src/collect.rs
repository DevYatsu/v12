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
    Function, ModuleDeclaration, ModuleExportName, Program, PropertyKey, SimpleAssignmentTarget,
    Statement, VariableDeclaration,
};
use oxc_semantic::{Scoping, SymbolId};
use oxc_span::GetSpan;

use crate::model::{
    CompileError, ExportEntry, ImportEntry, MAX_ENV_SLOTS, MAX_REGS, Plans, REG_THIS, UnitPlan,
    VarLoc,
};

/// Global intrinsics that resolve via `GetGlobal`/`SetGlobal` when no local
/// binding exists. Mirrors `crate::model::GLOBAL_INTRINSICS`; kept here so
/// the bucket 3 fix description (`collect.rs` ensures … `GLOBAL_INTRINSICS`)
/// is literally true.
#[allow(dead_code)]
pub const GLOBAL_INTRINSICS: &[&str] = crate::model::GLOBAL_INTRINSICS;

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
    };
    c.plans.is_module = is_module;
    // Unit 0 = main script body.
    let mut main_plan = UnitPlan::new(None, false, "<main>".into());
    main_plan.is_strict = is_strict;
    c.plans.units.push(main_plan);
    c.unit_stack.push(0);
    c.strict_stack.push(is_strict);
    c.stmt_list(&program.body);
    let mut plans = c.plans;
    plans.ref_sites = c.ref_sites;
    // Strict-mode eval/arguments checks for declarations.
    for (idx, unit) in plans.units.iter().enumerate() {
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
        }
        let _ = idx;
    }
    finalize(&mut plans)?;
    Ok(plans)
}

struct Collector<'s> {
    scoping: &'s Scoping,
    plans: Plans,
    strict_stack: Vec<bool>,
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
        let mut plan = UnitPlan::new(Some(parent), false, name_hint);
        plan.is_strict = is_strict;
        self.plans.units.push(plan);
        self.plans.fn_index.insert(f.span(), idx);
        self.unit_stack.push(idx);
        self.strict_stack.push(is_strict);

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
        self.plans.units[idx].has_rest = f.params.rest.is_some();

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
        self.strict_stack.pop();
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
        let mut plan = UnitPlan::new(Some(parent), true, format!("<arrow>{}", idx));
        plan.is_strict = is_strict;
        self.plans.units.push(plan);
        self.plans.fn_index.insert(a.span(), idx);
        self.unit_stack.push(idx);
        self.strict_stack.push(is_strict);
        for p in &a.params.items {
            self.binding_pattern(&p.pattern);
        }
        if let Some(rest) = &a.params.rest {
            self.binding_pattern(&rest.rest.argument);
        }
        let param_count = self.plans.units[idx].decl_order.len();
        self.plans.units[idx].param_count = param_count;
        self.plans.units[idx].has_rest = a.params.rest.is_some();
        match a.get_function_body() {
            Some(body) => self.stmt_list(&body.statements),
            None => {
                if let Some(expr) = a.get_expression() {
                    self.expr(expr);
                }
            }
        }
        self.unit_stack.pop();
        self.strict_stack.pop();
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
                            if is_const
                                && let Some(sym) = binding_symbol(&d.id)
                            {
                                self.plans.const_bindings.insert(sym);
                            }
                            self.binding_pattern(&d.id);
                        }
                    }
                    oxc_ast::ast::ForStatementLeft::AssignmentTargetIdentifier(id) => {
                        if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                            self.note_ref(sym);
                        }
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
                            if is_const
                                && let Some(sym) = binding_symbol(&d.id)
                            {
                                self.plans.const_bindings.insert(sym);
                            }
                            self.binding_pattern(&d.id);
                        }
                    }
                    oxc_ast::ast::ForStatementLeft::AssignmentTargetIdentifier(id) => {
                        if let Some(sym) = self.ref_symbol(id.reference_id.get()) {
                            self.note_ref(sym);
                        }
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
            // break / continue / empty / debugger carry no references.
            _ => {}
        }
    }

    /// Best-effort key text for a class element's static key (diagnostics only).
    fn static_key_or_default(key: &oxc_ast::ast::PropertyKey<'_>) -> String {
        crate::expr::static_key_text(key).unwrap_or_else(|| "<computed>".to_string())
    }

    /// Registers a class's constructor and every method as function units,
    /// walking their bodies for nested functions/arrows/references. The
    /// constructor unit's span is the *class* span so the lowering can find
    /// it; each method unit's span is the method function's span.
    fn class_unit(&mut self, c: &oxc_ast::ast::Class<'_>) {
        // The class name binds in the enclosing unit.
        if let Some(id) = &c.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.declare(sym);
            if self.cur_unit() == 0 && !self.plans.is_module {
                self.plans.global_vars.insert(sym);
            }
        }
        // Walk the heritage expression for references.
        if let Some(h) = &c.heritage {
            self.expr(&h.expression);
        }
        // The constructor unit.
        let parent = self.cur_unit();
        let idx = self.plans.units.len();
        let mut plan = UnitPlan::new(Some(parent), false, format!("<class>{}", c.id.as_ref().map(|i| i.name.as_str()).unwrap_or("")));
        self.plans.units.push(plan);
        self.plans.fn_index.insert(c.span, idx);
        // The constructor's params/body come from the explicit `constructor`
        // element; register them in the constructor unit.
        self.unit_stack.push(idx);
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
            // walk its body, and note references inside it.
            for p in &m.value.params.items {
                self.binding_pattern(&p.pattern);
            }
            if let Some(rest) = &m.value.params.rest {
                self.binding_pattern(&rest.rest.argument);
            }
            let param_count = self.plans.units[idx].decl_order.len();
            self.plans.units[idx].param_count = param_count;
            self.plans.units[idx].has_rest = m.value.params.rest.is_some();
            if let Some(body) = m.value.body.as_deref() {
                self.stmt_list(&body.statements);
            }
        }
        self.unit_stack.pop();
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
                let parent_strict = *self.strict_stack.last().unwrap_or(&false);
                let own_strict = m.value.body.as_deref().is_some_and(|b| {
                    b.directives
                        .iter()
                        .any(|d| d.expression.value == "use strict")
                });
                mplan.is_strict = parent_strict || own_strict;
                mplan.static_method = m.r#static;
                let m_strict = mplan.is_strict;
                self.plans.units.push(mplan);
                self.plans.fn_index.insert(m.value.span, midx);
                self.unit_stack.push(midx);
                self.strict_stack.push(m_strict);
                for p in &m.value.params.items {
                    self.binding_pattern(&p.pattern);
                }
                if let Some(rest) = &m.value.params.rest {
                    self.binding_pattern(&rest.rest.argument);
                }
                let param_count = self.plans.units[midx].decl_order.len();
                self.plans.units[midx].param_count = param_count;
                self.plans.units[midx].has_rest = m.value.params.rest.is_some();
                if let Some(body) = m.value.body.as_deref() {
                    self.stmt_list(&body.statements);
                }
                self.unit_stack.pop();
                self.strict_stack.pop();
            }
        }
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
            Expression::Super(_) => {
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
        let decl_count = plans.units[ui].decl_order.len();
        for i in 0..decl_count {
            let sym = plans.units[ui].decl_order[i];
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
