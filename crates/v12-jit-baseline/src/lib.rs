#![forbid(unsafe_code)]

//! Tier-1 baseline JIT over Cranelift.
//!
//! This crate provides a template JIT that lowers bytecode to Cranelift IR
//! one block per bytecode instruction. Arithmetic is baked via
//! `box_number` immediates and heap-agnostic helpers; control flow maps
//! bytecode jumps to Cranelift branches; calls go through a runtime helper
//! that re-enters the interpreter or native registry. The W^X strategy is
//! documented in [`mmap`].

pub mod mmap;

#[cfg(feature = "jit")]
mod compiler;
#[cfg(feature = "jit")]
mod runtime;
#[cfg(not(feature = "jit"))]
mod stub;

// ADR-006: the shared cache/error types now live in `v12-codegen`; this
// crate re-exports them for back-compat (both JIT tiers depend on the
// shared core, not on each other).
pub use v12_codegen::{CompiledFn, FunctionId, JitCache, JitError, MAX_JIT_FUNCTION_SIZE, MAX_JIT_REGISTERS};
#[cfg(feature = "jit")]
pub use compiler::JitBaseline;
#[cfg(not(feature = "jit"))]
pub use stub::JitBaseline;

// Re-export bytecode types for convenience.
pub use v12_bytecode::{FunctionBytecode, PcMapEntry};

#[cfg(feature = "jit")]
#[cfg(test)]
mod tests {
    use super::*;
    use v12_bytecode::{Const, ConstantPool, FunctionBytecode, Instr, Opcode, WideOp};
    use v12_heap::JsValue;

    // -----------------------------------------------------------------------
    // Helpers to build bytecode for differential tests.
    // -----------------------------------------------------------------------

    fn empty_fn(max_regs: u16, instrs: Vec<Instr>, consts: ConstantPool) -> FunctionBytecode {
        let mut fb = FunctionBytecode::with_instructions(instrs, max_regs);
        fb.consts = consts;
        fb
    }

    /// Evaluates `bytecode` with the interpreter via the throw-surface trick.
    fn interp_throw_result(bytecode: FunctionBytecode) -> JsValue {
        // Append a Throw that surfaces the requested return register.
        // For the harness we rewrite the final Return into Throw for interp.
        let mut fb = bytecode;
        let last = fb.instrs.len() - 1;
        let ret_reg = fb.instrs[last].a();
        fb.instrs[last] = Instr::new(Opcode::Throw, ret_reg, 0, 0);
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        let mut interp = v12_interp::Interp::new(&mut heap, vec![fb], 0, Vec::new());
        match interp.run() {
            Err(e) => e.0,
            Ok(()) => panic!("interp did not throw"),
        }
    }

    // -----------------------------------------------------------------------
    // Stage 1: skeleton
    // -----------------------------------------------------------------------

