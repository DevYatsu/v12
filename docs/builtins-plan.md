# Built-ins expansion plan (in progress)

Goal: raise test262 built-ins coverage. Hybrid architecture:

- **Pure natives** via `define_builtins!` (`NativeHandler = fn(&mut Heap, JsValue, &[JsValue])`) — everything that never calls back into JS.
- **Callback-taking methods** (Array map/filter/reduce/sort/…) intercepted at the interpreter seam (`Interp::run_callback_builtin` in `crates/v12-interp/src/call_setup.rs`), following the existing `FunctionCall`/`FunctionApply` precedent; macro entries carry a `callback_stub` install.
- **Long-term** context-centric refactor (one `&mut Cx` with heap access + call capability) deferred as a separate ADR.

## Session todo list (2026-09-06)

### Done
- [x] Macro plumbing: `install_value` (evaluates handler once at install), `Attrs::BUILTIN`, `Json`/`BooleanProto`/`String` targets in `BuiltinTargets`
- [x] Math: 34 functions + 8 constants (value-install path)
- [x] Number: prototype (toString radix, toFixed, toPrecision, toExponential, valueOf) + statics (isInteger/isSafeInteger) + 8 constants + `NumberPrim` surface
- [x] Array.prototype non-callback methods + `Array.of`/`from` + methods.rs table (~35 entries)
- [x] Callback array methods (forEach/map/filter/some/every/find/findIndex/findLast/findLastIndex/reduce/reduceRight/flatMap/sort) interp-side, wired into BOTH call seams
- [x] Object statics: assign/is/hasOwn/freeze/isFrozen/seal/isSealed/preventExtensions/isExtensible/fromEntries/getOwnPropertyNames/getOwnPropertySymbols/getOwnPropertyDescriptor/defineProperty/setPrototypeOf — migrated to shape descriptors (the `property_keys` parallel vec is NOT authoritative)
- [x] String.prototype ~25 methods + `StringPrim` surface + `String.fromCharCode`/`fromCodePoint`
- [x] Global URI functions: encodeURI/decodeURI/encodeURIComponent/decodeURIComponent (`builtins/global.rs`)
- [x] JSON.parse/stringify (`builtins/json.rs`)
- [x] Boolean.prototype toString/valueOf + `BooleanPrim` surface
- [x] Constructor `prototype` properties installed on Object/Array/String/Number/Boolean in realm.rs
- [x] **Bug fix (root cause of probe2 failure):** RegExt-merged operands — `GetGlobal`/`SetGlobal`/`CallApply`/`CopyObjectRest`/`CreateGenerator`/`SuspendYield`/`Await` arms in `execute.rs` read `instr.a()` (the Wide header's mask byte) instead of the merged `ra`/`rb`/`rc` from `decode_instr`. Symptoms only appeared past 255 registers.
- [x] Merge duplicate Array group in `define_builtins!` (unreachable_patterns warning gone)
- [x] nextest workspace gate: **569/569 pass** (includes the concurrent session's cross-realm tests)
- [x] Reconstruct work lost to an accidental `git checkout` (RealmEval arms ×5, Eval/Function seam arms, `realm_globals` in gc.rs, `is_realm_global`/`realm_global_intrinsic_read` in globals.rs, ic_lookup intrinsic fallback)

### In progress
- [x] **Harden array natives against huge-length/sparse-array hangs.** `array_len` returns the length *property*; scan loops (`indexOf`, `lastIndexOf`, `includes`, `splice` pre-clamp done, `fill`, `array_from`, `callback_len` in call_setup.rs) spin billions of iterations on `{length: 2**32-1}`-style receivers. `dense_bound` helper added in `crates/v12-engine/src/builtins/array.rs:175` (max of element-store len and shape-bound integer keys, clamped to len) — wired into the scan loops. Verified 2026-09-06: Array slice now completes, 712/3 332 (21.4 %).

### Pending
- [ ] `git diff` review of the reconstructed concurrent-session code (re-implementation, not byte-identical restore) before committing
- [x] Conformance slices + record numbers (2026-09-06, `--jobs 8`, human format): Array 712/3 332 (21.4 %, now completes), Math 95/327 (29.1 %, was 10.1 %), Number 137/340 (40.3 %, was 18.6 %), Object 552/3 414 (16.2 %, was 1.7 %), String 346/1 341 (25.8 %, was 7.7 %), JSON 28/165 (17.0 %, was 11.7 %), Boolean 13/51 (25.5 %, was 22 %), global 14/29 (48.3 %). Nextest gate 569/569.
- [x] Update CONTEXT.md + conformance/fix-log.md with new pass rates and the RegExt fix entry
- [x] Known small fix: `map` results display as `[object Object]` in console.log (display-string path for arrays) — landed via array_join_text in ops.rs/helpers.rs/display.rs

### Follow-ups (implemented, pending full-gate + conformance after overflow lane)
- [x] Symbol constructor + statics (81 failing tests) — symbol.rs, SymbolPrim, realm wiring; slot contract verified
- [x] Iterator prototype methods (653 failing) — toArray/take/drop/from + callback helpers via seam
- [x] Map/Set prototype additions: forEach/clear/entries/keys/values — structural surface + iterator wrappers

## Incident log
- **Accidental `git checkout`** of execute.rs/call_setup.rs/globals.rs/property.rs/gc.rs mid-session (intended only to drop debug eprintlns) reverted ALL uncommitted changes in those files, including the concurrent session's uncommitted cross-realm work. Reconstructed from context; gate green. Lesson: revert specific hunks, never whole files shared with a concurrent editor.

## Key invariants (learned the hard way)
- `ordinary_define_own_property` maintains shape + properties but NOT `property_keys` — enumerate via `heap.get(shape).descriptors`.
- Array element store can be shorter than the length property (sparse arrays) — clamp all `elems[..]` slicing and scan loops.
- Natives must not loop `0..array_len()` unbounded — deadline doesn't reach them.
- `map_set_method` allocates a fresh rooted function per method read — surfaces never yield undefined.
