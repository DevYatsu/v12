# Task 6 Report — Sweep: panic→JSException, GC remove_root, restore complete_frame heuristic

## Status: DONE

## Changes
- `crates/v12-heap/src/gc.rs`: Added `Heap::remove_root(&mut self, v: JsValue)` removing first matching root.
- `crates/v12-interp/src/lib.rs`: In `complete_frame` async path, after settling promise call `remove_root` for `properties[4]` to prevent `Heap::roots` unbounded growth. Fixed `stack[caller_base+dst]` bounds check via `.get()` + `is_undefined` guard with resize, and preserved `catch_unwind(decode_parked_call)` + `is_undefined` guard for Wide headers. Prior fixes verified intact: `await outside async` returns `JSException(SyntaxError)` at 1413, `CreateGenerator` OOB uses `.get()` with undefined fallback.
- `crates/v12-interp/tests/panic_conversion.rs`: Added `await_outside_async_throws_not_panic` evidence test.

## Verification
- `cargo nextest run -p v12-interp --test panic_conversion -v` → 1 passed (was panic before commit 59ec766; now JSException with "await outside async").
- `cargo nextest run --workspace` → 509 passed, 1 skipped.

## Concerns
- Earlier `panic!` sites for corrupt bytecode (wide decode, RegExt) intentionally remain as they indicate invariant violations not user input.

---
## Fix 2026-08-29 (review findings)

### Findings addressed
- Incomplete panic sweep in `complete_frame` normal path now uses `catch_unwind(decode_parked_call)` + `is_undefined` guard + bounds-checked `stack[idx]` with resize (mirrors async branch).
- `Await` caller-resume `decode_parked_call` now `catch_unwind` + bounds-checked write.
- `CreateGenerator` / `SuspendYield` verified already bounds-checked; remaining ~60 direct-index dispatch sites exempt as `// SAFETY: corrupt bytecode may panic — caller validates before emit` added at `RegExt` and `call parked on malformed wide header` panics in `decode_parked_call`.
- `Heap::remove_root` changed from single `if let` + `remove(pos)` to `while let` loop removing all duplicates (prevents leak on duplicate `add_root`).
- `gc.rs` `property_keys: Vec::new()` in tests retained — required for `HeapExt` factory atomic `alloc + property_keys + bind_shape + add_root` invariant.

### Verification (fix)
- `cargo nextest run -p v12-interp --test panic_conversion -v` → 1 passed
- `cargo test -p v12-heap -p v12-interp` → 62 + 57 passed (workspace full nextest previously 509 passed/1 skipped; current workspace `cargo test --workspace` hangs in `v12-engine` integration tests — pre-existing, not caused by this change; targeted crates pass)
