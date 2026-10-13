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

use oxc_ast::AstKind;
use oxc_ast::ast::{
    ArrayExpressionElement, ArrowFunctionExpression, BindingPattern, Expression, ForStatementInit,
    FormalParameters, Function, ImportPhase, ModuleDeclaration, ModuleExportName, Program,
    PropertyKey, SimpleAssignmentTarget, Statement, VariableDeclaration,
};
use oxc_semantic::{AstNodes, ScopeId, Scoping, SymbolId};
use oxc_span::GetSpan;
use std::collections::HashSet;
use v12_bytecode::{EvalBindingKind, EvalCallerFacts, EvalCompileFacts, EvalPhase};

use crate::model::{
    BlockFunctionPlan, BlockFunctionSkip, CompileError, EvalSitePlan, ExportEntry, ImportEntry,
    MAX_ENV_SLOTS, MAX_REGS, ModuleBindingPlan, ModuleBindingSource, Plans, REG_THIS, UnitPlan,
    VarLoc,
};

/// Entry point: produce finalized layout plans for a whole program.
pub fn collect(
    program: &Program<'_>,
    scoping: &Scoping,
    is_strict: bool,
) -> Result<Plans, CompileError> {
    collect_inner(program, scoping, is_strict, false, None, None)
}

pub fn collect_with_eval_facts(
    program: &Program<'_>,
    scoping: &Scoping,
    is_strict: bool,
    facts: &EvalCompileFacts,
    nodes: Option<&AstNodes<'_>>,
) -> Result<Plans, CompileError> {
    collect_inner(program, scoping, is_strict, false, Some(facts), nodes)
}

/// Module-mode collection: top-level `var` stays module-scoped.
pub fn collect_module_with_nodes(
    program: &Program<'_>,
    scoping: &Scoping,
    is_strict: bool,
    nodes: Option<&AstNodes<'_>>,
) -> Result<Plans, CompileError> {
    collect_inner(program, scoping, is_strict, true, None, nodes)
}

