//! Function-unit compilation: prologues (Environment creation, parameter
//! copies, `this` threading, self-reference binding), body dispatch, and
//! assembly into [`Compiler::functions`].

use oxc_ast::ast::{
    ArrowFunctionExpression, BindingPattern, Class, FormalParameters, Function, FunctionType,
    MethodDefinitionKind, Program, PropertyKey,
};
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;
use v12_bytecode::{FunctionBytecode, Opcode};

use crate::model::{CompileError, Compiler, FnCtx, REG_THIS, VarLoc};

/// Which AST node a program function index was registered for.
pub enum UnitNode<'a> {
    Main(&'a Program<'a>),
    Fn(&'a Function<'a>),
    Arrow(&'a ArrowFunctionExpression<'a>),
    /// The constructor unit of a class: the explicit `constructor` body, or a
    /// default body when the class has none.
    Class(&'a Class<'a>),
    /// A non-constructor class method.
    Method(&'a Function<'a>),
}

fn placeholder(name_hint: Option<String>) -> FunctionBytecode {
    let mut fb = FunctionBytecode::with_instructions(Vec::new(), 1);
    fb.name_hint = name_hint;
    fb
}

/// Reserved native index for synchronous module loading.
///
/// Calling convention (module linkage):
/// ```text
///   Closure rC, #NATIVE_IMPORT_INDEX   // rC = native function object for import
///   Move    r{C+1}, undef               // this = undefined
///   LoadConst r{C+2}, k{specifier_id}   // arg0 = module specifier string
///   Call    rC, rC, argc=1              // rC = import(specifier)
/// ```
/// One `Call` is emitted per distinct specifier at the start of the main
/// function (unit 0). The call returns the module namespace object; per-
/// binding `GetProperty` + `Store` sequences then materialize individual
/// imported bindings. When no `ImportCall` opcode exists, this `Closure` +
/// `Call` pair is the portable lowering. The native at this index is
/// provided by `v12-engine`'s `NativeRegistry`.
pub use crate::model::NATIVE_IMPORT_INDEX;

