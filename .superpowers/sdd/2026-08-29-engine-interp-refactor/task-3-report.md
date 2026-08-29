# Task 3 Report — Suspendable trait

Status: done
Commit: 2e3e6b22da70cedc47de53c0d9333980fc74f34e

## Steps

1. Created failing test `crates/v12-interp/tests/suspendable.rs` (`suspend_resume_round_trip`).
2. Ran `cargo nextest run -p v12-interp --test suspendable -v` — initially passed (existing generator logic already correct), proceeded with refactor.
3. Created `crates/v12-interp/src/generator.rs` with `pub trait Suspendable { fn suspend(&mut self, dst: u16, val: JsValue) -> Result<Handle<JsObject>, JSException>; fn resume(&mut self, r#gen: Handle<JsObject>, arg: JsValue) -> Result<JsValue, JSException>; }` and impl for Interp extracting snapshot logic (stack[base..base+max_regs], resume_pc, env, yield_dst) and resume restore logic.
4. Modified `lib.rs` to `pub mod generator; use generator::Suspendable;`, replaced inline SuspendYield/Await save/restore with `self.suspend(u16::from(dst), yielded)?` and `self.suspend(u16::from(dst), arg)?` (Await does `promise_resolve_for_await` before suspend).
5. Verified `cargo nextest run -p v12-interp --test suspendable` PASS and `cargo nextest run --workspace` 507 passed, 1 skipped.
6. Committed.

## Test summary

- `cargo nextest run -p v12-interp --test suspendable -v`: 1 passed
- `cargo nextest run --workspace`: 507 passed, 1 skipped

## Concerns

- Suspend computes resume_pc as pc+1 (narrow op width 1); wide suspend not emitted — matches current narrow SuspendYield/Await. If Wide suspend ever emitted, resume_pc would need op_width param.
- Resume impl duplicates generator_next restore; T4 may want to delegate fully to trait.

---

## Fix pass (review findings)
Commit: 54e0ff8
Date: 2026-08-29

### Issues fixed
1. Hardcoded `resume_pc = pc+1`: trait now `suspend(&mut self, dst: u16, val: JsValue, resume_pc: usize)`, call sites compute `resume_pc = pc + op_width` before call — fixes Wide SuspendYield/Await corruption.
2. Dead `val` param (`let _ = val`): now stored as `self.top_result = Some(val)` inside suspend; SuspendYield relies on it (removed caller top_result set), Await overwrites with None after (payload queued).
3. Raw slot numbers: added consts GEN_PC_SLOT=1, GEN_DONE_SLOT=2, GEN_DST_SLOT=3 and box via JsValue::from_f64 instead of ops::box_number.
4. Resume duplication: now delegates to `self.generator_next(JsValue::object(gen), arg)` instead of 65-line copy-paste.
5. Prototype env overload: documented `// TODO: dedicated env slot, using prototype as env storage per Task 2 contract`.

### Tests
- `cargo nextest run -p v12-interp --test suspendable -v`: 1 passed
- `cargo nextest run -p v12-interp -v`: 67 passed