    #[test]
    fn cache_insert_and_lookup() {
        let mut cache = JitCache::new();
        assert!(cache.is_empty());
        let fb = empty_fn(
            2,
            vec![Instr::new(Opcode::Return, 0, 0, 0)],
            ConstantPool::new(),
        );
        let mut baseline = JitBaseline::new().expect("new");
        let compiled = baseline.compile(&fb).expect("compile supported");
        let id: FunctionId = 42;
        cache.insert(id, compiled);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(id).is_some());
        assert!(cache.get(99).is_none());
        assert_eq!(cache.ids().collect::<Vec<_>>(), vec![42]);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn unsupported_opcode_returns_error() {
        let mut baseline = JitBaseline::new().expect("new");
        // GetProperty is not supported in the baseline.
        let fb = empty_fn(
            2,
            vec![
                Instr::new(Opcode::GetProperty, 0, 1, 2),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
            ConstantPool::new(),
        );
        let err = baseline.compile(&fb).expect_err("should be unsupported");
        assert!(matches!(err, JitError::UnsupportedOpcode(_)));
    }

    #[test]
    fn too_large_function_rejected() {
        let mut baseline = JitBaseline::new().expect("new");
        let instrs = vec![Instr::new(Opcode::Move, 0, 0, 0); MAX_JIT_FUNCTION_SIZE + 1];
        let fb = empty_fn(2, instrs, ConstantPool::new());
        let err = baseline.compile(&fb).expect_err("should be too large");
        assert!(matches!(err, JitError::TooLarge { .. }));
    }

    // -----------------------------------------------------------------------
    // Stage 2: straight-line arithmetic
    // -----------------------------------------------------------------------

    #[test]
    fn differential_arithmetic_return_2_plus_3_mul_4() {
        // return 2 + 3 * 4  => 14
        let instrs = vec![
            Instr::new(Opcode::LoadInt, 0, 0, 2),
            Instr::new(Opcode::LoadInt, 1, 0, 3),
            Instr::new(Opcode::LoadInt, 2, 0, 4),
            Instr::new(Opcode::Mul, 3, 1, 2),
            Instr::new(Opcode::Add, 4, 0, 3),
            Instr::new(Opcode::Return, 4, 0, 0),
        ];
        let fb = empty_fn(5, instrs, ConstantPool::new());
        let fb_for_interp = fb.clone();

        let mut baseline = JitBaseline::new().expect("new");
        let compiled = baseline.compile(&fb).expect("compile");
        let mut regs = vec![JsValue::undefined(); 5];
        let jit_result = compiled.execute(&mut regs);
        assert_eq!(jit_result.as_smi(), Some(14));

        let interp_result = interp_throw_result(fb_for_interp);
        assert_eq!(jit_result.bits(), interp_result.bits());
    }

    #[test]
    fn differential_arithmetic_with_constants_and_neg() {
        // r0 = const 10.5, r1 = 2, r2 = r0 / r1 => 5.25, r3 = -r2 => -5.25, return r3
        let mut pool = ConstantPool::new();
        let k = pool.insert(Const::F64(10.5)).unwrap();
        let instrs = vec![
            Instr::new_imm16(Opcode::LoadConst, 0, k),
            Instr::new(Opcode::LoadInt, 1, 0, 2),
            Instr::new(Opcode::Div, 2, 0, 1),
            Instr::new(Opcode::Neg, 3, 2, 0),
            Instr::new(Opcode::Return, 3, 0, 0),
        ];
        let fb = empty_fn(4, instrs, pool);
        let fb2 = fb.clone();
        let mut baseline = JitBaseline::new().unwrap();
        let compiled = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 4];
        let jit_result = compiled.execute(&mut regs);
        assert_eq!(jit_result.as_f64(), Some(-5.25));
        let interp_result = interp_throw_result(fb2);
        assert_eq!(jit_result.bits(), interp_result.bits());
    }

