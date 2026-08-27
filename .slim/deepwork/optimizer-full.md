# Deepwork: Full Tier-2 Optimizer — hand-in-hand with baseline JIT

## Goal

Turn `v12-jit-opt` from a scaffold (1970cb6) into a full optimizing JIT that works hand-in-hand with `v12-jit-baseline`:

- Baseline collects type feedback (shapes, type tags) via `FeedbackVector` and tier-up counters
- Optimizer consumes that feedback, speculatively specializes hot functions (Smi vs double, monomorphic shapes, inline small callees, version loops), emits guards that fail-closed to deopt trampolines, and hands `CompiledFn` back to `JitCache`
- Engine driver orchestrates `baseline → opt` transition on second tier-up, with deopt backoff

Plus proper tests: inventive, boundary-covering, differential against interpreter+baseline, no magic values, `#![forbid(unsafe_code)]` where possible.

## Confirmed research context

- Cranelift 0.135 with user stack maps (2024) — every call is a safepoint, references reloaded after call, supports moving GC (but we are non-moving, so simpler)
- Baseline `JitCache` keyed by `FunctionId`, `PcMapEntry jit_pc→bc_pc` already 1:1, `GuardKind::{ShapeEq,TypeIsNumber,ValidityCell}` and `should_speculate(entry>512||loop>800 && is_mono)` gating already in scaffold
- `v12-bytecode` ISA is 49 opcodes (In/InstanceOf added), fixed-width 32-bit, wide escape for large immediates
- `v12-heap` handle-based, non-moving mark-sweep, 14 global intrinsics, `GLOBAL_VAR_OFFSET` 14
- `v12-interp` feedback is per-function `FeedbackVector { ics: HashMap<pc, MonoIc>, loop_counter, entry_counter }` with saturating counters
- Existing optimizer scaffold: `OptCompiler::compile(&mut self, fb, id)`, `JitOpt { inner: Option<Pipeline> }`, `DeoptMap { pc_map, live_regs }`, 5 files, 206 lines, 2 dead_code warnings until driver wires

Files:
- `crates/v12-jit-opt/src/{lib,guard,deopt,compile,stub}.rs`
- `crates/v12-jit-baseline/src/{lib,compiler,cache,mmap}.rs`
- `crates/v12-interp/src/{lib,feedback}.rs`
- `crates/v12-bytecode/src/lib.rs`
- `crates/v12-engine/src/engine.rs` (driver)

## Plan — 3 phases, 3 Oracle gates

**Phase 1 — Speculative type system & guard emission**
- Extend `FeedbackVector` to record per-op type tags (Smi/Double/String/Object) and shape history
- Implement type lattice `Lattice::{Unknown, Smi, Double, String, Object(Shape), Any}` with join/meet, lattice-driven specialization
- Emit `GuardKind` checks as Cranelift `if` + deopt branch, record `Assumption{bc_pc, guard}` in `DeoptMap`
- Tests: guard hit/miss, lattice join, type feedback collection

**Phase 2 — Optimizing pipeline: SSA, inlining, loop versioning**
- Build SSA form from bytecode (one block per bytecode op → Cranelift block, phi for loop headers)
- Type-specialized arithmetic (Smi fast path + double fallback with guard, no call to `ops::` helpers on hot path)
- Inline small callees (≤ 20 bytecode ops, monomorphic) by inlining their blocks
- Loop versioning: peel first iteration with shape guard, unroll 2x when loop counter is hot
- Tests: Smi vs double specialization, inlining correctness, loop versioning

**Phase 3 — Integration, tier-up, and comprehensive tests**
- Wire `v12-engine` driver: `on_tier_up` first fire → baseline, second fire → `JitOpt::compile` if `should_speculate`, `JitCache` insertion, deopt trampoline back to baseline/interp with backoff
- Add `v12-cli --jit-opt` flag and `Engine` config to enable/disable tier-2
- Comprehensive tests: differential `interp` vs `baseline` vs `opt` on 20 programs (closures, loops, polymorphic ICs, deopt triggers), fuzz with `proptest`, Test262 language subset delta, bench with `criterion` for Smi vs double hot loops
- Docs: update `README.md` roadmap and `docs/language-coverage-plan.md` with optimizer tier

## Delegation

| Phase | Specialist | Ownership |
|-------|------------|-----------|
| 1 | @fixer (1 lane) | `v12-jit-opt/src/guard.rs`, `v12-interp/src/feedback.rs` — type lattice + guard emission |
| 2 | @fixer (1 lane) | `v12-jit-opt/src/compile.rs`, `v12-bytecode/src/lib.rs` (SSA helpers), `v12-jit-opt/src/deopt.rs` — SSA + inlining |
| 3 | @fixer (1 lane) | `v12-engine/src/engine.rs`, `v12-cli/src/main.rs`, `v12-jit-opt/src/lib.rs` (driver wiring) + test suite |

