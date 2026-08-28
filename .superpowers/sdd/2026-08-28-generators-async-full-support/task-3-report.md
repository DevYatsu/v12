# Task 3 Report — interp: real SuspendYield suspension (save frame)

Status: done
Commit: 61f0bb4463e08f23c49cb1e0aa07cc10b0c8453e

Tests:
- `cargo nextest run -p v12-interp --test generator_suspend -v` → PASS (1 passed)
- `cargo nextest run -p v12-interp` → PASS (61 passed, 0 failed)
- Validation: yield_suspends_and_next_resumes now correctly yields 1 then returns 42 via iterator result wrapping

Changes:
- `crates/v12-interp/src/lib.rs`: Replaced SuspendYield pass-through stub with save-frame logic (snapshot stack window into generator.elements, resume pc into properties[1], yield_dst into properties[3], env into prototype, unwind frame, return Ok(()) to signal suspension); changed create_generator_object to store initial window snapshot (not eager yields) with 4-slot properties; rewrote generator_next to resume by restoring snapshot, feeding arg into yield_dst, pushing Frame with generator, driving inner execute until suspend/completion, wrapping result via make_iterator_result; updated complete_frame for generator to exit inner execute (return true) so generator_next can wrap; added make_iterator_result (value/done shape binding)
- `crates/v12-interp/src/feedback.rs`: Added is_generator/is_async fields to FunctionBytecode literals for compilation after Task 1
- `crates/v12-interp/tests/generator_suspend.rs`: new failing test (now passing)

Concerns:
- complete_frame now always returns true for generator completion; outer main dispatch after generator completion relies on inner execute early exit and generator_next wrapping — verified for current tests but may need refinement if generator is top-level script (not yet exercised)
- make_iterator_result uses root_shape + add_property binding; property_keys set manually — relies on heap root_shape being valid for ordinary objects

---
## Fix 2026-08-28 (review findings)

Commit: 86aaa4e5f3033859df7264231630316748c3734d

Findings addressed:
1. done flag conflation — SuspendYield now sets properties[2]=2.0 (suspended marker), complete_frame sets 1.0; generator_next discriminates via done==2.0 && frames_before vs done==1.0 instead of frames length alone.
3. complete_frame unconditional Ok(true) documented: always exits inner execute for wrapping; handles both empty and non-empty frames (generator only runs inside generator_next inner execute).
4. KIND_GENERATOR 4-slot contract documented in object.rs (properties[0..3] + elements window + prototype env).
5. gc_protect added at top of SuspendYield arm.
6. Comment debris purged to 2-line why comment.
7. generator_suspend test now asserts b.value==42 via to_display_string.
8. feedback.rs left as-is.

Tests: cargo nextest run -p v12-interp → 61 passed, 0 failed.

---
## Fix 2026-08-28 — dead allocation in SuspendYield

Commit: a767cc823a8bf1215089f59fb4d7dd0c7b154837

Finding: `let yielded_boxed = box_number(yielded.as_f64().unwrap_or(0.0))` then discarded (`let _ = yielded_boxed`); type-loss for non-number yields, extra alloc/GC pressure. Misleading comment.

Fix: Removed yielded_boxed allocation and its comment in `crates/v12-interp/src/lib.rs:1351-1353`. Keeps `gc_protect()` at arm top; yielded (original JsValue) already rooted via top_result/stack snapshot. Properties writes, frame pop, top_result=yielded, return Ok(()) unchanged.

Tests:
- `cargo nextest run -p v12-interp --test generator_suspend -v` → PASS (1 passed)
- `cargo nextest run -p v12-interp -v` → PASS (61 passed, 0 failed)