/// Compiles one function unit and stores it at `comp.functions[idx]`.
pub fn compile_unit(
    comp: &mut Compiler<'_, '_>,
    idx: usize,
    node: UnitNode<'_>,
) -> Result<(), CompileError> {
    // Reservation is lenient: `collect` assigns indices depth-first while
    // `stmt::stmt_list` hoists function declarations. A hoisted declaration
    // is emitted before earlier arrow initializers that share the same
    // statement list, so `idx` can arrive out of order (e.g. `let a = () =>
    // 1; function f(){}; let b = () => 2;` gives plans [main, a, f, b] but
    // hoisting compiles `f` (idx 2) before `a` (idx 1)). Gaps are filled
    // with placeholders so later `idx` values land on their intended slots.
    if idx < comp.functions.len() {
        // Placeholder already reserved by an earlier gap-fill.
    } else {
        while comp.functions.len() < idx {
            let fill = comp.functions.len();
            let hint = comp.plans.units[fill].name_hint.clone();
            comp.functions.push(placeholder(Some(hint)));
        }
        let hint = comp.plans.units[idx].name_hint.clone();
        comp.functions.push(placeholder(Some(hint)));
    }
    let strict = comp.plans.units[idx].is_strict;

    // Named function *expressions* re-create themselves in their prologue so
    // their own name resolves inside every activation (self-recursion).
    let self_symbol = match &node {
        UnitNode::Fn(f) if f.r#type == FunctionType::FunctionExpression => {
            f.id.as_ref().and_then(|id| id.symbol_id.get())
        }
        _ => None,
    };

    // Top-level `var` bindings alias the global object; their names are
    // interned up front (needs `&mut comp.strings`, so it happens before the
    // `FnCtx` borrows `comp`) and the slots are declared `undefined` in the
    // main unit's prologue. Keeps reads of declared-but-unassigned globals
    // `undefined` now that missing global reads throw `ReferenceError`.
    let global_var_init_ids: Vec<u32> = if idx == 0 && !comp.plans.is_module {
        let mut ids = Vec::new();
        for sym in comp.plans.units[idx].decl_order.clone() {
            if comp.plans.units[idx].vars.get(&sym) != Some(&VarLoc::Global) {
                continue;
            }
            let name = comp.scoping.symbol_name(sym).to_string();
            if v12_bytecode::GLOBAL_INTRINSICS.contains(&name.as_str()) {
                continue;
            }
            ids.push(crate::model::str_id_of(comp.strings.get_or_intern(&name)));
        }
        ids
    } else {
        Vec::new()
    };

    let mut cx = FnCtx::new(comp, idx);
    // Flag generator/async on the underlying FunctionBuilder before emission.
    match &node {
        UnitNode::Fn(f) => {
            cx.b.is_generator = f.generator;
            cx.b.is_async = f.r#async;
            cx.b.is_arrow = false;
        }
        UnitNode::Arrow(a) => {
            cx.b.is_generator = false;
            cx.b.is_async = a.r#async;
            cx.b.is_arrow = true;
        }
        UnitNode::Main(_) => {
            cx.b.is_generator = false;
            cx.b.is_async = false;
            cx.b.is_arrow = false;
        }
        UnitNode::Class(c) => {
            // The class constructor unit. If an explicit `constructor` exists,
            // its function is the unit's body source; the collect pass stored
            // its span as the unit's span. The unit itself is a plain
            // constructible function (never generator/async/arrow).
            cx.b.is_generator = false;
            cx.b.is_async = false;
            cx.b.is_arrow = false;
            let _ = c;
        }
        UnitNode::Method(f) => {
            cx.b.is_generator = f.generator;
            cx.b.is_async = f.r#async;
            cx.b.is_arrow = false;
        }
    }
    let params: Option<&FormalParameters<'_>> = match &node {
        UnitNode::Fn(f) => Some(&f.params),
        UnitNode::Arrow(a) => Some(&a.params),
        UnitNode::Method(f) => Some(&f.params),
        UnitNode::Class(c) => c.body.body.iter().find_map(|el| match el {
            oxc_ast::ast::ClassElement::MethodDefinition(m)
                if m.kind == MethodDefinitionKind::Constructor =>
            {
                Some(&*m.value.params)
            }
            _ => None,
        }),
        UnitNode::Main(_) => None,
    };
    emit_prologue(&mut cx, idx, params, self_symbol)?;
    if cx.b.is_generator {
        let dst = cx.new_temp();
        let func_idx = u16::try_from(idx).map_err(|_| CompileError {
            message: "programs above 65535 functions are not supported".into(),
            span: None,
        })?;
        cx.emit_reg3(
            Opcode::CreateGenerator,
            dst,
            func_idx,
            0,
            oxc_span::Span::default(),
        );
    }
    if idx == 0 {
        emit_import_calls(&mut cx)?;
    }
    // Anonymous classes name their constructor `""` (ClassDefinitionEvaluation
    // defaults `className` to the empty string); captured before `node` is
    // moved by the body match below.
    let anonymous_class = matches!(&node, UnitNode::Class(c) if c.id.is_none());
    match node {
        UnitNode::Main(p) => {
            // Declare top-level `var` slots on the global object (names were
            // gathered before the `FnCtx` borrow); hoisted function/class
            // stores below override these in order.
            for gid in &global_var_init_ids {
                let tmp = cx.new_temp();
                cx.load_undefined(tmp, oxc_span::Span::default());
                cx.emit_set_global(*gid, tmp, oxc_span::Span::default());
            }
            // Directive prologue (`p.directives`) holds leading string
            // literals separately from `p.body`; without this they are
            // dropped and a lone-string eval (`eval("'...'")`) completes
            // with `undefined` instead of the string. Re-emit each
            // directive as a string load so completion values survive.
            for d in &p.directives {
                let dst = cx.new_temp();
                cx.load_str(dst, d.expression.value.as_str(), d.span())?;
                cx.last_expr_reg = Some(dst);
            }
            cx.stmt_list(&p.body)?;
            let is_module = cx.comp.plans.is_module;
            if is_module {
                // Module completion: the exports object. The engine's loader
                // takes the module main's completion value as the namespace
                // snapshot, so the epilogue materializes one object carrying
                // every exported binding and returns it.
                emit_exports_epilogue(&mut cx)?;
            } else if let Some(reg) = cx.last_expr_reg {
                // Spec-compliant script completion: the value of the
                // last expression statement is the script's completion. Emit it
                // as an explicit `Return` so the interpreter's bottom-frame
                // completion captures it (`eval("1+1")` → 2).
                cx.emit_reg1(
                    Opcode::Return,
                    reg,
                    p.body.last().map(|s| s.span()).unwrap_or_default(),
                );
            }
        }
        UnitNode::Fn(f) => {
            let Some(body) = f.body.as_deref() else {
                return Err(cx.err(
                    f.span(),
                    "function declarations without a body are not supported",
                ));
            };
            cx.stmt_list(&body.statements)?;
        }
        UnitNode::Arrow(a) => match a.get_function_body() {
            Some(body) => cx.stmt_list(&body.statements)?,
            None => {
                let Some(expr) = a.get_expression() else {
                    return Err(cx.err(a.span(), "internal: arrow body missing"));
                };
                let v = cx.expr(expr)?;
                cx.emit_reg1(Opcode::Return, v, expr.span());
            }
        },
        UnitNode::Class(c) => {
            // Base-class instance fields initialize on `this` at the top of the
            // constructor, before the body. Derived classes must wait until
            // after `super()`; that ordering is not modeled yet, so derived
            // fields are skipped rather than initialized too early.
            if c.heritage.is_none() {
                for el in &c.body.body {
                    let oxc_ast::ast::ClassElement::PropertyDefinition(p) = el else {
                        continue;
                    };
                    if p.r#static {
                        continue;
                    }
                    if matches!(&p.key, PropertyKey::PrivateIdentifier(_)) {
                        continue;
                    }
                    let Some(value) = &p.value else {
                        continue;
                    };
                    crate::class::apply_field_function_name(&mut cx, &p.key, p.computed, value);
                    let value_reg = cx.expr(value)?;
                    let key_reg =
                        crate::class::property_key_reg(&mut cx, &p.key, p.computed, p.span)?;
                    cx.emit_reg3(
                        Opcode::SetProperty,
                        crate::model::REG_THIS,
                        key_reg,
                        value_reg,
                        p.span,
                    );
                }
            }
            // Find the explicit `constructor` element; compile its body, or
            // emit a default empty constructor when absent.
            let ctor = c.body.body.iter().find_map(|el| match el {
                oxc_ast::ast::ClassElement::MethodDefinition(m)
                    if m.kind == MethodDefinitionKind::Constructor =>
                {
                    Some(m)
                }
                _ => None,
            });
            if let Some(m) = ctor {
                let Some(body) = m.value.body.as_deref() else {
                    return Err(cx.err(m.span, "constructor without a body is not supported"));
                };
                cx.stmt_list(&body.statements)?;
            }
            // Default constructor: field initializers above, then `return undefined`.
        }
        UnitNode::Method(f) => {
            let Some(body) = f.body.as_deref() else {
                return Err(cx.err(f.span(), "class method without a body is not supported"));
            };
            cx.stmt_list(&body.statements)?;
        }
    }
    let mut fb = cx.finish()?;
    fb.name_hint = Some(comp.plans.units[idx].name_hint.clone());
    fb.function_name = comp.plans.units[idx].function_name.clone();
    // Anonymous classes name their constructor `""` (ClassDefinitionEvaluation
    // defaults `className` to the empty string); without this the function has
    // no own `name` property at all (`verifyProperty(class {}, "name", ...)`).
    if fb.function_name.is_none() && anonymous_class {
        fb.function_name = Some(String::new());
    }
    fb.is_strict = strict;
    let plan = &comp.plans.units[idx];
    fb.has_rest = plan.has_rest;
    fb.expected_args = u16::try_from(plan.expected_args).unwrap_or(u16::MAX);
    fb.needs_arguments = plan.needs_arguments;
    fb.fixed_params = plan.arity as u16;
    fb.rest_reg = if plan.has_rest {
        plan.arity as u16 + 1
    } else {
        0
    };
    comp.functions[idx] = fb;
    Ok(())
}