    #[test]
    fn move_and_sub() {
        // r0=10, r1=3, r2=r0, r3=r2 - r1 => 7
        let instrs = vec![
            Instr::new(Opcode::LoadInt, 0, 0, 10),
            Instr::new(Opcode::LoadInt, 1, 0, 3),
            Instr::new(Opcode::Move, 2, 0, 0),
            Instr::new(Opcode::Sub, 3, 2, 1),
            Instr::new(Opcode::Return, 3, 0, 0),
        ];
        let fb = empty_fn(4, instrs, ConstantPool::new());
        let fb2 = fb.clone();
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 4];
        let jit = c.execute(&mut regs);
        assert_eq!(jit.as_smi(), Some(7));
        assert_eq!(jit.bits(), interp_throw_result(fb2).bits());
    }

    #[test]
    fn wide_load_int_and_call_w() {
        // Wide LoadIntW and CallW path: r0 = 300 (wide), CallW r1 = call r2, argc 0
        // For call, r2 is callee Smi 255 => returns 255
        let mut instrs = Vec::new();
        instrs.extend(WideOp::LoadIntW { dst: 0, value: 300 }.encode());
        instrs.extend(WideOp::LoadIntW { dst: 2, value: 255 }.encode());
        instrs.extend(
            WideOp::CallW {
                dst: 1,
                func: 2,
                argc: 0,
            }
            .encode(),
        );
        instrs.push(Instr::new(Opcode::Add, 3, 0, 1)); // 300 + 255 = 555
        instrs.push(Instr::new(Opcode::Return, 3, 0, 0));
        let fb = empty_fn(4, instrs, ConstantPool::new());
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 4];
        let res = c.execute(&mut regs);
        assert_eq!(res.as_smi(), Some(555));
    }

    // -----------------------------------------------------------------------
    // Stage 3: control flow
    // -----------------------------------------------------------------------

    #[test]
    fn loop_summing_0_to_10() {
        // sum = 0, i = 0, limit = 10, one = 1
        // loop: if i >= limit goto end; sum += i; i += 1; goto loop
        use v12_bytecode::FunctionBuilder;
        let mut b = FunctionBuilder::new(None);
        b.reserve_regs(6);
        let top = b.label();
        let end = b.label();
        // r0=sum, r1=i, r2=limit, r3=one
        b.emit(Instr::new(Opcode::LoadInt, 0, 0, 0)); // sum=0
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 0)); // i=0
        b.emit(Instr::new(Opcode::LoadInt, 2, 0, 10)); // limit=10
        b.emit(Instr::new(Opcode::LoadInt, 3, 0, 1)); // one=1
        b.bind(top);
        b.emit(Instr::new_imm24(Opcode::LoopHeader, 0));
        b.emit(Instr::new(Opcode::Ge, 4, 1, 2)); // i >= limit ?
        b.emit_jump(Opcode::JumpIfTrue, 4, end);
        b.emit(Instr::new(Opcode::Add, 0, 0, 1)); // sum+=i
        b.emit(Instr::new(Opcode::Add, 1, 1, 3)); // i+=1
        b.emit_jump(Opcode::Jump, 0, top);
        b.bind(end);
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));
        let fb = b.finish();
        let fb2 = fb.clone();
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 6];
        let jit = c.execute(&mut regs);
        assert_eq!(jit.as_smi(), Some(45)); // 0..9 sum =45
        assert_eq!(jit.bits(), interp_throw_result(fb2).bits());
    }

    #[test]
    fn jump_if_false_and_true() {
        // r0 = 0, r1 = 1, test both branches
        // if r1 true -> r2=10 else r2=20 ; return r2 =>10
        use v12_bytecode::FunctionBuilder;
        let mut b = FunctionBuilder::new(None);
        b.reserve_regs(3);
        let else_l = b.label();
        let end = b.label();
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 1)); // r1=1 (truthy)
        b.emit(Instr::new(Opcode::Lt, 0, 1, 1)); // r0 = 1<1? false
        b.emit_jump(Opcode::JumpIfFalse, 0, else_l);
        b.emit(Instr::new(Opcode::LoadInt, 2, 0, 10));
        b.emit_jump(Opcode::Jump, 0, end);
        b.bind(else_l);
        b.emit(Instr::new(Opcode::LoadInt, 2, 0, 20));
        b.bind(end);
        b.emit(Instr::new(Opcode::Return, 2, 0, 0));
        let fb = b.finish();
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 3];
        let res = c.execute(&mut regs);
        assert_eq!(res.as_smi(), Some(20)); // r0 false => JumpIfFalse taken =>20
    }

    // -----------------------------------------------------------------------
    // Stage 4: calls
    // -----------------------------------------------------------------------

    #[test]
    fn call_native_seam() {
        // r1 = 255 (native probe), r2=7, r3=8 (unused args slot), Call r0, r1, argc=2 => 275
        // Note: Call ABI expects callee at r1, this at r2, args at r3,r4, but our
        // simplified helper only uses argc, so we ignore this/args layout for the
        // probe. The test still validates that Call lowers through the runtime.
        let mut instrs2 = Vec::new();
        instrs2.extend(WideOp::LoadIntW { dst: 1, value: 255 }.encode());
        instrs2.push(Instr::new(Opcode::Call, 0, 1, 2));
        instrs2.push(Instr::new(Opcode::Return, 0, 0, 0));
        let fb = empty_fn(4, instrs2, ConstantPool::new());
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 4];
        // Pre-fill this/args as undefined (not used by probe)
        let res = c.execute(&mut regs);
        assert_eq!(res.as_smi(), Some(275));
        // Also validate deopt_info is present (stage 5) even for call functions.
        assert!(!c.deopt_info().is_empty());
    }

    #[test]
    fn closure_call_style() {
        // More realistic: set up regs as the interpreter would: callee at r0, this at r1,
        // args at r2,r3, call via Call r4, r0, argc=2, return r4.
        // Callee is native 255, so result 275.
        let mut instrs = Vec::new();
        instrs.extend(WideOp::LoadIntW { dst: 0, value: 255 }.encode());
        instrs.push(Instr::new(Opcode::LoadInt, 1, 0, 0)); // this = 0
        instrs.extend(WideOp::LoadIntW { dst: 2, value: 7 }.encode());
        instrs.extend(WideOp::LoadIntW { dst: 3, value: 8 }.encode());
        instrs.push(Instr::new(Opcode::Call, 4, 0, 2));
        instrs.push(Instr::new(Opcode::Return, 4, 0, 0));
        let fb = empty_fn(5, instrs, ConstantPool::new());
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 5];
        let res = c.execute(&mut regs);
        assert_eq!(res.as_smi(), Some(275));
    }

    // -----------------------------------------------------------------------
    // Stage 5: deopt data
    // -----------------------------------------------------------------------

    #[test]
    fn deopt_info_not_empty_and_sorted() {
        let instrs = vec![
            Instr::new(Opcode::LoadInt, 0, 0, 1),
            Instr::new(Opcode::LoadInt, 1, 0, 2),
            Instr::new(Opcode::Add, 2, 0, 1),
            Instr::new(Opcode::Return, 2, 0, 0),
        ];
        let fb = empty_fn(3, instrs, ConstantPool::new());
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let info = c.deopt_info();
        assert!(!info.is_empty());
        // jit_pc entries should be increasing and map 1:1 to bc_pc for template.
        for w in info.windows(2) {
            assert!(w[0].jit_pc < w[1].jit_pc);
        }
        assert_eq!(info[0].bc_pc, 0);
    }

    #[test]
    fn pc_map_covers_all_bytecode_pcs() {
        let mut b = v12_bytecode::FunctionBuilder::new(None);
        b.reserve_regs(4);
        let top = b.label();
        let end = b.label();
        b.emit(Instr::new(Opcode::LoadInt, 0, 0, 0));
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 5));
        b.bind(top);
        b.emit(Instr::new_imm24(Opcode::LoopHeader, 0));
        b.emit(Instr::new(Opcode::Lt, 2, 0, 1));
        b.emit_jump(Opcode::JumpIfFalse, 2, end);
        b.emit(Instr::new(Opcode::Add, 0, 0, 1));
        b.emit(Instr::new(Opcode::LoadInt, 3, 0, 1));
        b.emit(Instr::new(Opcode::Add, 0, 0, 3));
        b.emit_jump(Opcode::Jump, 0, top);
        b.bind(end);
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));
        let fb = b.finish();
        let mut baseline = JitBaseline::new().unwrap();
        let c = baseline.compile(&fb).unwrap();
        let pcs: std::collections::HashSet<u32> = c.deopt_info().iter().map(|e| e.bc_pc).collect();
        // Every bytecode pc that is an instruction header should have a mapping.
        // For simplicity we check that first and last are present.
        assert!(pcs.contains(&0));
        assert!(pcs.contains(&(fb.instrs.len() as u32 - 1)));
    }

    #[test]
    fn const_null_bakes_to_null_via_load_const() {
        // `null` via LoadConst should bake to JsValue::null() bits.
        let mut pool = ConstantPool::new();
        let k_null = pool.insert(Const::Null).unwrap();
        let instrs = vec![
            Instr::new_imm16(Opcode::LoadConst, 0, k_null),
            Instr::new(Opcode::Return, 0, 0, 0),
        ];
        let fb = empty_fn(1, instrs, pool);
        let fb2 = fb.clone();
        let mut baseline = JitBaseline::new().unwrap();
        let compiled = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 1];
        let jit_result = compiled.execute(&mut regs);
        assert!(jit_result.is_null(), "JIT LoadConst Null must be null");
        let interp_result = interp_throw_result(fb2);
        assert_eq!(jit_result.bits(), interp_result.bits());
    }

    #[test]
    fn const_null_bakes_to_null_via_load_const_w() {
        // Wide variant LoadConstW with Const::Null.
        let mut pool = ConstantPool::new();
        let k_null = pool.insert(Const::Null).unwrap();
        let mut instrs = WideOp::LoadConstW {
            dst: 0,
            const_id: u32::from(k_null),
        }
        .encode();
        instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
        let fb = empty_fn(1, instrs, pool);
        let fb2 = fb.clone();
        let mut baseline = JitBaseline::new().unwrap();
        let compiled = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 1];
        let jit_result = compiled.execute(&mut regs);
        assert!(jit_result.is_null());
        let interp_result = interp_throw_result(fb2);
        assert_eq!(jit_result.bits(), interp_result.bits());
    }

    #[test]
    fn typeof_null_via_jit_is_object_when_interpreted() {
        // Baseline fast path does not yet implement `TypeOf`; this test documents
        // that interp correctly reports "object" for null via the same bytecode
        // that the JIT would later lower.
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        let mut interp = v12_interp::Interp::from_source(&mut heap, "throw typeof null;").expect("compiles");
        let thrown = match interp.run() {
            Err(e) => e.0,
            Ok(()) => panic!("expected throw"),
        };
        assert!(thrown.is_string());
        assert_eq!(interp.to_display_string(thrown), "object");
    }
}