All phases use `cargo nextest`, `cargo clippy --all-targets`, `cargo fmt`, `#![forbid(unsafe_code)]` where possible, named constants, inventive tests.

## Oracle reviews

Total: 3 reviews, one after each phase.

- **Gate 1 (after Phase 1):** Guard fail-closed correctness and type lattice soundness — prevents deopt bugs that would corrupt later phases.
- **Gate 2 (after Phase 2):** SSA and inlining correctness — ensures speculative code is semantics-preserving before wiring tier-up.
- **Gate 3 (after Phase 3):** Integration and test coverage — verifies hand-in-hand baseline→opt handoff and that no `test/language` regresses.

## Status

- [x] Phase 1 — completed 2026-08-27 (lattice + guards, 65 tests)
- [x] Oracle 1 — PASS with 5 minor notes (2026-08-27): lattice diamond sound, fail-closed stub safe via baseline fallback, is_mono vacuous, threshold mismatch documented, String concrete unguarded — all fix-in-Phase-2, no blocker
- [x] Phase 2 — completed 2026-08-27 (SSA + Smi diamond + inlining + loop versioning)
- [ ] Oracle 2
- [ ] Phase 3 — not started
- [ ] Oracle 3
- [ ] Final validation

## Validation results

### Phase 1 — 2026-08-27
- `cargo fmt --check` clean
- `cargo clippy -p v12-jit-opt -p v12-interp --all-targets` 0 warnings
- `cargo nextest run -p v12-jit-opt -p v12-interp` 65 passed: 13 feedback lattice/join/meet + proptest, 7 guard hit/miss/speculation, 4 compile guard emission/differential, 3 deopt
- Lattice: `Unknown, Smi, Double, Number, String, Object(Shape), Any` with diamond join Smi+Double=Number, String+Number=Any, MAX_GUARDS_PER_FUNCTION=32, guard budget enforced
- Guards: `ShapeEq, TypeIsSmi, TypeIsNumber, ValidityCell`, `Assumption::check_value/check_shape`, `should_speculate` + `should_speculate_with_lattice` using `!is_concrete`
- Feedback: `type_feedback: HashMap<u32, Lattice>` + `record_type/type_at`, collection in Add/Sub/Mul/GetProperty, fixed non-foldable test case
- Files touched: `crates/v12-interp/src/feedback.rs`, `crates/v12-jit-opt/src/guard.rs`, `crates/v12-jit-opt/src/compile.rs` (guard_for_lattice/emit_guard), `crates/v12-jit-opt/src/deopt.rs`

### Phase 2 — 2026-08-27
- SSA: one Cranelift block per bytecode op (`build_ssa_ir`), phi-via-variables for loop headers (`Jump`/`LoopHeader`), explicit block sealing for loop headers after backedge discovery. Helpers live in `v12-bytecode` (per plan delegation): `instr_width`, `logical_pcs`, `next_logical_pc`, `is_loop_header`, `loop_headers`, `find_counted_loop`/`CountedLoop` (backedge + exit + canonical self-increment induction), `is_inline_candidate`, `MAX_INLINE_SIZE=20`
- Smi fast path: actual `brif` diamond (`fast → range-check → box | fallback→generic helper`), tag check via BOX_MASK/TAG/SPARE masks, i32 add with overflow + Smi-range branch to double fallback; no `ops::` call on hot path
- Inlining: `inline_at` splices callee body (≤ MAX_INLINE_SIZE logical ops, terminated by Return/Throw) at Call/CallW; `should_inline` requires clash-aware mono (`MonoTracker`/`ClashCounter`; TODO(poly IC) documented)
- Loop versioning: `decide_loop_version` wired into `Pipeline::compile_with_feedback` — peel first iteration (`ShapeEq` guard on loop-carried IC shape), unroll `2×` for counted hot loops, `ValidityCell` guard recorded for peeled headers
- Oracle 1 fixes: guard emission single-site in Pipeline (no duplicate from SSA builder); is_mono clash counter present + doc'd; threshold mismatch note (`FEEDBACK_THRESHOLD_MISMATCH_NOTE`, 512/800 vs 1024); `String` lattice emits `TypeIsString`
- Also: made `v12-interp::feedback` public (guard.rs re-exported `Lattice` through it but the module was private — Phase-1 wiring gap); ungated `Interp::feedback_vector` accessor (tier-up driver needs it)
- Files touched: `crates/v12-jit-opt/src/{compile,deopt,guard}.rs`, `crates/v12-bytecode/src/lib.rs`, `crates/v12-interp/src/{lib,Cargo.toml}`; reverted a non-compiling half-finished edit in `crates/v12-jit-baseline/src/compiler.rs` (Arc exec-closure attempt that broke `[Const]` indexing) left behind mid-flight

## Open questions

- Should `should_speculate` threshold remain 512/800 or be tuned via `bench/` hyperfine?
- Does `JitOpt` need its own `JitCache` or share baseline's? Decision: share via `v12-engine` driver, not duplicate.