/// Entry code common to every unit.
///
/// - `NewEnvironment` when the unit owns a heap Environment (any local
///   escapes into an inner function, or an arrow-descendant reads `this`).
///   Fresh environment slots read as `undefined`: the interpreter allocates
///   them filled, matching the register ABI.
/// - Captured *parameters* are copied from their incoming registers into the
///   environment (parameter `i` arrives in `r{i+1}` by the call ABI).
/// - `this` is threaded into the environment when arrow-descendants read it.
fn emit_prologue(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    idx: usize,
    params: Option<&FormalParameters<'_>>,
    self_symbol: Option<SymbolId>,
) -> Result<(), CompileError> {
    let (has_env, env_slots, this_slot, arity) = {
        let plan = &cx.comp.plans.units[cx.unit];
        (
            plan.has_env,
            plan.env_slot_count,
            plan.this_slot,
            plan.arity,
        )
    };

    if has_env {
        let ancestor_envs = if cx.unit == 0 {
            0
        } else {
            cx.comp.plans.env_depth_between(cx.unit, 0)
        };
        cx.emit_new_env(ancestor_envs, env_slots, oxc_span::Span::default());
    }

    if let Some(ps) = params {
        // Formal `i` arrives in `r{i+1}` (`r0` is `this`). A simple identifier
        // is already in place unless captured (then copy into the env); a
        // pattern runs the shared destructuring lowerer against its incoming
        // register.
        for (i, p) in ps.items.iter().enumerate() {
            let incoming = i as u16 + 1;
            // oxc stores a top-level default (`a = 1`, `[a] = []`) on the
            // `FormalParameter`; the binding pattern stays bare. Apply the
            // default first, then bind the (possibly destructuring) pattern
            // against the chosen value.
            if let Some(init) = &p.initializer {
                let chosen = cx.lower_default(incoming, init, p.span)?;
                match &p.pattern {
                    BindingPattern::BindingIdentifier(id) => {
                        if let Some(sym) = id.symbol_id.get() {
                            let access = cx.access(sym);
                            cx.store_access(access, chosen, p.span);
                        }
                    }
                    pat => cx.lower_binding_pattern(pat, chosen)?,
                }
                continue;
            }
            let loc = match &p.pattern {
                BindingPattern::BindingIdentifier(id) => id
                    .symbol_id
                    .get()
                    .and_then(|sym| cx.comp.plans.units[cx.unit].vars.get(&sym).copied()),
                _ => {
                    cx.lower_binding_pattern(&p.pattern, incoming)?;
                    None
                }
            };
            if let Some(VarLoc::Env(slot)) = loc {
                cx.emit_set_env(0, slot, incoming, oxc_span::Span::default());
            }
        }

        if let Some(rest) = &ps.rest {
            let rest_reg = arity as u16 + 1;
            let loc = match &rest.rest.argument {
                BindingPattern::BindingIdentifier(id) => id
                    .symbol_id
                    .get()
                    .and_then(|sym| cx.comp.plans.units[cx.unit].vars.get(&sym).copied()),
                pattern => {
                    cx.lower_binding_pattern(pattern, rest_reg)?;
                    None
                }
            };
            if let Some(VarLoc::Env(slot)) = loc {
                cx.emit_set_env(0, slot, rest_reg, oxc_span::Span::default());
            }
        }
    }

    if let Some(slot) = this_slot {
        cx.emit_set_env(0, slot, REG_THIS, oxc_span::Span::default());
    }

    if let Some(sym) = self_symbol {
        let idx16 = u16::try_from(idx).map_err(|_| CompileError {
            message: "programs above 65535 functions are not supported".into(),
            span: None,
        })?;
        let dst = cx.new_temp();
        cx.emit_closure(dst, idx16, oxc_span::Span::default());
        let access = cx.access(sym);
        cx.store_access(access, dst, oxc_span::Span::default());
    }
    Ok(())
}

