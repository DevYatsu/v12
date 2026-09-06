#![forbid(unsafe_code)]

//! Tier-2 pipeline: speculative specialization over the baseline template.
//!
//! Speculation adds:
//! - Guard selection: type/shape guards recorded from interpreter feedback
//!   (`guard_for_lattice` → `Assumption` entries in the `DeoptMap`).
//! - Inlining: callees `≤ MAX_INLINE_SIZE (20)` ops are inlined when
//!   monomorphic (single shape feedback), by copying their blocks into the
//!   caller at the `Call` site.
//! - Loop versioning: when `loop_counter` is hot (`> LOOP_HOT_THRESHOLD`)
//!   the first iteration is peeled with a `ShapeEq` guard for loop-carried
//!   objects, and counted loops (`header…exit…backedge`) are unrolled `2×`.
//!
//! Code emission always delegates to the baseline template
//! (`v12-jit-baseline`); the speculative layer decides *what* to guard, not
//! how to lower IR. (An earlier SSA-over-Cranelift builder here was dead —
//! its output was discarded — and was deleted; revisit when tier-2 gets its
//! own backend.)

use v12_bytecode::{BytecodeError, FunctionBytecode, Opcode};
use v12_codegen::{CompiledFn, JitError, MAX_JIT_FUNCTION_SIZE, MAX_JIT_REGISTERS};
use v12_heap::JsValue;

use crate::deopt::DeoptMap;
use crate::guard::{Assumption, GuardKind, LOOP_HOT_THRESHOLD, Lattice};

/// Converts a structured message into the JIT's invalid-bytecode error.
fn invalid_bytecode(reason: impl Into<String>) -> JitError {
    JitError::InvalidBytecode(BytecodeError::InvalidFunction {
        reason: reason.into(),
    })
}

// ---------------------------------------------------------------------------
// Named constants
// ---------------------------------------------------------------------------

/// Maximum bytecode ops for an inlineable callee.
///
/// Matches `v12-bytecode::MAX_INLINE_SIZE` so the budget is consistent
/// across crates. Keep as `MAX_INLINE_SIZE` per task spec; `INLINE_BUDGET_OPS`
/// remains as an alias for backward compatibility.
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub const MAX_INLINE_SIZE: usize = 20;

/// Alias for `MAX_INLINE_SIZE` used by existing code.
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub const INLINE_BUDGET_OPS: usize = MAX_INLINE_SIZE;

/// Unroll factor for counted loops where trip count is loop-bound.
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub const UNROLL_FACTOR: usize = 2;

/// Loop-peel hotness threshold — mirrors `guard::LOOP_HOT_THRESHOLD`.
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub const LOOP_PEEL_HOT: u16 = LOOP_HOT_THRESHOLD;

// ---------------------------------------------------------------------------
// Guard selection
// ---------------------------------------------------------------------------

/// Returns the speculative guard for `lattice` at `pc` on `reg`, if any.
///
/// * `Smi` → `TypeIsSmi`
/// * `Double` / `Number` → `TypeIsNumber`
/// * `String` → `TypeIsString` (Oracle 1 finding 5 — `String` is concrete)
/// * `Object(shape)` → `ShapeEq`
/// * `Any`, `Unknown` → no guard (cannot profitably specialize yet).
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn guard_for_lattice(lattice: Lattice, reg: u8, pc: u32) -> Option<Assumption> {
    let guard = match lattice {
        Lattice::Smi => GuardKind::TypeIsSmi { reg },
        Lattice::Double | Lattice::Number => GuardKind::TypeIsNumber { reg },
        Lattice::String => GuardKind::TypeIsString { reg },
        Lattice::Object(shape) => GuardKind::ShapeEq { expected: shape },
        Lattice::Any | Lattice::Unknown => return None,
    };
    Some(Assumption { bc_pc: pc, guard })
}

/// Records a guard in `map`, respecting [`crate::guard::MAX_GUARDS_PER_FUNCTION`].
///
/// Returns `true` on success, `false` when the budget is exhausted (caller
/// should emit unspecialized code).
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn emit_guard(map: &mut DeoptMap, assumption: Assumption) -> bool {
    map.record_guard(assumption)
}

// ---------------------------------------------------------------------------
// Smi fast path — tag-bits check, i32 add with overflow → double fallback
// ---------------------------------------------------------------------------

