//! Function-unit compilation: prologues (Environment creation, parameter
//! copies, `this` threading, self-reference binding), body dispatch, and
//! assembly into [`Compiler::functions`].

use oxc_ast::ast::{ArrowFunctionExpression, Function, FunctionType, Program};
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;
use v12_bytecode::{FunctionBytecode, Opcode};

use crate::model::{CompileError, Compiler, FnCtx, REG_THIS, VarLoc};

/// Which AST node a program function index was registered for.
pub enum UnitNode<'a> {
    Main(&'a Program<'a>),
    Fn(&'a Function<'a>),
    Arrow(&'a ArrowFunctionExpression<'a>),
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

    let mut cx = FnCtx::new(comp, idx);
    // Flag generator/async on the underlying FunctionBuilder before emission.
    match &node {
        UnitNode::Fn(f) => {
            cx.b.is_generator = f.generator;
            cx.b.is_async = f.r#async;
        }
        UnitNode::Arrow(a) => {
            cx.b.is_generator = false;
            cx.b.is_async = a.r#async;
        }
        UnitNode::Main(_) => {
            cx.b.is_generator = false;
            cx.b.is_async = false;
        }
    }
    emit_prologue(&mut cx, idx, self_symbol)?;
    if cx.b.is_generator {
        let dst = cx.new_temp();
        let func_idx = u16::try_from(idx).unwrap_or(0);
        cx.emit_reg3(Opcode::CreateGenerator, dst, func_idx, 0, oxc_span::Span::default());
    }
    if idx == 0 {
        emit_import_calls(&mut cx)?;
    }
    match node {
        UnitNode::Main(p) => cx.stmt_list(&p.body)?,
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
    }
    let mut fb = cx.finish()?;
    fb.name_hint = Some(comp.plans.units[idx].name_hint.clone());
    fb.is_strict = strict;
    let plan = &comp.plans.units[idx];
    fb.has_rest = plan.has_rest;
    fb.fixed_params = if plan.has_rest {
        plan.param_count.saturating_sub(1) as u16
    } else {
        plan.param_count as u16
    };
    fb.rest_reg = if plan.has_rest {
        fb.fixed_params + 1
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
    self_symbol: Option<SymbolId>,
) -> Result<(), CompileError> {
    let (has_env, env_slots, this_slot, param_count) = {
        let plan = &cx.comp.plans.units[cx.unit];
        (
            plan.has_env,
            plan.env_slot_count,
            plan.this_slot,
            plan.param_count,
        )
    };

    if has_env {
        let ancestor_envs = if cx.unit == 0 {
            0
        } else {
            cx.comp.plans.env_depth_between(cx.unit, 0)
        };
        cx.emit_new_env(ancestor_envs, env_slots, oxc_span::Span::default());

        for pi in 0..param_count {
            let Some(sym) = cx.comp.plans.units[cx.unit].decl_order.get(pi).copied() else {
                continue;
            };
            if let VarLoc::Env(slot) = cx.comp.plans.units[cx.unit].vars[&sym] {
                let incoming = pi as u16 + 1; // r0 is `this`
                cx.emit_set_env(0, slot, incoming, oxc_span::Span::default());
            }
        }
        if let Some(slot) = this_slot {
            cx.emit_set_env(0, slot, REG_THIS, oxc_span::Span::default());
        }
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