/// Emits the module epilogue: materialize the exports object and return it.
///
/// Named exports read their local binding (register/env/global access —
/// whatever `collect` assigned); `export default <expr>` reads the hidden
/// capture slot written by the declaration lowering
/// ([`crate::model::DEFAULT_EXPORT_GLOBAL`]); re-exports (`export ... from`)
/// are skipped (their linkage is not modeled yet). The returned object is the
/// module's completion value and doubles as its namespace snapshot in the
/// engine's loader.
fn emit_exports_epilogue(cx: &mut FnCtx<'_, '_, '_, '_>) -> Result<(), CompileError> {
    let exports = cx.comp.plans.exports.clone();
    let span = oxc_span::Span::default();
    let obj = cx.new_temp();
    cx.emit_reg3(Opcode::NewObject, obj, 0, 0, span);
    for e in &exports {
        if e.specifier.is_some() {
            continue;
        }
        let value = if let Some(sym) = e.local {
            let access = cx.access(sym);
            let dst = cx.new_temp();
            cx.read_access(access, dst, span);
            dst
        } else if e.exported == "default" {
            let name_id = crate::model::str_id_of(
                cx.comp
                    .strings
                    .get_or_intern(crate::model::DEFAULT_EXPORT_GLOBAL),
            );
            let dst = cx.new_temp();
            cx.emit_get_global(dst, name_id, span);
            dst
        } else {
            continue;
        };
        let key = cx.load_str_key(&e.exported, span)?;
        cx.emit_reg3(Opcode::SetProperty, obj, key, value, span);
    }
    cx.emit_reg1(Opcode::Return, obj, span);
    Ok(())
}

