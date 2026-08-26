//! Function-unit compilation: prologues (Environment creation, parameter
//! copies, `this` threading, self-reference binding), body dispatch, and
//! assembly into [`Compiler::functions`].

use oxc_ast::ast::{ArrowFunctionExpression, Function, FunctionType, Program};
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;
use v12_bytecode::{FunctionBytecode, Instr, Opcode};

use crate::model::{CompileError, Compiler, FnCtx, REG_THIS, VarLoc};

/// Which AST node a program function index was registered for.
pub enum UnitNode<'a> {
    Main(&'a Program<'a>),
    Fn(&'a Function<'a>),
    Arrow(&'a ArrowFunctionExpression<'a>),
}

fn placeholder(name_hint: Option<String>) -> FunctionBytecode {
    FunctionBytecode {
        name_hint,
        max_regs: 1,
        instrs: Vec::new(),
        consts: v12_bytecode::ConstantPool::new(),
        handlers: Vec::new(),
        spans: Vec::new(),
        pc_map: Vec::new(),
        is_strict: false,
    }
}

/// Compiles one function unit and stores it at `comp.functions[idx]`.
///
/// Nested units recurse through their parents' emitters ([`crate::expr`] →
/// [`FnCtx::closure_fn`]), so compilation is depth-first in discovery order,
/// matching how [`crate::collect`] assigned indices.
pub fn compile_unit(
    comp: &mut Compiler<'_, '_>,
    idx: usize,
    node: UnitNode<'_>,
) -> Result<(), CompileError> {
    debug_assert_eq!(
        idx,
        comp.functions.len(),
        "units must reserve slots in order"
    );
    let hint = comp.plans.units[idx].name_hint.clone();
    let strict = comp.strict;
    comp.functions.push(placeholder(Some(hint)));

    // Named function *expressions* re-create themselves in their prologue so
    // their own name resolves inside every activation (self-recursion).
    let self_symbol = match &node {
        UnitNode::Fn(f) if f.r#type == FunctionType::FunctionExpression => {
            f.id.as_ref().and_then(|id| id.symbol_id.get())
        }
        _ => None,
    };

    let mut cx = FnCtx::new(comp, idx);
    emit_prologue(&mut cx, idx, self_symbol)?;
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
                cx.emit_spanned(Instr::new(Opcode::Return, v, 0, 0), expr.span());
            }
        },
    }
    let mut fb = cx.finish()?;
    fb.name_hint = Some(comp.plans.units[idx].name_hint.clone());
    fb.is_strict = strict;
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
        cx.emit_op(Opcode::NewEnvironment, ancestor_envs, env_slots, 0);

        for pi in 0..param_count {
            let Some(sym) = cx.comp.plans.units[cx.unit].decl_order.get(pi).copied() else {
                continue;
            };
            if let VarLoc::Env(slot) = cx.comp.plans.units[cx.unit].vars[&sym] {
                let incoming = pi as u8 + 1; // r0 is `this`
                cx.emit_set_env(0, slot, incoming, oxc_span::Span::default());
            }
        }
        if let Some(slot) = this_slot {
            cx.emit_set_env(0, slot, REG_THIS, oxc_span::Span::default());
        }
    }

    if let Some(sym) = self_symbol {
        let idx8 = u8::try_from(idx).map_err(|_| CompileError {
            message: "programs above 255 functions are not supported".into(),
            span: None,
        })?;
        let dst = cx.new_temp();
        cx.emit(Instr::new(Opcode::Closure, dst, idx8, 0));
        let access = cx.access(sym);
        cx.store_access(access, dst, oxc_span::Span::default());
    }
    Ok(())
}