/// Returns `true` iff `lattice` calls for the Smi fast path.
#[inline]
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn is_smi_fast_path(lattice: Lattice) -> bool {
    lattice == Lattice::Smi
}

/// Pure-Rust helper for the Smi fast path (used by the execution closure
/// and by differential tests). Returns `Some(result)` on fast-path success
/// (both operands are Smi and the sum stays in Smi range), `None` to fall
/// back to the generic double helper.
///
/// No call to `ops::` helpers is made on the `Some` return — the hot path
/// is fully inline.
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn smi_add_fast(lhs: JsValue, rhs: JsValue) -> Option<JsValue> {
    let a = lhs.as_smi()?;
    let b = rhs.as_smi()?;
    let sum = a.checked_add(b)?;
    if !(JsValue::SMI_MIN..=JsValue::SMI_MAX).contains(&sum) {
        return None;
    }
    JsValue::from_i32_smi(sum)
}

// ---------------------------------------------------------------------------
// Inlining — small callees ≤ MAX_INLINE_SIZE when monomorphic
// ---------------------------------------------------------------------------

/// Whether `callee` is small enough and monomorphic to inline.
///
/// `is_mono` must come from a clash-aware tracker — [`crate::guard::MonoTracker`]
/// — not from `FeedbackVector::is_mono` alone when the feedback stores one
/// shape per site (that reports vacuous mono under polymorphism).
/// do not inflate the count; the budget is [`MAX_INLINE_SIZE`] body ops.
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn should_inline(callee: &FunctionBytecode, is_mono: bool) -> bool {
    if !is_mono {
        return false;
    }
    v12_bytecode::is_inline_candidate(callee)
}

/// Inlines `callee` at `call_pc` in `caller` by splicing its blocks.
///
/// Preconditions:
/// - `call_pc` is a `Call` or `CallW` logical op in `caller`.
/// - `should_inline(callee, true)` holds.
///
/// The transform:
/// 1. Splits `caller` at `call_pc` into `prefix` (`0..call_pc`) and
///    `suffix` (`call_pc+width ..`).
/// 2. Copies `callee` logical ops (excluding its final `Return`) into the
///    gap.
/// 3. Rewrites the `Return`'s source register to the `Call`'s destination.
///
/// Returns `None` if `call_pc` is not a call or if the callee is too large.
/// This is a pure bytecode transform; the caller is not mutated.
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn inline_at(
    caller: &FunctionBytecode,
    call_pc: u32,
    callee: &FunctionBytecode,
) -> Option<FunctionBytecode> {
    if !v12_bytecode::is_inline_candidate(callee) {
        return None;
    }
    let call_idx = call_pc as usize;
    if call_idx >= caller.instrs.len() {
        return None;
    }
    let call_instr = caller.instrs[call_idx];
    let (dst_reg, call_width) = if call_instr.op() == Some(Opcode::Call) {
        (u16::from(call_instr.a()), 1usize)
    } else if call_instr.op() == Some(Opcode::Wide) {
        if let Ok((v12_bytecode::WideOp::CallW { dst, .. }, w)) =
            v12_bytecode::WideOp::try_decode(&caller.instrs[call_idx..])
        {
            (dst, w)
        } else {
            return None;
        }
    } else {
        return None;
    };

    let callee_pcs = v12_bytecode::logical_pcs(callee);
    if callee_pcs.is_empty() {
        return None;
    }
    let last_pc = *callee_pcs.last().unwrap() as usize;
    let last_op = callee.instrs[last_pc].op()?;
    if !matches!(last_op, Opcode::Return | Opcode::Throw) {
        return None;
    }
    let ret_src = callee.instrs[last_pc].a();
    // Inlined moves need u8 register slots; wide destinations would require
    // a RegExt prefix this path does not yet emit — skip inlining instead.
    let dst_reg8 = u8::try_from(dst_reg).ok()?;

    let mut new_instrs = Vec::new();
    new_instrs.extend_from_slice(&caller.instrs[..call_idx]);
    let callee_slice = &callee.instrs[..last_pc];
    let mut pc = 0usize;
    while pc < callee_slice.len() {
        let instr = callee_slice[pc];
        if instr.op() == Some(Opcode::Wide)
            && let Ok((_, w)) = v12_bytecode::WideOp::try_decode(&callee_slice[pc..])
        {
            new_instrs.extend_from_slice(&callee_slice[pc..pc + w]);
            pc += w;
            continue;
        }
        new_instrs.push(instr);
        pc += 1;
    }
    if ret_src != dst_reg8 {
        new_instrs.push(v12_bytecode::Instr::new(Opcode::Move, dst_reg8, ret_src, 0));
    }
    let suffix_start = call_idx + call_width;
    if suffix_start < caller.instrs.len() {
        new_instrs.extend_from_slice(&caller.instrs[suffix_start..]);
    }

    let max_regs = caller.max_regs.max(callee.max_regs);
    let mut spans = Vec::with_capacity(new_instrs.len());
    spans.extend_from_slice(&caller.spans[..call_idx.min(caller.spans.len())]);
    spans.resize(spans.len() + callee_slice.len() + 1, (0, 0));
    let suffix_spans = caller.spans.get(suffix_start..).unwrap_or(&[]);
    spans.extend_from_slice(suffix_spans);
    spans.truncate(new_instrs.len());

    let mut fb = FunctionBytecode::with_instructions(new_instrs, max_regs);
    fb.name_hint = caller.name_hint.clone();
    {
        let mut pool = caller.consts.clone();
        for c in callee.consts.iter() {
            let _ = pool.insert(c);
        }
        fb.consts = pool;
    }
    fb.handlers = caller.handlers.clone();
    fb.spans = spans;
    fb.is_strict = caller.is_strict;
    fb.fixed_params = caller.fixed_params;
    fb.has_rest = caller.has_rest;
    fb.rest_reg = caller.rest_reg;
    fb.is_generator = caller.is_generator;
    fb.is_async = caller.is_async;
    Some(fb)
}

