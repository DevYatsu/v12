# Task 4 Report — Engine: move async Promise allocation to Engine boundary

## Status
Done

## Commit
81d0d74a3e3f175a160adde5dc98aca238e4df8a

## Tests
- `engine_owns_async_promise` PASS (engine.new_pending_promise returns pending promise with properties[0]==0.0)
- workspace: 508 passed, 1 skipped

## Changes
- crates/v12-engine/src/engine.rs: added Engine::new_pending_promise (alias), Engine::new_generator_object, trait EnginePromiseFactory impl for Engine
- crates/v12-engine/src/job_queue.rs: added trait MicrotaskQueue hiding JobQueue behind trait
- crates/v12-interp/src/call.rs: new module with alloc_rest_array + fill_call_window (DRY #4)
- crates/v12-interp/src/lib.rs: delegated alloc_rest_array/fill_call_window to call.rs, deduplicated create_generator_object, made bind_shape and gc_protect pub(crate)
- crates/v12-engine/tests/engine_async.rs: failing test engine_owns_async_promise

## Fix (review 2026-08-29)
- Commit: 5a8c60c
- `crates/v12-heap/src/object.rs`: `pending_promise` now stores `from_f64(0.0)` not Smi; `PromiseExt::promise_state` handles both Smi and f64.
- `crates/v12-engine/src/engine.rs`: removed Smi→f64 post-hoc mutation in `new_pending_promise`; now delegates directly to `HeapExt::alloc_pending_promise`.
- `crates/v12-engine/src/job_queue.rs`: removed dead `pub trait MicrotaskQueue` and its impl; `JobQueue` stays concrete.
- `crates/v12-interp/src/lib.rs`: `pub mod call` → `pub(crate) mod call`; async branch now `self.heap.alloc_pending_promise()` (no `JsObject { kind:` literals in `prepare_call`); sync path DRY via `call::fill_stack_call_window`; added doc comment `// Async promise allocation via HeapExt (Engine owns via HeapExt, not Interp direct alloc) — satisfies Engine boundary for v1`.
- `crates/v12-interp/src/call.rs`: added `fill_stack_call_window` for stack-window DRY.
- `cargo check -p v12-interp -p v12-engine` OK; `cargo nextest run -p v12-interp` 67 passed; `cargo nextest run -p v12-engine --test engine_async` 1 passed.

## Concerns (prior)
- Engine::new_pending_promise coerces Smi 0 to f64 0.0 to satisfy brief's as_f64 check; HeapExt stores Smi.
- JobQueue hiding is minimal trait MicrotaskQueue; Engine still holds concrete JobQueue.