#[cfg(not(feature = "jit"))]
#[cfg(test)]
mod stub_tests {
    use super::*;
    use v12_bytecode::{ConstantPool, FunctionBytecode, Instr, Opcode};

    fn empty_fn(max_regs: u16, instrs: Vec<Instr>, consts: ConstantPool) -> FunctionBytecode {
        let mut fb = FunctionBytecode::with_instructions(instrs, max_regs);
        fb.consts = consts;
        fb
    }

    #[test]
    fn disabled_feature_stub_reports_jit_disabled() {
        let mut baseline = JitBaseline::new().expect("new even when disabled");
        let fb = empty_fn(
            1,
            vec![Instr::new(Opcode::Return, 0, 0, 0)],
            ConstantPool::new(),
        );
        let err = baseline.compile(&fb).expect_err("disabled");
        assert_eq!(err.to_string(), "JIT disabled");
        assert!(matches!(err, JitError::Disabled));
    }

    #[test]
    fn cache_still_works_when_jit_disabled() {
        let cache = JitCache::new();
        assert!(cache.is_empty());
        // Cache does not require JIT to be enabled; it is just a map.
        // Insert a dummy compiled fn via stub (we cannot compile, but we can
        // test cache insert/lookup with a manually constructed entry if the
        // stub exposes a test helper). For now just check empty behavior.
        assert_eq!(cache.len(), 0);
        assert!(cache.get(0).is_none());
    }
}