fn collect_inner(
    program: &Program<'_>,
    scoping: &Scoping,
    is_strict: bool,
    is_module: bool,
    eval_caller_facts: Option<&EvalCompileFacts>,
    nodes: Option<&AstNodes<'_>>,
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
        list_stack: Vec::new(),
        control_depth: 0,
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
    c.stmt_list_kind(&program.body, ListKind::FunctionBody);
    if let Some(err) = c.early_error {
        return Err(err);
    }
    resolve_block_functions(&mut c.plans, c.scoping);
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
    resolve_eval_sites(&mut plans, scoping, nodes);
    if is_module {
        build_module_binding_plans(&mut plans, scoping);
    }
    if !is_strict
        && let Some(facts) = eval_caller_facts
        && facts.direct
        && plans.units[0]
            .var_declared_names
            .iter()
            .any(|name| name == "arguments")
        && facts.caller.bindings.iter().any(|(name, kind)| {
            name == "arguments"
                && (*kind != EvalBindingKind::ImplicitArguments
                    || facts.caller.arguments_object_needed)
        })
    {
        return Err(CompileError {
            message: "SyntaxError: 'arguments' is not a valid var binding in direct eval".into(),
            span: None,
        });
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
///
/// `PropertyKey` flattens to 46 variants — two direct
/// (`StaticIdentifier`, `PrivateIdentifier`) plus every `Expression` through
/// `INHERIT`. The `_` is a genuine general fallback, not a leftover: this
/// function's whole answer is "is this key a private name?", and no
/// `Expression` variant is one, so all 44 of them share the `PrivateIdentifier`
/// arm's answer. Naming them individually would be 44 arms of `=> None` that
/// say nothing a reader does not already get from the type.
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
        // `ClassElement` has 5 variants and no `INHERIT`. `StaticBlock` and
        // `AccessorProperty` are real JavaScript that declares a private name
        // (`class C { static { C.#x } }`, `class C { accessor #x = 1 }`) — both
        // are rejected downstream by Pass B (`class.rs:237`/`:243`, "not
        // supported yet"), so naming them here records *where* the gap is
        // rather than letting a reader assume nobody looked.
        let key = match el {
            oxc_ast::ast::ClassElement::MethodDefinition(m) => &m.key,
            oxc_ast::ast::ClassElement::PropertyDefinition(p) => &p.key,
            oxc_ast::ast::ClassElement::StaticBlock(_)
            | oxc_ast::ast::ClassElement::AccessorProperty(_)
            | oxc_ast::ast::ClassElement::TSIndexSignature(_) => continue,
        };
        if let Some(n) = private_name_text(key) {
            names.push(n);
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
                // The update *target* is a member, but `ContainsArguments`
                // reaches through it to the object: `(arguments).x++` reads
                // `arguments`. These were `=> false` under a wildcard, so
                // `class C { x = (arguments).x++ }`, `(arguments)[0]++` and
                // `(arguments.x)++` all compiled; node rejects all three
                // (`SyntaxError`, verified on node 24).
                oxc_ast::ast::SimpleAssignmentTarget::ComputedMemberExpression(c) => {
                    walk_expr(&c.object) || walk_expr(&c.expression)
                }
                oxc_ast::ast::SimpleAssignmentTarget::StaticMemberExpression(s) => {
                    walk_expr(&s.object)
                }
                oxc_ast::ast::SimpleAssignmentTarget::PrivateFieldExpression(p) => {
                    walk_expr(&p.object)
                }
                // Never produced: the parser is never asked for TypeScript.
                oxc_ast::ast::SimpleAssignmentTarget::TSAsExpression(_)
                | oxc_ast::ast::SimpleAssignmentTarget::TSSatisfiesExpression(_)
                | oxc_ast::ast::SimpleAssignmentTarget::TSNonNullExpression(_)
                | oxc_ast::ast::SimpleAssignmentTarget::TSTypeAssertion(_) => false,
            },
            Expression::AssignmentExpression(a) => {
                // The whole left side, not just an identifier: `({a: b =
                // arguments} = o)` and `([a = arguments] = o)` both put the
                // reference in a destructuring default. `walk_assignment_target`
                // is exhaustive over `AssignmentTarget` and subsumes the
                // identifier and array cases this used to spell out inline.
                walk_assignment_target(&a.left) || walk_expr(&a.right)
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
            // ES §15.7.1: `FieldDefinition : It is a Syntax Error if
            // ContainsArguments of FieldDefinition is true`, and
            // ContainsArguments is defined over the whole expression tree — an
            // optional chain or a dynamic `import` does not stop it. Missing
            // these two arms (they used to fall into `_ => false`) made
            // `class C { x = (() => arguments)?.f }` and
            // `class C { x = import(arguments) }` compile silently.
            Expression::ChainExpression(ch) => match &ch.expression {
                oxc_ast::ast::ChainElement::CallExpression(c) => {
                    walk_expr(&c.callee)
                        || c.arguments.iter().any(|a| match a {
                            oxc_ast::ast::Argument::SpreadElement(s) => walk_expr(&s.argument),
                            other => other.as_expression().is_some_and(walk_expr),
                        })
                }
                oxc_ast::ast::ChainElement::ComputedMemberExpression(c) => {
                    walk_expr(&c.object) || walk_expr(&c.expression)
                }
                oxc_ast::ast::ChainElement::StaticMemberExpression(s) => walk_expr(&s.object),
                oxc_ast::ast::ChainElement::PrivateFieldExpression(p) => walk_expr(&p.object),
                oxc_ast::ast::ChainElement::TSNonNullExpression(n) => walk_expr(&n.expression),
            },
            Expression::ImportExpression(i) => {
                walk_expr(&i.source) || i.options.as_ref().is_some_and(walk_expr)
            }
            // Nothing to search: a literal has no subexpressions, and `this`/
            // `super`/`import.meta`/`new.target` are keyword reads with no
            // operands, so none of them can reach the `arguments` identifier.
            // The `TS*`/`JSX*`/`V8Intrinsic` variants are never produced (the
            // parser is never asked for TypeScript or JSX).
            Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::RegExpLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::ThisExpression(_)
            | Expression::Super(_)
            | Expression::ImportMeta(_)
            | Expression::NewTarget(_)
            | Expression::TSAsExpression(_)
            | Expression::TSSatisfiesExpression(_)
            | Expression::TSTypeAssertion(_)
            | Expression::TSNonNullExpression(_)
            | Expression::TSInstantiationExpression(_)
            | Expression::V8IntrinsicExpression(_)
            | Expression::JSXElement(_)
            | Expression::JSXFragment(_) => false,
        }
    }
    /// ES §15.7.1: a binding pattern is not just a shape — `AssignmentPattern`
    /// and `BindingProperty` both carry a *default*, and a computed
    /// `BindingProperty` key is a full `Expression`. All three are
    /// `CanContainArguments`, so a pattern that only names identifiers is
    /// invisible to `ContainsArguments` unless it is descended into.
    ///
    /// Exhaustive over `BindingPattern` (4 variants) and `BindingProperty` is a
    /// struct, so there is nothing to match on its key beyond `computed`.
    fn walk_binding_pattern(p: &BindingPattern<'_>) -> bool {
        match p {
            // A bare binding *name* is not a reference: `let arguments = 1` is
            // rejected by the strict-mode binding rule in `Collect`, not here.
            // (`ContainsArguments` is about reading the binding.)
            BindingPattern::BindingIdentifier(_) => false,
            BindingPattern::ObjectPattern(o) => {
                o.properties.iter().any(walk_binding_property)
                    || o.rest
                        .as_ref()
                        .is_some_and(|r| walk_binding_pattern(&r.argument))
            }
            BindingPattern::ArrayPattern(a) => {
                a.elements.iter().flatten().any(walk_binding_pattern)
                    || a.rest
                        .as_ref()
                        .is_some_and(|r| walk_binding_pattern(&r.argument))
            }
            // `{a = arguments}` / `[a = arguments]`: the default is an
            // `AssignmentExpression` that runs when the element is undefined.
            BindingPattern::AssignmentPattern(a) => {
                walk_expr(&a.right) || walk_binding_pattern(&a.left)
            }
        }
    }
    fn walk_binding_property(p: &oxc_ast::ast::BindingProperty<'_>) -> bool {
        // `{[arguments]: a}`: a computed key is evaluated like any other
        // expression. A static/private key is a name, not a reference.
        (p.computed && p.key.as_expression().is_some_and(walk_expr))
            || walk_binding_pattern(&p.value)
    }
    /// The assignment-target twin of `walk_binding_pattern`: used for a for-in/
    /// of head that is an assignment rather than a declaration, and for the
    /// left of an `AssignmentExpression`. Exhaustive over `AssignmentTarget`'s
    /// flattened 13 (`SimpleAssignmentTarget` 9 + `AssignmentTargetPattern` 2 +
    /// ... resolved through `INHERIT`), matching `walk_expr`'s member arms.
    fn walk_assignment_target(t: &oxc_ast::ast::AssignmentTarget<'_>) -> bool {
        match t {
            // `for (arguments of o)`: the head reads the binding.
            oxc_ast::ast::AssignmentTarget::AssignmentTargetIdentifier(id) => {
                id.name.as_str() == "arguments"
            }
            oxc_ast::ast::AssignmentTarget::ComputedMemberExpression(c) => {
                walk_expr(&c.object) || walk_expr(&c.expression)
            }
            oxc_ast::ast::AssignmentTarget::StaticMemberExpression(s) => walk_expr(&s.object),
            oxc_ast::ast::AssignmentTarget::PrivateFieldExpression(p) => walk_expr(&p.object),
            oxc_ast::ast::AssignmentTarget::ArrayAssignmentTarget(arr) => {
                arr.elements.iter().flatten().any(walk_maybe_default)
                    || arr
                        .rest
                        .as_ref()
                        .is_some_and(|r| walk_assignment_target(&r.target))
            }
            oxc_ast::ast::AssignmentTarget::ObjectAssignmentTarget(obj) => {
                obj.properties.iter().any(walk_assignment_property)
                    || obj
                        .rest
                        .as_ref()
                        .is_some_and(|r| walk_assignment_target(&r.target))
            }
            oxc_ast::ast::AssignmentTarget::TSAsExpression(_)
            | oxc_ast::ast::AssignmentTarget::TSSatisfiesExpression(_)
            | oxc_ast::ast::AssignmentTarget::TSNonNullExpression(_)
            | oxc_ast::ast::AssignmentTarget::TSTypeAssertion(_) => false,
        }
    }
    /// `[a = arguments]` / `({a: b = arguments} = o)`: the default runs at the
    /// assignment, so it is searched. `AssignmentTargetMaybeDefault` is
    /// `AssignmentTargetWithDefault` plus `INHERIT(AssignmentTarget)`.
    fn walk_maybe_default(t: &oxc_ast::ast::AssignmentTargetMaybeDefault<'_>) -> bool {
        match t {
            oxc_ast::ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) => {
                walk_expr(&d.init) || walk_assignment_target(&d.binding)
            }
            other => other
                .as_assignment_target()
                .is_some_and(walk_assignment_target),
        }
    }
    fn walk_assignment_property(p: &oxc_ast::ast::AssignmentTargetProperty<'_>) -> bool {
        match p {
            // `{a} = o` shorthand: the key *is* the reference. `{a = init}` (the
            // `init` field) is the default form.
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(id) => {
                id.binding.name.as_str() == "arguments" || id.init.as_ref().is_some_and(walk_expr)
            }
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(pp) => {
                (pp.computed && pp.name.as_expression().is_some_and(walk_expr))
                    || walk_maybe_default(&pp.binding)
            }
        }
    }
    /// A declarator's *initializer* is one of two places an expression can hide
    /// in a declaration; the other is the binding pattern, which is why this
    /// descends into `d.id` as well. Shared by the `VariableDeclaration` arm
    /// above and the three loop heads below.
    fn walk_var_decl(v: &oxc_ast::ast::VariableDeclaration<'_>) -> bool {
        v.declarations
            .iter()
            .any(|d| d.init.as_ref().is_some_and(walk_expr) || walk_binding_pattern(&d.id))
    }
    /// The `left` of a for-in/of head. `Collect::stmt` splits this the same way
    /// (`:857-900`): a declaration, or an assignment target.
    ///
    /// Enumerated rather than wildcarded: `ForStatementLeft` flattens to 15, but
    /// 14 of them are `AssignmentTarget`'s variants verbatim (`INHERIT`), so
    /// converting through `as_assignment_target()` and delegating is both
    /// exhaustive and short. A new `AssignmentTarget` variant is a compile error
    /// in `walk_assignment_target` above, which is the guarantee that matters.
    fn walk_for_left(l: &oxc_ast::ast::ForStatementLeft<'_>) -> bool {
        match l {
            oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) => walk_var_decl(v),
            oxc_ast::ast::ForStatementLeft::TSAsExpression(_)
            | oxc_ast::ast::ForStatementLeft::TSSatisfiesExpression(_)
            | oxc_ast::ast::ForStatementLeft::TSNonNullExpression(_)
            | oxc_ast::ast::ForStatementLeft::TSTypeAssertion(_) => false,
            other => other
                .as_assignment_target()
                .is_some_and(walk_assignment_target),
        }
    }
    fn walk_stmt(s: &Statement<'_>) -> bool {
        match s {
            Statement::ExpressionStatement(e) => walk_expr(&e.expression),
            Statement::ReturnStatement(r) => r.argument.as_ref().is_some_and(walk_expr),
            Statement::IfStatement(i) => walk_expr(&i.test) || walk_stmt(&i.consequent),
            Statement::BlockStatement(b) => b.body.iter().any(walk_stmt),
            Statement::VariableDeclaration(v) => walk_var_decl(v),
            Statement::ThrowStatement(t) => walk_expr(&t.argument),
            Statement::FunctionDeclaration(_) => false,
            // Nothing to search, genuinely: `break`/`continue`/`empty`/
            // `debugger` carry no expression and no binding (a label is not a
            // symbol); the eight module declarations are top-level only, so they
            // cannot appear in the arrow body being walked; the nine `TS*`
            // variants are never produced (the parser is never asked for
            // TypeScript).
            Statement::BreakStatement(_)
            | Statement::ContinueStatement(_)
            | Statement::DebuggerStatement(_)
            | Statement::EmptyStatement(_)
            | Statement::ImportDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSExternalModuleDeclaration(_)
            | Statement::TSNamespaceDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_) => false,
            // These ten all reach expressions, so ES §15.7.1
            // `ContainsArguments` descends into them. They were `=> false` in
            // 3dce22fa (W1.2 batch 1), which named them `KNOWN WRONG`:
            // ADR-0002's "only the 2 `contains_arguments` gaps are genuine" was
            // wrong — it is 12, these ten plus `ChainExpression` /
            // `ImportExpression`. `walk_stmt` is reached only through an
            // arrow's block body, which is why a field initializer could carry
            // a loop and still miss the `arguments` inside it.
            Statement::WhileStatement(w) => walk_expr(&w.test) || walk_stmt(&w.body),
            Statement::DoWhileStatement(d) => walk_expr(&d.test) || walk_stmt(&d.body),
            Statement::ForStatement(f) => {
                // `init` is optional and is a declaration *or* an expression,
                // split exactly as `Collect::stmt` splits it (`:857-862`); the
                // declaration side is not covered by the expression path.
                (match &f.init {
                    Some(ForStatementInit::VariableDeclaration(v)) => walk_var_decl(v),
                    Some(other) => other.as_expression().is_some_and(walk_expr),
                    None => false,
                }) || f.test.as_ref().is_some_and(walk_expr)
                    || f.update.as_ref().is_some_and(walk_expr)
                    || walk_stmt(&f.body)
            }
            Statement::ForInStatement(f) => {
                walk_for_left(&f.left) || walk_expr(&f.right) || walk_stmt(&f.body)
            }
            Statement::ForOfStatement(f) => {
                walk_for_left(&f.left) || walk_expr(&f.right) || walk_stmt(&f.body)
            }
            Statement::SwitchStatement(s) => {
                walk_expr(&s.discriminant)
                    || s.cases.iter().any(|case| {
                        case.test.as_ref().is_some_and(walk_expr)
                            || case.consequent.iter().any(walk_stmt)
                    })
            }
            Statement::TryStatement(t) => {
                t.block.body.iter().any(walk_stmt)
                    || t.handler.as_ref().is_some_and(|h| {
                        // The catch parameter is a `CatchParameter`, whose
                        // `pattern` is a `BindingPattern`. For an identifier it
                        // is not a source of `arguments` (that is the strict
                        // binding rule in `Collect`, not `ContainsArguments`),
                        // but a destructuring default is:
                        // `try {} catch ({a = arguments}) {}`. Fixed together
                        // with the rest of the pattern-default gap — this arm
                        // used to walk the clause body only.
                        h.param
                            .as_ref()
                            .is_some_and(|p| walk_binding_pattern(&p.pattern))
                            || h.body.body.iter().any(walk_stmt)
                    })
                    || t.finalizer
                        .as_ref()
                        .is_some_and(|f| f.body.iter().any(walk_stmt))
            }
            Statement::LabeledStatement(l) => walk_stmt(&l.body),
            // Unreachable in practice: a field initializer is strict-mode code,
            // so `with` is already a SyntaxError there (`stmt.rs:776`). Kept
            // correct anyway — `contains_arguments` has exactly one call site
            // and that strictness is an accident of *this* caller, not a
            // property of the walker.
            Statement::WithStatement(w) => walk_expr(&w.object) || walk_stmt(&w.body),
            // Heritage only, mirroring the `ClassExpression` arm in `walk_expr`:
            // a nested class's methods and field initializers are their own
            // `FunctionExpression` / `FieldDefinition` early errors, not this
            // one's, so the search must not descend past the heritage.
            Statement::ClassDeclaration(c) => c
                .heritage
                .as_ref()
                .is_some_and(|h| walk_expr(&h.expression)),
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

#[derive(Debug, PartialEq, Eq)]
enum ListKind {
    FunctionBody,
    Block,
    Control,
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
    list_stack: Vec<ListKind>,
    control_depth: usize,
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
        self.plans
            .symbol_names
            .insert(sym, self.scoping.symbol_name(sym).to_string());
        self.plans.units[unit].decl_order.push(sym);
    }

    /// Walks a member expression's operands: the object (which carries the
    /// *references*, including the base identifier of an optional chain) and,
    /// for a computed key, the key expression.
    ///
    /// Shared by the `Expression` member arms and by the optional-chain arm so
    /// both agree on what counts as a reference — they did not, and the chain
    /// arm's blindness is what made TDZ checks read a mislaid flag.
    ///
    /// Takes the `Option` oxc's `as_member_expression` hands back so both
    /// call sites (`Expression` arms and the chain's own element) share it.
    fn member(&mut self, m: Option<&oxc_ast::ast::MemberExpression<'_>>) {
        let Some(m) = m else {
            return;
        };
        match m {
            oxc_ast::ast::MemberExpression::ComputedMemberExpression(c) => {
                self.expr(&c.object);
                self.expr(&c.expression);
            }
            oxc_ast::ast::MemberExpression::StaticMemberExpression(s) => self.expr(&s.object),
            oxc_ast::ast::MemberExpression::PrivateFieldExpression(p) => {
                self.check_private_name_ref(&format!("#{}", p.field.name), p.span);
                self.expr(&p.object);
            }
        }
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
            self.fn_unit_ext(f, false, &format!("<objmethod>:{hint}"), true, true, false);
        }
    }

    /// Registers a non-arrow function unit and walks params + body inside it.
    ///
    /// `declare_name_here` is `Some(())` for declarations (name binds in the
    /// enclosing unit) and `None` for expressions (named expressions bind
    /// their own name inside themselves).
    fn fn_unit(
        &mut self,
        f: &Function<'_>,
        declare_name_in_enclosing: bool,
        hint: &str,
        block_candidate: bool,
    ) -> usize {
        self.fn_unit_ext(
            f,
            declare_name_in_enclosing,
            hint,
            false,
            false,
            block_candidate,
        )
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
        is_object_method: bool,
        block_candidate: bool,
    ) -> usize {
        let parent = self.cur_unit();
        if declare_name_in_enclosing
            && let Some(id) = &f.id
            && let Some(sym) = id.symbol_id.get()
        {
            self.declare(sym);
            self.record_declared_names(parent, vec![id.name.to_string()], true);
            self.plans.units[parent].var_symbols.insert(sym);
            if parent == 0 && !self.plans.is_module {
                self.plans.global_vars.insert(sym);
            }
        }
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
        plan.function_scope = f.scope_id.get();
        plan.implicit_arguments = true;
        plan.has_non_simple_parameters = !is_simple_parameter_list(&f.params);
        plan.allows_super = allows_super;
        plan.is_object_method = is_object_method;
        // A named function's `name` own property (installed at closure alloc):
        // without this the name is unobservable (`f.name === undefined`).
        if let Some(id) = f.id.as_ref() {
            plan.function_name = Some(id.name.to_string());
        }
        self.plans.units.push(plan);
        self.plans.fn_index.insert(f.span(), idx);
        if block_candidate
            && parent != 0
            && !is_strict
            && !f.generator
            && !f.r#async
            && let Some(symbol) = f.id.as_ref().and_then(|id| id.symbol_id.get())
            && let Some(function_scope) = f.scope_id.get()
        {
            let declaration_scope = self
                .scoping
                .scope_parent_id(function_scope)
                .unwrap_or_else(|| self.scoping.root_scope_id());
            let order = self.plans.units[parent].block_functions.len();
            self.plans.units[parent]
                .block_functions
                .push(BlockFunctionPlan {
                    order,
                    span: f.span(),
                    declaration_scope,
                    symbol,
                    child_unit: idx,
                    lexical_location: None,
                    var_location: None,
                    var_symbol: None,
                    skip: BlockFunctionSkip::Eligible,
                });
        }
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
            self.stmt_list_kind(&body.statements, ListKind::FunctionBody);
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
        plan.function_scope = a.scope_id.get();
        plan.has_non_simple_parameters = !is_simple_parameter_list(&a.params);
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
            Some(body) => self.stmt_list_kind(&body.statements, ListKind::FunctionBody),
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
        self.stmt_list_kind(stmts, ListKind::Block);
    }

    fn stmt_list_kind(&mut self, stmts: &[Statement<'_>], kind: ListKind) {
        self.list_stack.push(kind);
        for s in stmts {
            self.stmt(s);
        }
        self.list_stack.pop();
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
                self.control_depth += 1;
                self.stmt(&i.consequent);
                if let Some(alt) = &i.alternate {
                    self.stmt(alt);
                }
                self.control_depth -= 1;
            }
            Statement::WhileStatement(w) => {
                self.expr(&w.test);
                self.control_depth += 1;
                self.stmt(&w.body);
                self.control_depth -= 1;
            }
            Statement::DoWhileStatement(d) => {
                self.control_depth += 1;
                self.stmt(&d.body);
                self.control_depth -= 1;
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
                self.control_depth += 1;
                self.stmt(&f.body);
                self.control_depth -= 1;
            }
            Statement::ForInStatement(f) => {
                // `for (const k in obj)`: declare the left binding pattern
                // (the loop variable), then walk the right and body.
                self.for_left(&f.left);
                self.expr(&f.right);
                self.control_depth += 1;
                self.stmt(&f.body);
                self.control_depth -= 1;
            }
            Statement::ForOfStatement(f) => {
                // `for (const x of iterable)`: the left binding pattern is a
                // fresh per-iteration binding — declare it in the current
                // unit so `access()` resolves a real register, not r0.
                self.for_left(&f.left);
                self.expr(&f.right);
                self.control_depth += 1;
                self.stmt(&f.body);
                self.control_depth -= 1;
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
                    let unit = self.cur_unit();
                    if let Some(param) = &h.param {
                        self.plans.units[unit]
                            .lexical_names
                            .extend(binding_names(&param.pattern));
                        self.binding_pattern(&param.pattern);
                    }
                    self.stmt_list(&h.body.body);
                }
                if let Some(fin) = &t.finalizer {
                    self.stmt_list(&fin.body);
                }
            }
            Statement::LabeledStatement(l) => {
                self.control_depth += 1;
                self.stmt(&l.body);
                self.control_depth -= 1;
            }
            Statement::FunctionDeclaration(f) => {
                let block_candidate = self.cur_unit() != 0
                    && (self.list_stack.last() != Some(&ListKind::FunctionBody)
                        || self.control_depth != 0);
                self.fn_unit(f, true, "<fn>", block_candidate);
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
                    self.stmt_list_kind(&case.consequent, ListKind::Control);
                }
            }
            Statement::WithStatement(w) => {
                self.expr(&w.object);
                self.stmt(&w.body);
            }
            // Nothing to collect: no binding, no reference, no nested unit.
            //   - `break`/`continue`/`empty`/`debugger` carry no expression. A
            //     label is not a symbol; `LabeledStatement` above is the arm
            //     that walks the labelled body.
            //   - The eight module declarations never reach this match: the
            //     `as_module_declaration()` prefilter above returns first. They
            //     are named so this match is exhaustive in its own right rather
            //     than by courtesy of a prefilter, and so the pre-existing
            //     convention of writing out `TSExportAssignment` /
            //     `TSNamespaceExportDeclaration` there continues below.
            //     (`as_module_declaration()` is what makes that prefilter
            //     exhaustive, which is why the two TS forms were already
            //     listed in it.)
            //   - The seven `TS*Declaration` variants are TypeScript. The
            //     parser is only ever given `SourceType::script()`/`mjs()` and
            //     neither sets `with_typescript`, so oxc cannot produce them.
            Statement::BreakStatement(_)
            | Statement::ContinueStatement(_)
            | Statement::DebuggerStatement(_)
            | Statement::EmptyStatement(_)
            | Statement::ImportDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSExternalModuleDeclaration(_)
            | Statement::TSNamespaceDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_) => {}
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
                // `StaticBlock` / `AccessorProperty` *can* declare a private
                // name, and a duplicate one there would be an error Pass B
                // never reaches (it rejects both forms outright — see
                // `class.rs:237`/`:243`). Named so the gap is visible rather
                // than looking like a considered "these declare nothing".
                oxc_ast::ast::ClassElement::StaticBlock(_)
                | oxc_ast::ast::ClassElement::AccessorProperty(_)
                | oxc_ast::ast::ClassElement::TSIndexSignature(_) => continue,
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
            // A `constructor` is spelled as a `MethodDefinition` and nothing
            // else; the remaining four `ClassElement` variants cannot be one.
            oxc_ast::ast::ClassElement::MethodDefinition(_)
            | oxc_ast::ast::ClassElement::PropertyDefinition(_)
            | oxc_ast::ast::ClassElement::StaticBlock(_)
            | oxc_ast::ast::ClassElement::AccessorProperty(_)
            | oxc_ast::ast::ClassElement::TSIndexSignature(_) => None,
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
                self.stmt_list_kind(&body.statements, ListKind::FunctionBody);
            }
            self.walking_params = prev_params;
        } else if c.heritage.is_some() {
            // Default derived constructor. ES §15.7.1 synthesizes
            // `constructor(...args) { super(...args); }`, so the emitter must
            // forward every argument to the parent. Reserving a rest
            // parameter (with no user binding: `rest_ident` stays `None`)
            // makes the call ABI materialize the argument list as a real
            // array at `r{arity+1}`, which `Opcode::CallApply` can spread.
            // Marking the unit as using `super` lets the emitter resolve the
            // parent constructor from the class environment.
            self.plans.units[idx].has_rest = true;
            self.plans.units[idx].uses_super = true;
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
            let key = match el {
                oxc_ast::ast::ClassElement::MethodDefinition(m) if m.computed => &m.key,
                oxc_ast::ast::ClassElement::PropertyDefinition(p) if p.computed => &p.key,
                // Not computed, so there is no key expression to walk. The
                // first two arms' guards fall through to here when their key
                // is static.
                oxc_ast::ast::ClassElement::MethodDefinition(_)
                | oxc_ast::ast::ClassElement::PropertyDefinition(_) => continue,
                // `StaticBlock` and `AccessorProperty` also have a computed
                // key in principle (`class C { accessor [e] = 1 }`), but
                // neither is lowered today — Pass B rejects both outright
                // (`class.rs:237`/`:243`) — so there is nothing to walk, and
                // the key loop must not be what decides that.
                oxc_ast::ast::ClassElement::StaticBlock(_)
                | oxc_ast::ast::ClassElement::AccessorProperty(_)
                | oxc_ast::ast::ClassElement::TSIndexSignature(_) => continue,
            };
            if let Some(kx) = key.as_expression() {
                self.unit_stack.push(idx);
                self.expr(kx);
                self.unit_stack.pop();
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
                    // `MethodDefinitionKind` has 4 variants and no
                    // `INHERIT`. Only the two accessors have a prefix; a
                    // `constructor` cannot reach here (the loop condition
                    // above excludes it) and a plain method has none.
                    oxc_ast::ast::MethodDefinitionKind::Get => "get ",
                    oxc_ast::ast::MethodDefinitionKind::Set => "set ",
                    oxc_ast::ast::MethodDefinitionKind::Method
                    | oxc_ast::ast::MethodDefinitionKind::Constructor => "",
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
                    self.stmt_list_kind(&body.statements, ListKind::FunctionBody);
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
        // `import defer * as ns from "x"` (defer-import-eval): the dependency
        // links but its evaluation is postponed until an export is read. The
        // parser admits `defer` only on a namespace form (the invalid named/
        // default spellings are early errors), so one flag on every entry of
        // the declaration is sufficient.
        let deferred = matches!(d.phase, Some(ImportPhase::Defer));
        if let Some(specs) = &d.specifiers {
            if specs.is_empty() {
                // `import {} from "x"` – no bindings, but still a module dependency.
                self.plans.imports.push(ImportEntry {
                    specifier: specifier.clone(),
                    imported: String::new(),
                    local: None,
                    span,
                    deferred,
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
                            deferred,
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
                            deferred,
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
                            deferred,
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
                deferred,
            });
        }
    }

    fn export_decl(&mut self, d: &oxc_ast::ast::ExportDeclaration<'_>) {
        let span = Some((d.span.start, d.span.end));
        match &d.declaration {
            oxc_ast::ast::Declaration::VariableDeclaration(v) => {
                // Record each binding as an export.
                //
                // `binding_symbols`, not `binding_symbol`: `export const [a] =
                // [1]` and `export var {p, q} = …` are ordinary module syntax
                // and export *every* binding the pattern introduces, but
                // `binding_symbol` returns `None` for anything that is not a
                // bare `BindingIdentifier`, so the whole export was silently
                // dropped and the module exported nothing at all. Measured on
                // `2411f4cb`: `export const [a] = [1]` produced an empty
                // export table where node produces `['a']`. The exported name
                // is the *binding's* name — `scoping.symbol_name` — which for
                // a pattern leaf is the identifier the pattern binds, so this
                // is what `ident_name_of_binding` answered for the
                // identifier case.
                for decl in &v.declarations {
                    for sym in binding_symbols(&decl.id) {
                        let exported = self.scoping.symbol_name(sym).to_string();
                        self.plans.exports.push(ExportEntry {
                            specifier: None,
                            imported: None,
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
                        imported: None,
                        local: Some(sym),
                        exported,
                        span,
                    });
                }
                self.fn_unit(f, true, "<fn>", false);
            }
            oxc_ast::ast::Declaration::ClassDeclaration(c) => {
                if let Some(id) = &c.id
                    && let Some(sym) = id.symbol_id.get()
                {
                    let exported = id.name.to_string();
                    self.plans.exports.push(ExportEntry {
                        specifier: None,
                        imported: None,
                        local: Some(sym),
                        exported,
                        span,
                    });
                }
                // Known gap, not a wildcard: `export class C {}` records the
                // export entry but never calls `class_unit`, so the class body
                // is not walked for captures/`super`/field initializers the way
                // a plain `class C {}` declaration is. Left alone — the fix is
                // a behaviour change needing conformance measurement.
            }
            // `Declaration` has 10 variants and no `INHERIT`. The three above
            // are the only ones a JavaScript module can export a *runtime*
            // binding from; the other seven are TypeScript declarations, which
            // never produce a runtime binding and are never parsed (the parser
            // is only ever given `SourceType::script()`/`mjs()`).
            oxc_ast::ast::Declaration::TSTypeAliasDeclaration(_)
            | oxc_ast::ast::Declaration::TSInterfaceDeclaration(_)
            | oxc_ast::ast::Declaration::TSEnumDeclaration(_)
            | oxc_ast::ast::Declaration::TSExternalModuleDeclaration(_)
            | oxc_ast::ast::Declaration::TSNamespaceDeclaration(_)
            | oxc_ast::ast::Declaration::TSGlobalDeclaration(_)
            | oxc_ast::ast::Declaration::TSImportEqualsDeclaration(_) => {}
        }
    }

    fn export_named_decl(&mut self, d: &oxc_ast::ast::ExportNamedDeclaration<'_>) {
        let span = Some((d.span.start, d.span.end));
        for sp in &d.specifiers {
            let local = module_export_name_to_symbol(&sp.local, self.scoping);
            let exported = module_export_name_to_string(&sp.exported);
            self.plans.exports.push(ExportEntry {
                specifier: None,
                imported: None,
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
            let imported = module_export_name_to_string(&sp.local);
            let local = module_export_name_to_symbol(&sp.local, self.scoping);
            self.plans.exports.push(ExportEntry {
                specifier: Some(specifier.clone()),
                imported: Some(imported),
                local,
                exported,
                span,
            });
            // Indirect exports are module dependencies too. Keep a
            // side-effect-free import entry so the loader resolves and links
            // the source module before this module's main runs.
            self.plans.imports.push(ImportEntry {
                specifier: specifier.clone(),
                imported: String::new(),
                local: None,
                span,
                deferred: false,
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
            // Documented general fallback. `ExportDefaultDeclarationKind` is
            // `FunctionDeclaration`/`ClassDeclaration`/`TSInterfaceDeclaration`
            // plus every `Expression` (47 flattened). The `TS*` variant is
            // named; the remaining `other` is the 44 `Expression` variants,
            // and none of them can introduce a *local binding* —
            // `export default 1` and `export default (class {})` are
            // expressions with no name to bind. Naming 44 arms of `=> None`
            // would restate the type, not add information.
            oxc_ast::ast::ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => None,
            other => {
                debug_assert!(
                    other.as_expression().is_some(),
                    "ExportDefaultDeclarationKind must be a declaration, an interface, or an expression",
                );
                None
            }
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
            imported: None,
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
                self.fn_unit(f, false, "<default fn>", false);
            }
            oxc_ast::ast::ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                // KNOWN GAP, not a wildcard: a default-exported class is *not*
                // walked, so `export default class C extends A {}` never
                // notes the heritage reference `A`, and a field initializer
                // never gets its `this`/`super`/capture analysis. Left as is
                // deliberately — giving a default-exported class a unit is a
                // layout change that needs conformance measurement, not a
                // refactor. Recorded here so it reads as "known", the way the
                // `_ => {}` it replaced did not.
                let _ = c;
            }
            oxc_ast::ast::ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {}
            // Documented general fallback: the 44 `Expression` variants, all
            // of which are the expression form — `export default 1`. Delegated
            // through `as_expression()` rather than spelled out, which keeps
            // the recursion in one place: an `Expression` variant added later
            // is a compile error in `Collect::expr`, which this reaches.
            other => {
                if let Some(expr) = other.as_expression() {
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
            specifier: Some(specifier.clone()),
            imported: None,
            local: None,
            exported,
            span,
        });
        self.plans.imports.push(ImportEntry {
            specifier,
            imported: String::new(),
            local: None,
            span,
            deferred: false,
        });
    }

    fn record_declared_names(&mut self, unit: usize, names: Vec<String>, is_var: bool) {
        let target = if is_var {
            &mut self.plans.units[unit].var_names
        } else {
            &mut self.plans.units[unit].lexical_names
        };
        target.extend(names.iter().cloned());
        if is_var {
            for name in names {
                if !self.plans.units[unit].var_declared_names.contains(&name) {
                    self.plans.units[unit].var_declared_names.push(name);
                }
            }
        }
    }

    fn var_decl(&mut self, v: &VariableDeclaration<'_>) {
        let is_const = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Const);
        let is_var = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Var);
        let is_var_at_top = is_var && self.cur_unit() == 0 && !self.plans.is_module;
        let unit = self.cur_unit();
        for d in &v.declarations {
            self.record_declared_names(unit, binding_names(&d.id), is_var);
            if is_var {
                self.plans.units[unit]
                    .var_symbols
                    .extend(binding_symbols(&d.id));
            } else {
                self.plans.units[unit]
                    .lexical_symbols
                    .extend(binding_symbols(&d.id));
            }
            if is_const {
                self.plans.const_bindings.extend(binding_symbols(&d.id));
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
        for p in &params.items {
            self.plans.units[idx]
                .parameter_names
                .extend(binding_names(&p.pattern));
            self.plans.units[idx]
                .parameter_symbols
                .extend(binding_symbols(&p.pattern));
        }
        if let Some(rest) = &params.rest {
            self.plans.units[idx]
                .parameter_names
                .extend(binding_names(&rest.rest.argument));
            self.plans.units[idx]
                .parameter_symbols
                .extend(binding_symbols(&rest.rest.argument));
        }
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
                // An object-literal method's super base is the receiver's
                // prototype, so a `super` reached from inside an arrow needs
                // the owner to thread `this` through its environment — the
                // same requirement a bare `this` in that arrow creates.
                if self.plans.units[owner].is_object_method && owner != self.cur_unit() {
                    self.plans.units[owner].needs_this = true;
                }
                self.plans.units[owner].uses_super = true;
            }
            Expression::FunctionExpression(f) => {
                self.fn_unit(f, false, "<fnexpr>", false);
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
                if matches!(&c.callee, Expression::Identifier(id) if id.name.as_str() == "eval") {
                    let current = self.cur_unit();
                    let mut unit = Some(current);
                    while let Some(idx) = unit {
                        self.plans.units[idx].eval_reachable = true;
                        unit = self.plans.units[idx].parent;
                    }
                    self.plans.units[current]
                        .direct_eval_sites
                        .push(EvalSitePlan {
                            span: c.span,
                            phase: if self.walking_params {
                                EvalPhase::Parameter
                            } else {
                                EvalPhase::Body
                            },
                            scope_id: None,
                            pc: None,
                            caller: EvalCallerFacts::default(),
                        });
                }
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
            Expression::ChainExpression(ch) => {
                // An optional chain is just a member/call spine, and the arms
                // above already walk each link (object, computed key, call
                // arguments), so recursing on the chain's own expression
                // reaches the base identifier and any nested function.
                //
                // Skipping it — as this arm used to — left every reference
                // inside `a?.b` invisible: an `a` that only a closure reads
                // through a chain was never marked as escaping, so it was laid
                // out as a plain register of the *declaring* unit and the
                // closure read whatever happened to sit in that register of its
                // own frame. That was silently wrong before, and became a
                // spurious `ReferenceError` once TDZ checks read the same
                // mislaid flag.
                match &ch.expression {
                    oxc_ast::ast::ChainElement::CallExpression(c) => {
                        self.expr(&c.callee);
                        for a in &c.arguments {
                            match a {
                                oxc_ast::ast::Argument::SpreadElement(sp) => {
                                    self.expr(&sp.argument);
                                }
                                other => {
                                    if let Some(e) = other.as_expression() {
                                        self.expr(e);
                                    }
                                }
                            }
                        }
                    }
                    oxc_ast::ast::ChainElement::TSNonNullExpression(_) => {}
                    other => {
                        // `ChainElement`'s member variants are exactly the
                        // three `MemberExpression` ones; audited by the
                        // `INHERIT(MemberExpression)` declaration.
                        self.member(other.as_member_expression());
                    }
                }
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
            Expression::ComputedMemberExpression(_)
            | Expression::StaticMemberExpression(_)
            | Expression::PrivateFieldExpression(_) => self.member(e.as_member_expression()),
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
                        // Documented general fallback: `ArrayExpressionElement`
                        // is `SpreadElement`/`Elision` plus every `Expression`
                        // (46 flattened), and an array element that *is* an
                        // expression is by definition something to walk. The
                        // delegation is what makes this exhaustive in practice —
                        // a new `Expression` variant is a compile error in
                        // `Collect::expr`, which this reaches. Same shape as
                        // the `ObjectPropertyKind` arm above.
                        other => {
                            if let Some(x) = other.as_expression() {
                                self.expr(x);
                            }
                        }
                    }
                }
            }
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            Expression::ImportExpression(i) => {
                self.expr(&i.source);
                // `import(source, options)`: the optional second argument is a
                // full AssignmentExpression and is validated like any other.
                if let Some(opts) = &i.options {
                    self.expr(opts);
                }
            }
            // Nothing to collect: a literal has no subexpressions, and
            // `import.meta`/`new.target` are keyword reads with no operands.
            // The `TS*`/`JSX*`/`V8Intrinsic` variants are never produced: the
            // parser is only ever given `SourceType::script()`/`mjs()`, neither
            // of which sets `with_typescript`/`with_jsx`.
            Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::RegExpLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::ImportMeta(_)
            | Expression::NewTarget(_)
            | Expression::TSAsExpression(_)
            | Expression::TSSatisfiesExpression(_)
            | Expression::TSTypeAssertion(_)
            | Expression::TSNonNullExpression(_)
            | Expression::TSInstantiationExpression(_)
            | Expression::V8IntrinsicExpression(_)
            | Expression::JSXElement(_)
            | Expression::JSXFragment(_) => {}
        }
    }

    /// The `left` of a for-in/of head — `ForStatementLeft` (16 flattened
    /// variants). One method for both loops: the two statement arms were
    /// byte-identical, so any fix to one had to be made twice and a
    /// one-sided edit was a latent divergence.
    ///
    /// `ForStatementLeft` is `VariableDeclaration` plus `INHERIT(AssignmentTarget)`,
    /// i.e. the 15 `AssignmentTarget` variants verbatim. This used to match
    /// only `VariableDeclaration` and `AssignmentTargetIdentifier` and drop
    /// the other 13 behind a `_ => {}`, which is *not* a harmless
    /// "nothing to collect":
    ///
    /// - the dropped member heads (`for (o.x of xs)`, `for (o[k] of xs)`)
    ///   carry a read of `o` / `k`, and
    /// - the dropped destructuring heads (`for ([a] of xs)`,
    ///   `for ({x: a} of xs)`, `for ([a = 1] of xs)`) carry writes of `a`
    ///   plus the references inside every default.
    ///
    /// A reference that is never noted is a reference `finalize` cannot see,
    /// so the binding is never marked `captured`, the *declaring* unit never
    /// sets `has_env`, no environment is allocated, and the inner frame's
    /// write lands in a register of its own — the exact failure ADR-0004
    /// records for `class … extends`. Measured on `2411f4cb`:
    /// `let o={x:1}; let g=function(){ for (o.x of [5]) {} }` emitted **zero**
    /// `NewEnvironment` in the enclosing function, where the same write
    /// spelled `o.x = 5` emitted one.
    ///
    /// The remaining `other =>` arm is a *delegation*, not a swallow: the
    /// four `TS*` variants (the only `ForStatementLeft` variants that are not
    /// an `AssignmentTarget`) are named above, and every other variant is
    /// handed to `walk_assignment_target`, which is exhaustive over
    /// `AssignmentTarget` — so a new `AssignmentTarget` variant is a compile
    /// error *there* and is reached from here. This is the same shape as
    /// `contains_arguments::walk_for_left` (`:449`) and `walk_assignment_target`
    /// itself (`:2216`).
    fn for_left(&mut self, left: &oxc_ast::ast::ForStatementLeft<'_>) {
        match left {
            oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) => {
                // `for (const k in obj)` / `for (let x of xs)`: declare the
                // head bindings in the current unit so `access()` resolves a
                // real register, not r0.
                let unit = self.cur_unit();
                let is_const = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Const);
                let is_var = matches!(v.kind, oxc_ast::ast::VariableDeclarationKind::Var);
                for d in &v.declarations {
                    self.record_declared_names(unit, binding_names(&d.id), is_var);
                    if is_const {
                        // Every binding the head introduces is const, at any
                        // pattern depth. `binding_symbol` reached only the
                        // bare-identifier shape, so `for (const [y] of xs)`
                        // lost the "Assignment to constant variable" early
                        // error that `for (const y of xs)` fired — the
                        // `var_decl` path already had the pattern fallback
                        // (`collect_const_bindings`), and this is the same
                        // answer without the second walker.
                        self.plans.const_bindings.extend(binding_symbols(&d.id));
                    }
                    self.binding_pattern(&d.id);
                }
            }
            // Never produced: the parser is only ever given
            // `SourceType::script()`/`mjs()`, neither of which sets
            // `with_typescript`.
            oxc_ast::ast::ForStatementLeft::TSAsExpression(_)
            | oxc_ast::ast::ForStatementLeft::TSSatisfiesExpression(_)
            | oxc_ast::ast::ForStatementLeft::TSNonNullExpression(_)
            | oxc_ast::ast::ForStatementLeft::TSTypeAssertion(_) => {}
            other => {
                // Includes `AssignmentTargetIdentifier` (`for (x of xs)`), so
                // the head is now noted exactly as the same write would be
                // outside a loop head: the reference, the `arguments` mark,
                // and the strict-mode reserved-word early error that
                // `"use strict"; for (yield of xs) {}` needs and that only
                // `simple_target` checks.
                if let Some(target) = other.as_assignment_target() {
                    self.walk_assignment_target(target);
                }
            }
        }
    }

    fn walk_assignment_target(&mut self, target: &oxc_ast::ast::AssignmentTarget<'_>) {
        // Complex destructuring assignment targets: recurse into nested
        // patterns so references, captures, and nested functions inside
        // defaults are all registered (emission mirrors this recursion in
        // `destructure_assign`, including `lower_default` initializers).
        //
        // The match is exhaustive over `AssignmentTarget`'s flattened 15
        // (`SimpleAssignmentTarget` 9 + `AssignmentTargetPattern` 2, resolved
        // through `INHERIT`) *modulo the `other` delegation below*, which
        // exists because `simple_target` takes a `&SimpleAssignmentTarget` and
        // there is no way to narrow an `&AssignmentTarget` to one without
        // `as_simple_assignment_target()`. Note the shape this replaced: an
        // early `if let … { return; }` above the match, which hid the six
        // simple variants from rustc and left a `_ => {}` covering them —
        // which is how `ArrayAssignmentTarget`'s sibling arms drifted without
        // anyone noticing.
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
            // The two destructuring patterns are matched above and every
            // `SimpleAssignmentTarget` is delegated below, so nothing is
            // dropped. What the guard actually is, precisely: a new
            // `AssignmentTarget` variant is either a new *pattern* variant —
            // a compile error here, because both patterns are named — or a new
            // `SimpleAssignmentTarget` variant, which is a compile error in
            // `simple_target`, whose 9 variants are all named. The `other` arm
            // also absorbs the four `TS*` variants, which
            // `as_simple_assignment_target()` rejects and the parser never
            // produces; that is the one thing this arm drops, and it is a
            // deliberate no-op rather than an oversight.
            other => {
                if let Some(simple) = other.as_simple_assignment_target() {
                    self.simple_target(simple);
                }
            }
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
            // `SimpleAssignmentTarget` has 9 variants: the identifier and the
            // three member forms above, plus the four `TS*` ones — none of
            // which is ever produced (the parser is never asked for
            // TypeScript). Naming them so a new variant cannot be dropped
            // here unnoticed.
            SimpleAssignmentTarget::TSAsExpression(_)
            | SimpleAssignmentTarget::TSSatisfiesExpression(_)
            | SimpleAssignmentTarget::TSNonNullExpression(_)
            | SimpleAssignmentTarget::TSTypeAssertion(_) => {}
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

fn resolve_eval_sites(plans: &mut Plans, scoping: &Scoping, nodes: Option<&AstNodes<'_>>) {
    let Some(nodes) = nodes else {
        return;
    };
    for unit_index in 0..plans.units.len() {
        let strict = plans.units[unit_index].is_strict;
        let mut owner_index = unit_index;
        while plans.units[owner_index].is_arrow {
            owner_index = plans.units[owner_index].parent.expect("arrow has owner");
        }
        let (owner_implicit_arguments, owner_non_simple, owner_needs_arguments) = {
            let owner = &plans.units[owner_index];
            (
                owner.implicit_arguments,
                owner.has_non_simple_parameters,
                owner.needs_arguments,
            )
        };
        for site_index in 0..plans.units[unit_index].direct_eval_sites.len() {
            let span = plans.units[unit_index].direct_eval_sites[site_index].span;
            let Some(node) = nodes.iter().find(
                |node| matches!(node.kind(), AstKind::CallExpression(call) if call.span == span),
            ) else {
                continue;
            };
            let scope_id = node.scope_id();
            plans.units[unit_index].direct_eval_sites[site_index].scope_id = Some(scope_id);
            let mut seen = HashSet::new();
            for scope in scoping.scope_ancestors(scope_id) {
                for &symbol in scoping.get_bindings(scope).values() {
                    let name = scoping.symbol_name(symbol).to_string();
                    if !seen.insert(name.clone()) {
                        continue;
                    }
                    let (kind, binding_unit) = plans
                        .units
                        .iter()
                        .enumerate()
                        .find_map(|(index, candidate)| {
                            if candidate.parameter_symbols.contains(&symbol) {
                                Some((EvalBindingKind::Parameter, index))
                            } else if candidate.var_symbols.contains(&symbol) {
                                Some((EvalBindingKind::Var, index))
                            } else if candidate.lexical_symbols.contains(&symbol) {
                                Some((EvalBindingKind::Lexical, index))
                            } else {
                                None
                            }
                        })
                        .unwrap_or((EvalBindingKind::Var, usize::MAX));
                    let site = &plans.units[unit_index].direct_eval_sites[site_index];
                    if site.phase == EvalPhase::Parameter
                        && (kind == EvalBindingKind::Lexical
                            || (kind == EvalBindingKind::Var && binding_unit == unit_index))
                    {
                        continue;
                    }
                    plans.units[unit_index].direct_eval_sites[site_index]
                        .caller
                        .bindings
                        .push((name, kind));
                }
            }
            let owner_strict = plans.units[owner_index].is_strict;
            let implicit = owner_index != 0
                && owner_implicit_arguments
                && (owner_non_simple || owner_needs_arguments)
                && !plans.units[unit_index].direct_eval_sites[site_index]
                    .caller
                    .bindings
                    .iter()
                    .any(|(name, _)| name == "arguments");
            if implicit {
                plans.units[unit_index].direct_eval_sites[site_index]
                    .caller
                    .bindings
                    .push(("arguments".to_string(), EvalBindingKind::ImplicitArguments));
            }
            plans.units[unit_index].direct_eval_sites[site_index]
                .caller
                .arguments_object_needed = implicit;
            plans.units[unit_index].direct_eval_sites[site_index]
                .caller
                .strict = strict || owner_strict;
        }
    }
}

/// The symbol of a binding pattern that is a **bare name**, or `None` for any
/// pattern that introduces more than one binding or destructures.
///
/// `None` is a real answer here, not a resolution failure, and its one
/// remaining caller depends on it: `register_formals` (`:1858`) reads
/// `formal_idents` positionally against the incoming formal window, where a
/// `None` means "this formal is a pattern — leave the incoming register as
/// scratch for the prologue destructure, and let `decl_order` place the
/// leaves" (`finalize` step 1 at `:2748` skips on it, step 3 at `:2783` picks
/// the leaves up). Turning that call site into `binding_symbols` would move
/// a destructuring formal's leaves into the reserved window and double-allocate
/// them.
///
/// For "every symbol this pattern introduces", use [`binding_symbols`]. The
/// asymmetry between the two is what silently dropped the exports of
/// `export const [a] = [1]` and the const-ness of `for (const [y] of xs)`;
/// both now use `binding_symbols`.
fn binding_symbol(p: &oxc_ast::ast::BindingPattern<'_>) -> Option<SymbolId> {
    match p {
        oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.symbol_id.get(),
        // Not "cannot resolve": these three *are* resolved, they just bind
        // more than one name (or none, for a bare rest/default). Named rather
        // than wildcarded so a new `BindingPattern` variant cannot be added
        // without deciding which of the two answers it is.
        oxc_ast::ast::BindingPattern::ObjectPattern(_)
        | oxc_ast::ast::BindingPattern::ArrayPattern(_)
        | oxc_ast::ast::BindingPattern::AssignmentPattern(_) => None,
    }
}

pub(crate) fn binding_symbols(pattern: &BindingPattern<'_>) -> Vec<SymbolId> {
    match pattern {
        BindingPattern::BindingIdentifier(id) => id.symbol_id.get().into_iter().collect(),
        BindingPattern::ObjectPattern(object) => object
            .properties
            .iter()
            .flat_map(|property| binding_symbols(&property.value))
            .chain(
                object
                    .rest
                    .iter()
                    .flat_map(|rest| binding_symbols(&rest.argument)),
            )
            .collect(),
        BindingPattern::ArrayPattern(array) => array
            .elements
            .iter()
            .flatten()
            .flat_map(binding_symbols)
            .chain(
                array
                    .rest
                    .iter()
                    .flat_map(|rest| binding_symbols(&rest.argument)),
            )
            .collect(),
        BindingPattern::AssignmentPattern(assignment) => binding_symbols(&assignment.left),
    }
}

fn binding_names(pattern: &BindingPattern<'_>) -> Vec<String> {
    match pattern {
        BindingPattern::BindingIdentifier(id) => vec![id.name.to_string()],
        BindingPattern::ObjectPattern(object) => object
            .properties
            .iter()
            .flat_map(|property| binding_names(&property.value))
            .chain(
                object
                    .rest
                    .iter()
                    .flat_map(|rest| binding_names(&rest.argument)),
            )
            .collect(),
        BindingPattern::ArrayPattern(array) => array
            .elements
            .iter()
            .flatten()
            .flat_map(binding_names)
            .chain(
                array
                    .rest
                    .iter()
                    .flat_map(|rest| binding_names(&rest.argument)),
            )
            .collect(),
        BindingPattern::AssignmentPattern(assignment) => binding_names(&assignment.left),
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

fn resolve_block_functions(plans: &mut Plans, scoping: &Scoping) {
    for unit in &mut plans.units {
        let function_scope = unit.function_scope;
        let block_pairs: Vec<(ScopeId, SymbolId)> = unit
            .block_functions
            .iter()
            .map(|plan| (plan.declaration_scope, plan.symbol))
            .collect();
        for plan in &mut unit.block_functions {
            let name = scoping.symbol_name(plan.symbol);
            let nested_collision = block_pairs.iter().any(|(scope, symbol)| {
                *symbol != plan.symbol
                    && scoping.symbol_name(*symbol) == name
                    && Some(*scope) != function_scope
                    && scoping.scope_is_descendant_of(plan.declaration_scope, *scope)
            });
            let function_binding =
                function_scope.and_then(|scope| scoping.get_binding(scope, name.into()));
            let parameter_collision = (name == "arguments" && unit.implicit_arguments)
                || function_scope
                    .and_then(|scope| scoping.get_binding(scope, name.into()))
                    .is_some_and(|symbol| unit.parameter_symbols.contains(&symbol));
            let declaration_collision = scoping
                .get_binding(plan.declaration_scope, name.into())
                .is_some_and(|symbol| {
                    symbol != plan.symbol && unit.lexical_symbols.contains(&symbol)
                });
            let hoisted_without_var =
                function_binding.is_some_and(|symbol| unit.lexical_symbols.contains(&symbol));
            if name == "arguments" && unit.implicit_arguments {
                unit.needs_arguments = true;
            }
            plan.var_symbol = function_binding.or(Some(plan.symbol));
            plan.skip = if parameter_collision {
                BlockFunctionSkip::ParameterCollision
            } else if declaration_collision || hoisted_without_var || nested_collision {
                BlockFunctionSkip::LexicalCollision
            } else {
                BlockFunctionSkip::Eligible
            };
        }
    }
}

/// Builds the module-main seed environment plan after the AST walk has
/// classified imports/exports and declared their symbols.
fn build_module_binding_plans(plans: &mut Plans, scoping: &Scoping) {
    for import in plans.imports.clone() {
        let Some(symbol) = import.local else {
            continue;
        };
        let name = scoping.symbol_name(symbol).to_string();
        plans.module_bindings.push(ModuleBindingPlan {
            symbol: Some(symbol),
            name,
            env_slot: 0,
            source: ModuleBindingSource::Import {
                specifier: import.specifier,
                imported: import.imported,
                deferred: import.deferred,
            },
            exports: Vec::new(),
            initially_initialized: true,
        });
    }

    for export in plans.exports.clone() {
        if let Some(specifier) = export.specifier.clone() {
            if let Some(imported) = export.imported.clone() {
                plans.module_bindings.push(ModuleBindingPlan {
                    symbol: None,
                    name: export.exported.clone(),
                    env_slot: 0,
                    source: ModuleBindingSource::ReExport {
                        specifier,
                        imported,
                    },
                    exports: vec![export.exported],
                    initially_initialized: true,
                });
            }
            continue;
        }
        if let Some(symbol) = export.local {
            let name = scoping.symbol_name(symbol).to_string();
            let plan = plans
                .module_bindings
                .iter_mut()
                .find(|plan| plan.symbol == Some(symbol));
            if let Some(plan) = plan {
                if !plan.exports.contains(&export.exported) {
                    plan.exports.push(export.exported);
                }
            } else {
                plans.module_bindings.push(ModuleBindingPlan {
                    symbol: Some(symbol),
                    name,
                    env_slot: 0,
                    source: ModuleBindingSource::Local,
                    exports: vec![export.exported],
                    initially_initialized: plans.units[0].var_symbols.contains(&symbol),
                });
            }
        } else if export.exported == "default"
            && !plans
                .module_bindings
                .iter()
                .any(|plan| plan.source == ModuleBindingSource::DefaultExpression)
        {
            plans.module_bindings.push(ModuleBindingPlan {
                symbol: None,
                name: "default".to_string(),
                env_slot: 0,
                source: ModuleBindingSource::DefaultExpression,
                exports: vec!["default".to_string()],
                initially_initialized: false,
            });
        }
    }

    for (slot, plan) in plans.module_bindings.iter_mut().enumerate() {
        plan.env_slot = u16::try_from(slot).expect("module binding slots fit u16");
    }
    plans.units[0].has_env = true;
}

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
        let (escapes_here, needs_this, eval_reachable) = {
            let unit = &plans.units[ui];
            let escapes = unit
                .decl_order
                .iter()
                .any(|s| plans.captured.contains(s) && homes.get(s).is_some_and(|h| *h == ui));
            (escapes, unit.needs_this, unit.eval_reachable)
        };
        {
            let unit = &mut plans.units[ui];
            unit.has_env = escapes_here || needs_this || eval_reachable;
        }

        let mut slot: u16 = if ui == 0 && plans.is_module {
            plans.units[ui].has_env = true;
            for plan in &plans.module_bindings {
                if let Some(sym) = plan.symbol {
                    plans.units[ui].env_slots.insert(sym, plan.env_slot);
                    plans.units[ui].vars.insert(sym, VarLoc::Env(plan.env_slot));
                }
            }
            plans
                .module_bindings
                .iter()
                .map(|plan| plan.env_slot.checked_add(1))
                .collect::<Option<Vec<_>>>()
                .and_then(|slots| slots.into_iter().max())
                .unwrap_or(0)
        } else {
            0
        };
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
            if plans.captured.contains(&sym) || plans.units[ui].eval_reachable {
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
                if plans.captured.contains(&sym) || plans.units[ui].eval_reachable {
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
            // A `let`/`const` binding needs a parallel "initialized" cell so a
            // reference made before its declaration runs can be rejected (see
            // `UnitPlan::tdz_flags`). Only `lexical_symbols` qualifies:
            // `var`/formals are initialized when the environment record is
            // created and function declarations are initialized at scope
            // entry, so neither can be observed uninitialized. The flag is
            // allocated immediately after its value in the *same* storage
            // class, so a captured binding's flag is reachable from inner
            // functions through exactly the same environment chain.
            let needs_tdz_flag = plans.units[ui].lexical_symbols.contains(&sym);
            let flag = if plans.captured.contains(&sym) || plans.units[ui].eval_reachable {
                plans.units[ui].env_slots.insert(sym, slot);
                plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                slot = slot.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
                if needs_tdz_flag {
                    let flag = VarLoc::Env(slot);
                    slot = slot.checked_add(1).ok_or_else(|| CompileError {
                        message: "too many functions/constants".into(),
                        span: Some((0, 0)),
                    })?;
                    Some(flag)
                } else {
                    None
                }
            } else {
                plans.units[ui].vars.insert(sym, VarLoc::Reg(reg));
                reg = reg.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
                if needs_tdz_flag {
                    let flag = VarLoc::Reg(reg);
                    reg = reg.checked_add(1).ok_or_else(|| CompileError {
                        message: "too many functions/constants".into(),
                        span: Some((0, 0)),
                    })?;
                    Some(flag)
                } else {
                    None
                }
            };
            if let Some(flag) = flag {
                plans.units[ui].tdz_flags.insert(sym, flag);
            }
        }
        let eval_reachable = plans.units[ui].eval_reachable;
        let var_locations: Vec<Option<VarLoc>> = plans.units[ui]
            .block_functions
            .iter()
            .map(|plan| {
                plan.var_symbol
                    .and_then(|sym| plans.units[ui].vars.get(&sym).copied())
            })
            .collect();
        for (plan, var_location) in plans.units[ui]
            .block_functions
            .iter_mut()
            .zip(var_locations)
        {
            let captured = plans.captured.contains(&plan.symbol);
            plan.lexical_location = Some(if captured || eval_reachable {
                let location = VarLoc::Env(slot);
                slot = slot.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
                location
            } else {
                let location = VarLoc::Reg(reg);
                reg = reg.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
                location
            });
            if plan.skip == BlockFunctionSkip::Eligible {
                plan.var_location = var_location;
            }
        }
        let descriptors = {
            let unit = &plans.units[ui];
            unit.decl_order
                .iter()
                .filter_map(|sym| {
                    plans
                        .symbol_names
                        .get(sym)
                        .cloned()
                        .zip(unit.vars.get(sym).copied())
                })
                .collect::<Vec<_>>()
        };
        let unit = &mut plans.units[ui];
        unit.binding_descriptors = descriptors;
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