fn emit_import_calls(cx: &mut FnCtx<'_, '_, '_, '_>) -> Result<(), CompileError> {
    use std::collections::{HashMap, HashSet};

    if cx.comp.plans.imports.is_empty() {
        return Ok(());
    }
    // Group imported bindings by specifier, dedup specifiers for the actual
    // `import(specifier)` native call, then wire each binding via a
    // `GetProperty` from the returned namespace object.
    let mut by_spec: HashMap<String, Vec<crate::model::ImportEntry>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut seen_spec: HashSet<String> = HashSet::new();
    for e in &cx.comp.plans.imports {
        let s = e.specifier.clone();
        if seen_spec.insert(s.clone()) {
            order.push(s.clone());
        }
        by_spec.entry(s).or_default().push(e.clone());
    }

    for spec in order {
        let entries = &by_spec[&spec];
        // Representative span for the call site; use first entry's span or
        // default when side-effect only.
        let span = entries
            .first()
            .and_then(|e| e.span)
            .map(|(s, e)| oxc_span::Span::new(s, e))
            .unwrap_or_default();

        // Call native import helper: Closure + Call with one string arg.
        // Layout: [callee][this][arg] -> Call rC, rC, argc=1
        let block = cx.new_temps(crate::model::CALL_HEADER_REGS + 1);
        let callee = block;
        cx.emit_closure(callee, NATIVE_IMPORT_INDEX, span);
        cx.load_undefined(callee + 1, span);
        cx.load_str(callee + 2, &spec, span)?;
        cx.emit_call(callee, callee, 1, span);
        let ns_reg = callee;

        // Wire named imports: `local = ns[imported]`. Side-effect imports
        // (local == None) produce no wiring; the call's side effect is the
        // whole effect.
        for e in entries {
            let Some(local) = e.local else { continue };
            if e.imported == "*" {
                // `import * as ns from` : whole namespace object.
                let access = cx.access(local);
                cx.store_access(access, ns_reg, span);
            } else if e.imported.is_empty() {
                continue;
            } else {
                let key = cx.new_temp();
                cx.load_str(key, &e.imported, span)?;
                let dst = cx.new_temp();
                cx.emit_reg3(Opcode::GetProperty, dst, ns_reg, key, span);
                let access = cx.access(local);
                cx.store_access(access, dst, span);
            }
        }
    }
    Ok(())
}
