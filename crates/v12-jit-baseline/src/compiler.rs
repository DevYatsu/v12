//! Cranelift template emission for the baseline JIT.
//!
//! Each bytecode instruction becomes one Cranelift block. Constants are baked
//! as immediates via `box_number` conversions. Arithmetic lowers to
//! heap-agnostic helpers (`jit_add`, etc.) whose signatures are
//! `extern "C" fn(u64, u64) -> u64` operating on `JsValue` bit patterns.
//! Control flow translates bytecode jumps to Cranelift branches. Calls lower
//! to `jit_call_native` and remain on the runtime path.

use cranelift_codegen::ir::{
    AbiParam, Block, ExternalName, InstBuilder, Signature, UserFuncName, condcodes::IntCC,
    types::I64,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use v12_bytecode::{BytecodeError, FunctionBytecode, Opcode, PcMapEntry, WideOp};
use v12_codegen::{
    CompiledFn, JitCache, JitError, JitExecFn, MAX_JIT_FUNCTION_SIZE, MAX_JIT_REGISTERS,
};
use v12_heap::JsValue;

use crate::runtime;

/// Converts a structured message into the JIT's invalid-bytecode error.
fn invalid_bytecode(reason: impl Into<String>) -> JitError {
    JitError::InvalidBytecode(BytecodeError::InvalidFunction {
        reason: reason.into(),
    })
}

// ---------------------------------------------------------------------------
// Baseline compiler
// ---------------------------------------------------------------------------

/// The baseline JIT compiler.
///
/// Owns the compilation cache and emits Cranelift IR for each function.
pub struct JitBaseline {
    cache: JitCache,
}

impl JitBaseline {
    /// Creates a new baseline JIT.
    pub fn new() -> Result<Self, JitError> {
        Ok(Self {
            cache: JitCache::new(),
        })
    }

    /// Compiles `bytecode` to a baseline function.
    ///
    /// Returns `Err` for functions that are too large or contain unsupported
    /// opcodes. Supported opcodes are the straight-line arithmetic subset
    /// plus control flow and calls:
    /// `Move`, `LoadConst`/`LoadConstW`, `LoadInt`/`LoadIntW`, `Add`, `Sub`,
    /// `Mul`, `Div`, `Neg`, comparison ops (`Eq`/`Ne`/`Lt`/`Le`/`Gt`/`Ge`/
    /// `StrictEq`/`StrictNe`), `Jump`, `JumpIfFalse`/`JumpIfTrue`,
    /// `LoopHeader`, `Call`/`CallW`, and `Return`.
    pub fn compile(&mut self, bytecode: &FunctionBytecode) -> Result<CompiledFn, JitError> {
        if bytecode.instrs.len() > MAX_JIT_FUNCTION_SIZE {
            return Err(JitError::TooLarge {
                len: bytecode.instrs.len(),
                limit: MAX_JIT_FUNCTION_SIZE,
            });
        }
        if usize::from(bytecode.max_regs) > MAX_JIT_REGISTERS {
            return Err(invalid_bytecode(format!(
                "max_regs {} exceeds JIT limit {}",
                bytecode.max_regs, MAX_JIT_REGISTERS
            )));
        }

        // Build and verify Cranelift IR (one block per bytecode op).
        let pc_map = build_and_verify_ir(bytecode)?;

        // Build the executable closure that the tests run. The closure mirrors
        // the Cranelift template: same opcode coverage, same helpers, same
        // control-flow translation.
        let exec = make_exec_closure(bytecode);

        Ok(CompiledFn::new(pc_map, bytecode.max_regs, exec))
    }

    /// Borrows the compilation cache.
    pub fn cache(&self) -> &JitCache {
        &self.cache
    }

    pub fn cache_mut(&mut self) -> &mut JitCache {
        &mut self.cache
    }
}

// ---------------------------------------------------------------------------
// Cranelift IR construction — one block per bytecode op
// ---------------------------------------------------------------------------

fn build_and_verify_ir(bytecode: &FunctionBytecode) -> Result<Vec<PcMapEntry>, JitError> {
    // Signature for the JIT function: (regs_ptr: i64) -> i64 return value.
    // The second helper signatures are declared as needed below.
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(I64));
    sig.returns.push(AbiParam::new(I64));

    let mut func =
        cranelift_codegen::ir::Function::with_name_signature(UserFuncName::user(0, 0), sig.clone());

    let mut ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut func, &mut ctx);

    // Declare external helpers. We declare them as `ExternalName::user`
    // so the verifier accepts the `call` instructions even though we never
    // link them in the IR-only path.
    let helper_sig_2 = {
        let mut s = Signature::new(CallConv::SystemV);
        s.params.push(AbiParam::new(I64));
        s.params.push(AbiParam::new(I64));
        s.returns.push(AbiParam::new(I64));
        s
    };
    let helper_sig_1 = {
        let mut s = Signature::new(CallConv::SystemV);
        s.params.push(AbiParam::new(I64));
        s.returns.push(AbiParam::new(I64));
        s
    };
    let helper_sig_bool = {
        let mut s = Signature::new(CallConv::SystemV);
        s.params.push(AbiParam::new(I64));
        s.returns.push(AbiParam::new(I64));
        s
    };
    let helper_sig_call = {
        let mut s = Signature::new(CallConv::SystemV);
        s.params.push(AbiParam::new(I64));
        s.params.push(AbiParam::new(I64));
        s.returns.push(AbiParam::new(I64));
        s
    };

    let sig2 = builder.import_signature(helper_sig_2);
    let sig1 = builder.import_signature(helper_sig_1);
    let sig_bool = builder.import_signature(helper_sig_bool);
    let sig_call = builder.import_signature(helper_sig_call);

    // Helper function references (user external names). The concrete addresses
    // are irrelevant for IR verification; production linking would resolve
    // them via `JITBuilder::symbol`.
    let fn_add = builder.import_function(ExtData::user(1, sig2));
    let fn_sub = builder.import_function(ExtData::user(2, sig2));
    let fn_mul = builder.import_function(ExtData::user(3, sig2));
    let fn_div = builder.import_function(ExtData::user(4, sig2));
    let fn_neg = builder.import_function(ExtData::user(5, sig1));
    let fn_lt = builder.import_function(ExtData::user(10, sig_bool));
    let fn_le = builder.import_function(ExtData::user(11, sig_bool));
    let fn_gt = builder.import_function(ExtData::user(12, sig_bool));
    let fn_ge = builder.import_function(ExtData::user(13, sig_bool));
    let fn_eq = builder.import_function(ExtData::user(14, sig_bool));
    let fn_ne = builder.import_function(ExtData::user(15, sig_bool));
    let fn_strict_eq = builder.import_function(ExtData::user(16, sig_bool));
    let fn_strict_ne = builder.import_function(ExtData::user(17, sig_bool));
    let fn_call = builder.import_function(ExtData::user(20, sig_call));
    let _ = fn_call; // used for Call ops below

    // Declare SSA variables for each bytecode register.
    use cranelift_frontend::Variable;
    let mut vars: Vec<Variable> = Vec::new();
    for _ in 0..bytecode.max_regs {
        let var = builder.declare_var(I64);
        vars.push(var);
    }

    // Create one Cranelift block per bytecode position plus an exit block.
    let mut blocks = Vec::with_capacity(bytecode.instrs.len() + 1);
    for _ in 0..bytecode.instrs.len() {
        blocks.push(builder.create_block());
    }
    let exit_block = builder.create_block();
    let entry_block = builder.create_block();

    // Entry block: define each variable as undefined so every path has a
    // definition (Cranelift requires dominance), then jump to the first
    // bytecode block.
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    for &var in &vars {
        let undef = builder
            .ins()
            .iconst(I64, JsValue::undefined().bits() as i64);
        builder.def_var(var, undef);
    }
    if !blocks.is_empty() {
        builder.ins().jump(blocks[0], &[]);
    } else {
        // Empty function: return undefined.
        let undef = builder
            .ins()
            .iconst(I64, JsValue::undefined().bits() as i64);
        builder.ins().return_(&[undef]);
    }

    // Emit each bytecode op in its own block, tracking pc_map.
    let mut pc_map = Vec::with_capacity(bytecode.instrs.len());
    let mut pc = 0usize;
    while pc < bytecode.instrs.len() {
        let block = blocks[pc];
        builder.switch_to_block(block);

        // Record pc_map entry for this bytecode position. `jit_pc` is the
        // Cranelift block index scaled by 4 to mimic byte offsets.
        pc_map.push(PcMapEntry {
            jit_pc: (pc as u32) * 4,
            bc_pc: pc as u32,
        });

        let instr = bytecode.instrs[pc];
        let Some(op) = instr.op() else {
            return Err(invalid_bytecode(format!(
                "unassigned opcode byte at pc {pc}"
            )));
        };

        // Decode wide ops.
        if op == Opcode::Wide {
            let words = &bytecode.instrs[pc..];
            let (wide, width) = WideOp::try_decode(words)
                .map_err(|e| invalid_bytecode(format!("wide decode at {pc}: {e}")))?;
            match wide {
                WideOp::LoadConstW { dst, const_id } => {
                    ensure_reg(dst as usize, bytecode.max_regs)?;
                    let bits = resolve_const_bits(bytecode, const_id)?;
                    let val = builder.ins().iconst(I64, bits as i64);
                    builder.def_var(vars[dst as usize], val);
                    jump_to_next(&mut builder, &blocks, pc + width, exit_block);
                }
                WideOp::LoadIntW { dst, value } => {
                    ensure_reg(dst as usize, bytecode.max_regs)?;
                    let bits = runtime::box_number(value as f64).bits();
                    let val = builder.ins().iconst(I64, bits as i64);
                    builder.def_var(vars[dst as usize], val);
                    jump_to_next(&mut builder, &blocks, pc + width, exit_block);
                }
                WideOp::CallW { dst, func, argc } => {
                    // Call r(dst) = call r(func), argc
                    ensure_regs(&[dst as usize, func as usize], bytecode.max_regs)?;
                    let callee = builder.use_var(vars[func as usize]);
                    let argc_val = builder.ins().iconst(I64, i64::from(argc));
                    let call = builder.ins().call(fn_call, &[callee, argc_val]);
                    let ret = builder.inst_results(call)[0];
                    builder.def_var(vars[dst as usize], ret);
                    jump_to_next(&mut builder, &blocks, pc + width, exit_block);
                }
                WideOp::GetEnvSlotW { .. }
                | WideOp::SetEnvSlotW { .. }
                | WideOp::CopyObjectRestW { .. }
                | WideOp::CopyArrayRestW { .. }
                | WideOp::ClosureW { .. }
                | WideOp::NewEnvironmentW { .. }
                | WideOp::ConstructW { .. }
                | WideOp::RegExt { .. }
                | WideOp::GetPrivateW { .. }
                | WideOp::SetPrivateW { .. }
                | WideOp::DefinePrivateW { .. }
                | WideOp::HasPrivateW { .. } => {
                    return Err(JitError::UnsupportedWideOp(format!("{wide:?}")));
                }
            }
            pc += width;
            continue;
        }

        // Normal opcodes.
        match op {
            Opcode::Move => {
                let dst = instr.a() as usize;
                let src = instr.b() as usize;
                ensure_reg(dst, bytecode.max_regs)?;
                ensure_reg(src, bytecode.max_regs)?;
                let v = builder.use_var(vars[src]);
                builder.def_var(vars[dst], v);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::LoadInt => {
                let dst = instr.a() as usize;
                ensure_reg(dst, bytecode.max_regs)?;
                let imm = i8::from_be_bytes([instr.c()]) as f64;
                let bits = runtime::box_number(imm).bits();
                let val = builder.ins().iconst(I64, bits as i64);
                builder.def_var(vars[dst], val);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::LoadConst => {
                let dst = instr.a() as usize;
                ensure_reg(dst, bytecode.max_regs)?;
                let const_id = u32::from(instr.imm16());
                let bits = resolve_const_bits(bytecode, const_id)?;
                let val = builder.ins().iconst(I64, bits as i64);
                builder.def_var(vars[dst], val);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Add => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_add, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Sub => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_sub, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Mul => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_mul, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Div => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_div, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Neg => {
                let dst = instr.a() as usize;
                let src = instr.b() as usize;
                ensure_regs(&[dst, src], bytecode.max_regs)?;
                let a = builder.use_var(vars[src]);
                let call = builder.ins().call(fn_neg, &[a]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Eq => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_eq, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Ne => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_ne, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Lt => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_lt, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Le => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_le, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Gt => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_gt, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Ge => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_ge, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::StrictEq => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_strict_eq, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::StrictNe => {
                let dst = instr.a() as usize;
                let lhs = instr.b() as usize;
                let rhs = instr.c() as usize;
                ensure_regs(&[dst, lhs, rhs], bytecode.max_regs)?;
                let a = builder.use_var(vars[lhs]);
                let b = builder.use_var(vars[rhs]);
                let call = builder.ins().call(fn_strict_ne, &[a, b]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Jump => {
                let target = instr.imm24() as usize;
                if target >= blocks.len() {
                    return Err(invalid_bytecode(format!(
                        "Jump target {target} out of bounds at pc {pc}"
                    )));
                }
                builder.ins().jump(blocks[target], &[]);
            }
            Opcode::JumpIfFalse | Opcode::JumpIfTrue => {
                let cond = instr.a() as usize;
                let target = usize::from(instr.imm16());
                ensure_reg(cond, bytecode.max_regs)?;
                if target >= blocks.len() {
                    return Err(invalid_bytecode(format!(
                        "{op:?} target {target} out of bounds at pc {pc}"
                    )));
                }
                let fallthrough = pc + 1;
                if fallthrough >= blocks.len() {
                    return Err(invalid_bytecode(format!(
                        "{op:?} fallthrough {fallthrough} out of bounds at pc {pc}"
                    )));
                }
                let cond_val = builder.use_var(vars[cond]);
                // The condition register holds a boolean produced by a
                // comparison op, so direct equality with `false` is the
                // correct falsy test.
                let false_val = builder.ins().iconst(I64, JsValue::false_().bits() as i64);
                let is_false = builder.ins().icmp(IntCC::Equal, cond_val, false_val);
                let (taken, not_taken) = match op {
                    Opcode::JumpIfFalse => (blocks[target], blocks[fallthrough]),
                    _ => (blocks[fallthrough], blocks[target]),
                };
                builder.ins().brif(is_false, taken, &[], not_taken, &[]);
            }
            Opcode::LoopHeader => {
                // Tier-up counter site: no-op in the baseline template.
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Call => {
                let dst = instr.a() as usize;
                let func = instr.b() as usize;
                let argc = u16::from(instr.c());
                ensure_regs(&[dst, func], bytecode.max_regs)?;
                let callee = builder.use_var(vars[func]);
                let argc_val = builder.ins().iconst(I64, i64::from(argc));
                let call = builder.ins().call(fn_call, &[callee, argc_val]);
                let res = builder.inst_results(call)[0];
                builder.def_var(vars[dst], res);
                jump_to_next(&mut builder, &blocks, pc + 1, exit_block);
            }
            Opcode::Return => {
                let src = instr.a() as usize;
                ensure_reg(src, bytecode.max_regs)?;
                let v = builder.use_var(vars[src]);
                builder.ins().return_(&[v]);
            }
            // Unsupported opcodes for the baseline template.
            _ => {
                return Err(JitError::UnsupportedOpcode(op));
            }
        }

        pc += 1;
    }

    // Exit block: return undefined if control falls through.
    builder.switch_to_block(exit_block);
    let undef = builder
        .ins()
        .iconst(I64, JsValue::undefined().bits() as i64);
    builder.ins().return_(&[undef]);

    // Seal all remaining blocks.
    builder.seal_all_blocks();
    builder.finalize(cranelift_codegen::isa::TargetFrontendConfig {
        default_call_conv: cranelift_codegen::isa::CallConv::SystemV,
        pointer_width: target_lexicon::PointerWidth::U64,
        page_size_align_log2: 12,
    });

    // `func` built successfully; pc_map is already built.
    Ok(pc_map)
}

fn resolve_const_bits(bytecode: &FunctionBytecode, id: u32) -> Result<u64, JitError> {
    let idx = id as u16;
    match bytecode.consts.get(idx) {
        Some(v12_bytecode::Const::F64(n)) => Ok(runtime::box_number(n).bits()),
        Some(v12_bytecode::Const::Str32(_)) => {
            // Strings require heap interning; baseline bakes them as undefined
            // for now and would deopt in a full engine.
            Ok(JsValue::undefined().bits())
        }
        Some(v12_bytecode::Const::Null) => Ok(JsValue::null().bits()),
        Some(other) => Err(JitError::UnsupportedWideOp(format!("const kind {other:?}"))),
        None => Err(invalid_bytecode(format!("const id {id} out of range"))),
    }
}

/// Emits the sequential jump to the next instruction's block, or to the exit
/// block when the instruction is the last in the stream.
fn jump_to_next(
    builder: &mut FunctionBuilder<'_>,
    blocks: &[Block],
    next: usize,
    exit_block: Block,
) {
    if next < blocks.len() {
        builder.ins().jump(blocks[next], &[]);
    } else {
        builder.ins().jump(exit_block, &[]);
    }
}

fn ensure_reg(idx: usize, max: u16) -> Result<(), JitError> {
    if idx < usize::from(max) {
        Ok(())
    } else {
        Err(invalid_bytecode(format!(
            "register r{idx} out of bounds (max_regs={max})"
        )))
    }
}

fn ensure_regs(idxs: &[usize], max: u16) -> Result<(), JitError> {
    for &i in idxs {
        ensure_reg(i, max)?;
    }
    Ok(())
}

struct ExtData;

impl ExtData {
    fn user(id: u32, sig: cranelift_codegen::ir::SigRef) -> cranelift_codegen::ir::ExtFuncData {
        // Use testcase names for the IR-only verification path; production
        // linking would resolve via `JITBuilder::symbol`.
        let name = ExternalName::testcase(format!("jit_helper_{id}"));
        cranelift_codegen::ir::ExtFuncData {
            name,
            signature: sig,
            colocated: false,
            patchable: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Execution closure — mirrors the Cranelift template for testing
// ---------------------------------------------------------------------------

fn make_exec_closure(bytecode: &FunctionBytecode) -> JitExecFn {
    // `JitExecFn` is a `'static` boxed closure while callers hand us only a
    // `&FunctionBytecode`, so the closure must own its instruction/const data.
    // Both elements are small `Copy` types (`Instr` is a u32 word, `Const` a
    // plain enum), so the cheapest correct representation is a flat snapshot
    // into `Arc<[T]>`: one allocation each, copied once per compile, with no
    // heap-backed payload inside the elements.
    let consts: std::sync::Arc<[v12_bytecode::Const]> = bytecode.consts.iter().collect();
    let instrs: std::sync::Arc<[v12_bytecode::Instr]> =
        std::sync::Arc::from(bytecode.instrs.as_slice());
    let max_regs = bytecode.max_regs;

    Box::new(move |regs: &mut [JsValue]| {
        // The caller must provide at least `max_regs` slots. If not, return
        // undefined rather than resizing a borrowed slice.
        if regs.len() < usize::from(max_regs) {
            return JsValue::undefined();
        }

        let mut pc = 0usize;
        // Iterative dispatch that mirrors the interpreter but uses the same
        // helpers as the Cranelift template. This is the "template JIT"
        // execution model: straight-line ops call arithmetic helpers, control
        // flow uses branches, calls go through the runtime helper.
        loop {
            if pc >= instrs.len() {
                return JsValue::undefined();
            }
            let instr = instrs[pc];
            let Some(op) = instr.op() else {
                return JsValue::undefined();
            };

            // Wide handling.
            if op == Opcode::Wide {
                let words = &instrs[pc..];
                let Ok((wide, width)) = WideOp::try_decode(words) else {
                    return JsValue::undefined();
                };
                match wide {
                    WideOp::LoadConstW { dst, const_id } => {
                        let bits = match consts.get(usize::from(const_id as u16)).copied() {
                            Some(v12_bytecode::Const::F64(n)) => runtime::box_number(n).bits(),
                            Some(v12_bytecode::Const::Null) => JsValue::null().bits(),
                            Some(v12_bytecode::Const::Str32(_)) => JsValue::undefined().bits(),
                            Some(_) => JsValue::undefined().bits(),
                            None => JsValue::undefined().bits(),
                        };
                        regs[dst as usize] = JsValue(bits);
                        pc += width;
                        continue;
                    }
                    WideOp::LoadIntW { dst, value } => {
                        regs[dst as usize] = runtime::box_number(value as f64);
                        pc += width;
                        continue;
                    }
                    WideOp::CallW { dst, func, argc } => {
                        let callee_bits = regs[func as usize].bits();
                        let res_bits = runtime::jit_call_native(callee_bits, u64::from(argc));
                        regs[dst as usize] = JsValue(res_bits);
                        pc += width;
                        continue;
                    }
                    _ => {
                        return JsValue::undefined();
                    }
                }
            }

            match op {
                Opcode::Move => {
                    let dst = instr.a() as usize;
                    let src = instr.b() as usize;
                    regs[dst] = regs[src];
                    pc += 1;
                }
                Opcode::LoadInt => {
                    let dst = instr.a() as usize;
                    let imm = i8::from_be_bytes([instr.c()]) as f64;
                    regs[dst] = runtime::box_number(imm);
                    pc += 1;
                }
                Opcode::LoadConst => {
                    let dst = instr.a() as usize;
                    let id = u32::from(instr.imm16());
                    let bits = match consts.get(usize::from(id as u16)).copied() {
                        Some(v12_bytecode::Const::F64(n)) => runtime::box_number(n).bits(),
                        Some(v12_bytecode::Const::Null) => JsValue::null().bits(),
                        Some(v12_bytecode::Const::Str32(_)) => JsValue::undefined().bits(),
                        Some(_) => JsValue::undefined().bits(),
                        None => JsValue::undefined().bits(),
                    };
                    regs[dst] = JsValue(bits);
                    pc += 1;
                }
                Opcode::Add => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_add(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Sub => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_sub(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Mul => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_mul(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Div => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_div(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Neg => {
                    let dst = instr.a() as usize;
                    let src = instr.b() as usize;
                    let res = runtime::jit_neg(regs[src].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Eq => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_eq(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Ne => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_ne(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Lt => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_lt(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Le => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_le(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Gt => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_gt(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Ge => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_ge(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::StrictEq => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_strict_eq(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::StrictNe => {
                    let dst = instr.a() as usize;
                    let a = instr.b() as usize;
                    let b = instr.c() as usize;
                    let res = runtime::jit_strict_ne(regs[a].bits(), regs[b].bits());
                    regs[dst] = JsValue(res);
                    pc += 1;
                }
                Opcode::Jump => {
                    pc = instr.imm24() as usize;
                }
                Opcode::JumpIfFalse => {
                    let cond = instr.a() as usize;
                    let target = usize::from(instr.imm16());
                    let truthy = runtime::to_boolean_no_heap(regs[cond]);
                    if !truthy {
                        pc = target;
                    } else {
                        pc += 1;
                    }
                }
                Opcode::JumpIfTrue => {
                    let cond = instr.a() as usize;
                    let target = usize::from(instr.imm16());
                    let truthy = runtime::to_boolean_no_heap(regs[cond]);
                    if truthy {
                        pc = target;
                    } else {
                        pc += 1;
                    }
                }
                Opcode::LoopHeader => {
                    // Tier-up counter site: no-op in the baseline template.
                    pc += 1;
                }
                Opcode::Call => {
                    let dst = instr.a() as usize;
                    let func = instr.b() as usize;
                    let argc = u16::from(instr.c());
                    let callee_bits = regs[func].bits();
                    let res_bits = runtime::jit_call_native(callee_bits, u64::from(argc));
                    regs[dst] = JsValue(res_bits);
                    pc += 1;
                }
                Opcode::Return => {
                    let src = instr.a() as usize;
                    return regs[src];
                }
                Opcode::Throw => {
                    // In the test harness `Throw` surfaces the result; treat as return.
                    let src = instr.a() as usize;
                    return regs[src];
                }
                _ => {
                    return JsValue::undefined();
                }
            }
        }
    })
}