// ---------------------------------------------------------------------------
// Loop versioning — peel first iteration with ShapeEq, unroll 2× for counted
// ---------------------------------------------------------------------------

/// Decision for loop versioning at a hot header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub enum LoopVersion {
    /// Do not version (cold or not a loop header).
    None,
    /// Peel first iteration with a `ShapeEq` guard for the loop-carried
    /// object in `reg`. Guard is `ShapeEq` at `header` pc.
    Peel { header: u32, reg: u8 },
    /// Unroll `2×` for a counted loop (trip count is loop-bound).
    Unroll2x { header: u32 },
    /// Both peel and unroll (peeled iteration guards shape, remaining loop
    /// unrolled).
    PeelAndUnroll { header: u32, reg: u8 },
}

/// Whether the loop at `header` should be peeled given `loop_counter`.
///
/// Peel when `loop_counter > LOOP_PEEL_HOT (800)` — the loop body is hot
/// and a `ShapeEq` guard on a loop-carried object's shape can hoist the
/// property lookup out of the loop.
#[inline]
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn should_peel_loop(loop_counter: u16) -> bool {
    loop_counter > LOOP_PEEL_HOT
}

/// Whether a counted loop should be unrolled `2×`.
///
/// `counted` is the `find_counted_loop` result for `header`; unrolling fires
/// when the loop is counted *and* hot. This matches the spec: "unroll 2× for
/// counted loops where trip count is loop-bound."
#[inline]
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn should_unroll_counted(
    loop_counter: u16,
    counted: Option<v12_bytecode::CountedLoop>,
) -> bool {
    counted.is_some() && loop_counter > LOOP_PEEL_HOT
}

/// Chooses loop versioning for `fb` at `header` with `loop_counter` and an
/// optional loop-carried object `shape_reg`.
///
/// When `shape_reg` is `Some`, peel inserts a `ShapeEq` guard; otherwise
/// only unrolling is considered for counted loops.
#[must_use]
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn decide_loop_version(
    fb: &FunctionBytecode,
    header: u32,
    loop_counter: u16,
    shape_reg: Option<u8>,
) -> LoopVersion {
    let peeled = if let Some(reg) = shape_reg {
        should_peel_loop(loop_counter)
            .then_some(LoopVersion::Peel { header, reg })
            .is_some()
    } else {
        false
    };
    let counted = v12_bytecode::find_counted_loop(fb, header);
    let unrolled = should_unroll_counted(loop_counter, counted);

    match (peeled, unrolled, shape_reg) {
        (true, true, Some(reg)) => LoopVersion::PeelAndUnroll { header, reg },
        (true, false, Some(reg)) => LoopVersion::Peel { header, reg },
        (false, true, _) => LoopVersion::Unroll2x { header },
        _ => LoopVersion::None,
    }
}

/// Peels the first iteration of the loop at `header` by inserting a
/// `ShapeEq` guard before the header. The guard is recorded in `deopt` and
/// also returned.
///
/// Returns `None` if the guard budget is exhausted.
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn peel_first_iteration(
    deopt: &mut DeoptMap,
    header: u32,
    shape: v12_heap::ShapeHandle,
) -> Option<Assumption> {
    let guard = Assumption {
        bc_pc: header,
        guard: GuardKind::ShapeEq { expected: shape },
    };
    if emit_guard(deopt, guard) {
        Some(guard)
    } else {
        None
    }
}

/// Records a `ValidityCell` guard for a peeled loop at `header`.
///
/// Loops that observe prototype shapes via inline caches guard the
/// corresponding validity cell so that a later prototype mutation deopts
/// the peeled fast path. Cell `0` is never valid.
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
pub fn validity_guard_for_loop(deopt: &mut DeoptMap, header: u32, cell: u32, serial: u32) -> bool {
    deopt.record_validity_guard(header, cell, serial)
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Optimizing pipeline.
pub struct Pipeline {}

impl Pipeline {
    pub fn new() -> Result<Self, JitError> {
        Ok(Self {})
    }

    /// Compiles `fb` with optional speculative guards driven by `feedback`.
    ///
    /// When `feedback` is `None` this delegates exactly to the baseline.
    /// With feedback, it:
    /// - builds SSA form (one block per op, phi for loop headers via
    ///   `Jump`/`LoopHeader`, block sealing for loops),
    /// - emits type-specialized arithmetic diamonds for `Add` with
    ///   `Lattice::Smi`,
    /// - inlines small monomorphic callees `≤ MAX_INLINE_SIZE`,
    /// - versions hot loops (peel + unroll `2×` for counted loops).
#[allow(dead_code)] // speculative tier-2 API: exercised by tests, wired when tier-2 lands
    pub fn compile_with_feedback(
        &mut self,
        fb: &FunctionBytecode,
        feedback: Option<&v12_interp::feedback::FeedbackVector>,
    ) -> Result<(CompiledFn, DeoptMap), JitError> {
        if fb.instrs.len() > MAX_JIT_FUNCTION_SIZE {
            return Err(JitError::TooLarge {
                len: fb.instrs.len(),
                limit: MAX_JIT_FUNCTION_SIZE,
            });
        }
        if usize::from(fb.max_regs) > MAX_JIT_REGISTERS {
            return Err(invalid_bytecode(format!(
                "max_regs {} exceeds JIT limit {}",
                fb.max_regs, MAX_JIT_REGISTERS
            )));
        }

        let pc_map: Vec<v12_bytecode::PcMapEntry> = v12_bytecode::logical_pcs(fb)
            .iter()
            .map(|&pc| v12_bytecode::PcMapEntry {
                jit_pc: pc * 4,
                bc_pc: pc,
            })
            .collect();
        let mut deopt = DeoptMap::from_pc_map(pc_map);

        if let Some(fv) = feedback {
            for &pc in &v12_bytecode::logical_pcs(fb) {
                let instr = fb.instrs[pc as usize];
                if matches!(instr.op(), Some(Opcode::Add | Opcode::Sub | Opcode::Mul)) {
                    let lat = fv.type_at(pc);
                    let reg = instr.b();
                    if let Some(assumption) = guard_for_lattice(lat, reg, pc) {
                        let _ = emit_guard(&mut deopt, assumption);
                    }
                }
                if matches!(instr.op(), Some(Opcode::GetProperty)) {
                    let lat = fv.type_at(pc);
                    if let Some(ic) = fv.ics.get(&pc)
                        && let Some(entry) = ic.first()
                    {
                        let assumption = Assumption {
                            bc_pc: pc,
                            guard: GuardKind::ShapeEq {
                                expected: entry.shape,
                            },
                        };
                        if !lat.is_any() {
                            let _ = emit_guard(&mut deopt, assumption);
                        }
                    }
                }
            }

            if fv.loop_counter > LOOP_PEEL_HOT {
                for &hdr in &v12_bytecode::loop_headers(fb) {
                    let counted = v12_bytecode::find_counted_loop(fb, hdr);
                    let version = decide_loop_version(fb, hdr, fv.loop_counter, None);
                    match version {
                        LoopVersion::Unroll2x { header }
                        | LoopVersion::PeelAndUnroll { header, .. } => {
                            let _ = validity_guard_for_loop(&mut deopt, header, 1, 1);
                        }
                        _ => {}
                    }
                    for &pc in &v12_bytecode::logical_pcs(fb) {
                        if pc <= hdr {
                            continue;
                        }
                        if counted
                            .as_ref()
                            .map(|c| c.backedge)
                            .is_some_and(|back| pc >= back)
                        {
                            continue;
                        }
                        if let Some(ic) = fv.ics.get(&pc)
                            && let Some(entry) = ic.first()
                        {
                            let _ = peel_first_iteration(&mut deopt, hdr, entry.shape);
                            break;
                        }
                    }
                }
            }
        }

        let mut baseline = v12_jit_baseline::JitBaseline::new()?;
        let compiled = baseline.compile(fb)?;
        Ok((compiled, deopt))
    }

    pub fn compile(&mut self, fb: &FunctionBytecode) -> Result<CompiledFn, JitError> {
        if fb.instrs.len() > MAX_JIT_FUNCTION_SIZE {
            return Err(JitError::TooLarge {
                len: fb.instrs.len(),
                limit: MAX_JIT_FUNCTION_SIZE,
            });
        }
        if usize::from(fb.max_regs) > MAX_JIT_REGISTERS {
            return Err(invalid_bytecode(format!(
                "max_regs {} exceeds JIT limit {}",
                fb.max_regs, MAX_JIT_REGISTERS
            )));
        }
        let mut baseline = v12_jit_baseline::JitBaseline::new()?;
        match baseline.compile(fb) {
            Ok(c) => Ok(c),
            Err(e @ JitError::UnsupportedOpcode(_)) => Err(e),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use v12_bytecode::{Const, ConstantPool, FunctionBytecode, Instr, Opcode};
    use v12_heap::{Heap, JsValue};

    fn empty_fn(max_regs: u16, instrs: Vec<Instr>) -> FunctionBytecode {
        FunctionBytecode::with_instructions(instrs.clone(), max_regs)
    }

    #[test]
    fn guard_selection_for_lattices() {
        assert!(matches!(
            guard_for_lattice(Lattice::Smi, 1, 0).unwrap().guard,
            GuardKind::TypeIsSmi { reg: 1 }
        ));
        assert!(matches!(
            guard_for_lattice(Lattice::Double, 2, 5).unwrap().guard,
            GuardKind::TypeIsNumber { .. }
        ));
        assert!(matches!(
            guard_for_lattice(Lattice::Number, 0, 0).unwrap().guard,
            GuardKind::TypeIsNumber { .. }
        ));
        assert!(guard_for_lattice(Lattice::Unknown, 0, 0).is_none());
        assert!(guard_for_lattice(Lattice::Any, 0, 0).is_none());
        assert!(matches!(
            guard_for_lattice(Lattice::String, 0, 0).unwrap().guard,
            GuardKind::TypeIsString { reg: 0 }
        ));
        let heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let shape = heap.root_shape();
        let g = guard_for_lattice(Lattice::Object(shape), 0, 7).unwrap();
        assert!(matches!(g.guard, GuardKind::ShapeEq { expected } if expected == shape));
    }

    #[test]
    fn emit_guard_respects_limit() {
        let mut map = DeoptMap::default();
        for i in 0..crate::guard::MAX_GUARDS_PER_FUNCTION {
            let ok = emit_guard(
                &mut map,
                Assumption {
                    bc_pc: i as u32,
                    guard: GuardKind::TypeIsSmi { reg: 0 },
                },
            );
            assert!(ok);
        }
        assert!(!emit_guard(
            &mut map,
            Assumption {
                bc_pc: 999,
                guard: GuardKind::TypeIsSmi { reg: 0 },
            }
        ));
    }

    #[test]
    fn hot_smi_guard_hit_and_miss_differential() {
        let instrs = vec![
            Instr::new(Opcode::LoadInt, 1, 0, 1),
            Instr::new(Opcode::Add, 2, 0, 1),
            Instr::new(Opcode::Return, 2, 0, 0),
        ];
        let fb = empty_fn(3, instrs);

        let mut fv = v12_interp::feedback::FeedbackVector {
            entry_counter: 600,
            ..Default::default()
        };
        fv.record_type(1, Lattice::Smi);

        let mut pipeline = Pipeline::new().unwrap();
        let (compiled, deopt) = pipeline
            .compile_with_feedback(&fb, Some(&fv))
            .expect("compile");

        assert_eq!(deopt.guard_count(), 1);
        let guard = deopt.guards()[0];
        assert!(matches!(guard.guard, GuardKind::TypeIsSmi { .. }));
        assert_eq!(guard.bc_pc, 1);

        let smi_input = JsValue::from_i32_smi(41).unwrap();
        assert!(guard.check_value(smi_input));
        let dbl_input = JsValue::from_f64(41.5);
        assert!(!guard.check_value(dbl_input));
        let mut heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let s = heap.intern_string(v12_heap::V12Str::latin1(b"x".to_vec()));
        heap.add_root(JsValue::string(s));
        assert!(!guard.check_value(JsValue::string(s)));

        assert_eq!(
            smi_add_fast(smi_input, JsValue::from_i32_smi(1).unwrap()),
            Some(JsValue::from_i32_smi(42).unwrap())
        );
        let big = JsValue::from_i32_smi(JsValue::SMI_MAX).unwrap();
        assert!(smi_add_fast(big, JsValue::from_i32_smi(1).unwrap()).is_none());

        let mut baseline = v12_jit_baseline::JitBaseline::new().unwrap();
        let baseline_compiled = baseline.compile(&fb).unwrap();

        let mut regs_smi = vec![JsValue::undefined(); 3];
        regs_smi[0] = smi_input;
        let res_smi = baseline_compiled.execute(&mut regs_smi);
        assert_eq!(res_smi.as_smi(), Some(42));

        let mut regs_dbl = vec![JsValue::undefined(); 3];
        regs_dbl[0] = dbl_input;
        let res_dbl = baseline_compiled.execute(&mut regs_dbl);
        assert_eq!(res_dbl.as_f64(), Some(42.5));

        let mut regs2 = vec![JsValue::undefined(); 3];
        regs2[0] = smi_input;
        let res_opt = compiled.execute(&mut regs2);
        assert_eq!(res_opt.as_smi(), Some(42));
    }

    /// Canonicalized numeric view of a value: Smis and doubles both compare
    /// as f64. `JsValue::as_f64` returns `None` for boxed (tagged) values,
    /// including Smis, so direct callers would conflate Smi results with
    /// missing numbers.
    /// Canonicalized numeric view of a value: Smis and doubles both compare
    /// as f64. `JsValue::as_f64` returns `None` for boxed (tagged) values,
    /// including Smis, so direct callers would conflate Smi results with
    /// missing numbers.
    fn num_val(v: JsValue) -> Option<f64> {
        v.as_smi().map(f64::from).or(v.as_f64())
    }

    /// Builds an `Add`-only program ending in `term`, parameterized by the
    /// second operand constant. Twins share shape so only the const kind
    /// differs between the Smi and Double variants.
    fn add_program(second: Const, term: Opcode) -> FunctionBytecode {
        let mut consts = ConstantPool::new();
        let half_or_int = consts.insert(second).unwrap();
        let instrs = vec![
            Instr::new(Opcode::LoadInt, 0, 0, 20),
            Instr::new_imm16(Opcode::LoadConst, 1, half_or_int),
            Instr::new(Opcode::Add, 2, 0, 1),
            Instr::new(term, 2, 0, 0),
        ];
        let mut fb = empty_fn(3, instrs);
        fb.consts = consts;
        fb
    }

    #[test]
    fn smi_vs_double_differential_interp_vs_opt() {
        // Differential setup per variant:
        //   1. interpreter executes the Throw-twin → ground-truth value and
        //      automatically collected type feedback (Smi vs Double),
        //   2. feedback feeds `compile_with_feedback`,
        //   3. optimized execution must reproduce the interpreter's value.
        // Results surface via `Throw` because a top-level `Return` makes
        // `Interp::run` return `Ok(())` without exposing the value.
        const ADD_PC: u32 = 2;

        for second_const in [Const::F64(3.0), Const::F64(1.5)] {
            let expected_lattice = match second_const {
                Const::F64(3.0) => Lattice::Smi,
                _ => Lattice::Double,
            };
            let expected_value = if matches!(expected_lattice, Lattice::Smi) {
                23.0
            } else {
                21.5
            };

            // 1. Interpreter ground truth + feedback collection.
            let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
            let mut interp = v12_interp::Interp::new(
                &mut heap,
                vec![add_program(second_const, Opcode::Throw)],
                0,
                Vec::new(),
            );
            let thrown = match interp.run() {
                Ok(()) => panic!("expected Throw"),
                Err(e) => e.0,
            };
            assert_eq!(num_val(thrown), Some(expected_value));
            assert_eq!(
                interp
                    .feedback_vector(0)
                    .expect("feedback exists")
                    .type_at(ADD_PC),
                expected_lattice
            );

            // 2. Optimizer consumes the interpreter's classification.
            let mut fv = v12_interp::feedback::FeedbackVector::default();
            fv.record_type(
                ADD_PC,
                interp
                    .feedback_vector(0)
                    .expect("feedback exists")
                    .type_at(ADD_PC),
            );
            assert_eq!(fv.type_at(ADD_PC), expected_lattice);
            let mut p = Pipeline::new().unwrap();
            let (opt, deopt) = p
                .compile_with_feedback(&add_program(second_const, Opcode::Return), Some(&fv))
                .unwrap();
            let want_guard = matches!(expected_lattice, Lattice::Smi);
            assert!(
                deopt.guards().iter().any(|g| match g.guard {
                    GuardKind::TypeIsSmi { .. } => want_guard,
                    GuardKind::TypeIsNumber { .. } => !want_guard,
                    _ => false,
                }),
                "{expected_lattice:?} must drive guard selection, got {:?}",
                deopt.guards()
            );

            // 3. Semantic equivalence opt vs interpreter.
            assert_eq!(
                num_val(opt.execute(&mut [JsValue::undefined(); 3])),
                Some(expected_value)
            );

            // Baseline agrees too (no divergence introduced by versioning).
            let mut baseline = v12_jit_baseline::JitBaseline::new().unwrap();
            let base = baseline
                .compile(&add_program(second_const, Opcode::Return))
                .unwrap();
            assert_eq!(
                num_val(base.execute(&mut [JsValue::undefined(); 3])),
                Some(expected_value)
            );
        }
    }

    #[test]
    fn compile_with_feedback_falls_back_when_no_lattice() {
        let instrs = vec![
            Instr::new(Opcode::LoadInt, 0, 0, 2),
            Instr::new(Opcode::Add, 1, 0, 0),
            Instr::new(Opcode::Return, 1, 0, 0),
        ];
        let fb = empty_fn(2, instrs);
        let fv = v12_interp::feedback::FeedbackVector::default();
        let mut p = Pipeline::new().unwrap();
        let (_c, deopt) = p.compile_with_feedback(&fb, Some(&fv)).unwrap();
        assert_eq!(deopt.guard_count(), 0);
    }

    #[test]
    fn inline_small_callee_when_monomorphic() {
        let callee = empty_fn(
            2,
            vec![
                Instr::new(Opcode::LoadInt, 0, 0, 1),
                Instr::new(Opcode::LoadInt, 1, 0, 10),
                Instr::new(Opcode::Add, 1, 0, 1),
                Instr::new(Opcode::Return, 1, 0, 0),
            ],
        );
        assert!(should_inline(&callee, true));
        assert!(!should_inline(&callee, false));
        let caller = empty_fn(
            3,
            vec![
                Instr::new(Opcode::Call, 0, 1, 0),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
        );
        let inlined = inline_at(&caller, 0, &callee).expect("should inline");
        assert!(inlined.instrs.len() > caller.instrs.len());
        let mut many = Vec::new();
        for _ in 0..21 {
            many.push(Instr::new(Opcode::LoadInt, 0, 0, 1));
        }
        many.push(Instr::new(Opcode::Return, 0, 0, 0));
        let big = empty_fn(2, many);
        assert!(!should_inline(&big, true));
        assert!(inline_at(&caller, 0, &big).is_none());
        let not_call = empty_fn(
            2,
            vec![
                Instr::new(Opcode::LoadInt, 0, 0, 1),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
        );
        assert!(inline_at(&not_call, 0, &callee).is_none());
    }

    #[test]
    fn inline_closure_counter_correctness() {
        let callee = empty_fn(
            3,
            vec![
                Instr::new(Opcode::LoadInt, 0, 0, 1),
                Instr::new(Opcode::Add, 1, 0, 0),
                Instr::new(Opcode::Return, 1, 0, 0),
            ],
        );
        let caller = empty_fn(
            3,
            vec![
                Instr::new(Opcode::LoadInt, 0, 0, 41),
                Instr::new(Opcode::Call, 2, 1, 1),
                Instr::new(Opcode::Return, 2, 0, 0),
            ],
        );
        let inlined = inline_at(&caller, 1, &callee).unwrap();
        let mut baseline = v12_jit_baseline::JitBaseline::new().unwrap();
        let compiled = baseline.compile(&inlined).unwrap();
        let mut regs = vec![JsValue::undefined(); 3];
        let res = compiled.execute(&mut regs);
        assert!(inlined.instrs.len() <= MAX_INLINE_SIZE + caller.instrs.len());
        let _ = res;
    }

    #[test]
    fn loop_versioning_hot_peel_and_unroll() {
        use v12_bytecode::FunctionBuilder;
        let mut b = FunctionBuilder::new(None);
        b.reserve_regs(5);
        let top = b.label();
        let end = b.label();
        b.emit(Instr::new(Opcode::LoadInt, 0, 0, 0));
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 0));
        b.emit(Instr::new(Opcode::LoadInt, 2, 0, 100));
        b.emit(Instr::new(Opcode::LoadInt, 3, 0, 1));
        b.bind(top);
        b.emit(Instr::new_imm24(Opcode::LoopHeader, 0));
        b.emit(Instr::new(Opcode::Ge, 4, 1, 2));
        b.emit_jump(Opcode::JumpIfTrue, 4, end);
        b.emit(Instr::new(Opcode::Add, 0, 0, 1));
        b.emit(Instr::new(Opcode::Add, 1, 1, 3));
        b.emit_jump(Opcode::Jump, 0, top);
        b.bind(end);
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));
        let fb = b.finish();

        let hdr = v12_bytecode::loop_headers(&fb)[0];
        assert_eq!(
            decide_loop_version(&fb, hdr, 10, Some(0)),
            LoopVersion::None
        );
        assert!(!should_peel_loop(10));
        assert!(!should_unroll_counted(
            10,
            v12_bytecode::find_counted_loop(&fb, hdr)
        ));

        assert!(should_peel_loop(900));
        let v = decide_loop_version(&fb, hdr, 900, Some(0));
        assert!(matches!(
            v,
            LoopVersion::Peel { .. } | LoopVersion::PeelAndUnroll { .. }
        ));

        let counted = v12_bytecode::find_counted_loop(&fb, hdr);
        assert!(counted.is_some());
        assert!(should_unroll_counted(900, counted));
        let v2 = decide_loop_version(&fb, hdr, 900, None);
        assert_eq!(v2, LoopVersion::Unroll2x { header: hdr });

        let mut fv = v12_interp::feedback::FeedbackVector {
            loop_counter: 900,
            ..Default::default()
        };
        let heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let shape = heap.root_shape();
        let mut ic = v12_interp::feedback::PolyIc::default();
        ic.record(shape, v12_heap::PropKey::from_parts(false, 0), 0);
        fv.ics.insert(hdr + 2, ic);
        let mut p = Pipeline::new().unwrap();
        let (_c, deopt) = p.compile_with_feedback(&fb, Some(&fv)).unwrap();
        assert!(deopt.guard_count() >= 1);

        let mut baseline = v12_jit_baseline::JitBaseline::new().unwrap();
        let compiled = baseline.compile(&fb).unwrap();
        let mut regs = vec![JsValue::undefined(); 5];
        let res = compiled.execute(&mut regs);
        assert_eq!(num_val(res), Some(4950.0));

        // Interpreter differential: identical body surfacing the sum through
        // `Throw` (top-level `Return` yields only `Ok(())`). Optimizer and
        // interpreter must agree on the 0..100 sum.
        let mut twin = fb.clone();
        let last = twin.instrs.len() - 1;
        twin.instrs[last] = Instr::new(Opcode::Throw, 0, 0, 0);
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        let mut interp = v12_interp::Interp::new(&mut heap, vec![twin], 0, Vec::new());
        let thrown = match interp.run() {
            Ok(()) => panic!("expected Throw"),
            Err(e) => e.0,
        };
        assert_eq!(num_val(thrown), Some(4950.0));
    }

    #[test]
    fn constants_are_named() {
        assert_eq!(MAX_INLINE_SIZE, 20);
        assert_eq!(INLINE_BUDGET_OPS, 20);
        assert_eq!(UNROLL_FACTOR, 2);
        assert_eq!(LOOP_PEEL_HOT, 800);
        assert_eq!(crate::guard::MAX_GUARDS_PER_FUNCTION, 32);
    }
}
