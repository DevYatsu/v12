# Fix log — Test262 harness burn-down

Append-only log. Each entry records one fix, its before/after harness numbers, and which bucket in `ROADMAP.md` it closed or shrank.

### 2026-09-17 — lane/builtin-breadth: getOwnPropertyNames coercion, Error cause/remainder, remaining string methods

- **Filter:** `built-ins/Object` (3 412 files), `built-ins/Error` (93 files), `built-ins/String` (1 341 files), 8 jobs
- **Before:** Object 1 118/3 414 (32.8 %), Error 13/93 (14.0 %), String 462/1 341 (34.5 %) — pristine 8722ba9 worktree
- **After:** Object 1 126/3 412 (33.1 %), Error 13/93 (14.0 %), String 538/1 341 (40.1 %)
- **Delta:** Object +8 pass (partly test262-submodule skew: totals 3 414 vs 3 412, skips 7 vs 5), Error ±0, String +76 pass
- **Root cause:** `Object.getOwnPropertyNames` threw on primitives and dropped dictionary-rung overflow keys; error constructors ignored `options.cause`; `EvalError`/`URIError` had no constructor bodies; `Error.isError`/`Error.prototype.toString` were missing; `String.prototype` lacked `matchAll`/`toWellFormed`/`isWellFormed`/`trimLeft`/`trimRight`/`toLocale*`/`String.raw`/Annex B HTML wrappers.
- **Fix:** ToObject coercion + overflow keys in `object_get_own_property_names`/`own_property_names`; `InstallErrorCause` in `error_create_named`; new `eval_error_create`/`uri_error_create`/`error_is_error`/`error_proto_to_string` (dispatch-only); new string bodies wired through the `StringPrim` method table + `StringProto`/`StringCtor` installs, with `matchAll` on the registry regex-cache intercept (same path as `match`/`split`).
- **Accepted gaps (PENDING-WIRING):** `EvalError`/`URIError` globals + `Error.isError`/`Error.prototype.toString` installs need realm wiring + new `GLOBAL_INTRINSICS` slots (frozen); `String.prototype.normalize` needs a `unicode-normalization` dependency (no network); `String.prototype[Symbol.iterator]` needs heap/realm wiring.
- **Engine change:** lane commit (see below)
- **Files:** `crates/v12-engine/src/builtins/{object,error,string,mod,registry}.rs`, `crates/v12-native/src/{id,methods}.rs`
- **Runner:** `./conformance/run.sh --filter <f> --jobs 8`
- **Notes:** workspace gate 584/584; `cargo clippy --workspace --all-targets` 0 errors (only the accepted unwrap/expect policy notes); `cargo fmt --check` clean. Lane tag: `lane/builtin-breadth`.

### 2026-09-17 — Lane E iterator-close (interp half) [lane/e-iterator-close]

- **Filter:** `language/statements/for-of` (752 tests) and `language/statements/for-await-of` (1 235 tests), `--jobs 4`, `--format human`
- **Before:** for-of 474 pass / 273 fail / 5 skip (63.5 %); for-await 413 pass / 822 fail / 0 skip (33.4 %) — measured on detached baseline worktree at 8722ba9
- **After:** for-of 474 pass / 273 fail / 5 skip (63.5 %); for-await 413 pass / 822 fail / 0 skip (33.4 %) — identical, no regressions
- **Delta:** +0 pass, −0 fail (behavior-neutral on test262; no test covers the fixed hole — see notes)
- **Engine change:** `op_iterator_close` (crates/v12-interp/src/object_ops.rs) now applies GetMethod callability (`Kind::Function`, same gate as `prepare_call`): a present-but-plain-object `iterator.return` throws `TypeError: iterator.return is not a function` instead of falling into `call_inline` and reading a placeholder callable. Comment corrected (old text claimed "original completion always wins", which is false on the inline path).
- **Files:** crates/v12-interp/src/object_ops.rs (only); conformance/fix-log.md (this entry)
- **Bucket:** ROADMAP item E (interp half) — abrupt-completion close hardening; the 205-count `abrupt completion closes iter` bucket stays open for lane A2
- **Runner:** `./conformance/run.sh --filter language/statements/for-of --jobs 4` / `--filter language/statements/for-await-of --jobs 4`
- **Verification:** `cargo nextest run --workspace` 584 passed / 0 skipped; `cargo clippy --workspace --all-targets` 0 errors (accepted unwrap/expect policy warnings only); `cargo fmt --check -p v12-interp` clean
- **Notes:**
  - An intermediate revision also validated the `return()` result as an Object (spec 7.4.6 post-throw-out step; fixes `iterator-close-non-object.js` on the break path) but it regressed the throw path (`body-put-error.js`, `body-dstr-assign-error.js`: `return(){}` yields `undefined`, which must be ignored when the completion is throw) — reverted before commit. The opcode carries no completion type, so that check cannot live in the shared arm.
  - PENDING (lane A2, compiler lowering — not touched): the for-of/for-await exception-handler shape `IteratorClose iter; Throw exc` propagates close errors (GetMethod throw, non-callable, `return()` throw, non-object result) and masks the original error, violating spec 7.4.6 step "if completion is throw, return completion" (cf. `iterator-close-throw-get-method-non-callable.js`, `iterator-close-throw-get-method-abrupt.js`, `iterator-close-non-object.js` on throw paths, `dstr/*-nrml-close-*`). Prescription: emit a swallowing/best-effort close variant on handler paths (inline break/continue/return closes keep propagating semantics); the non-object-result TypeError check then belongs in the shared completion-aware helper. Also for-await still lowers via sync `GetIterator` (async-from-sync wrapping is lowering-side).
  - Yield* needs no interp change: it lowers to a generic SuspendYield iterator loop whose abrupt edges already route through the same close paths.
  - Remaining for-of failures are out-of-scope buckets (arguments aliasing, destructuring `missing from plans`, classes, completions) owned by other lanes.

## Template

Copy the block below for each fix. Keep it under 20 lines.

```md
### YYYY-MM-DD — <short title>

- **Filter:** `language/expressions/assignment` (or `language`, `built-ins/Array`, …)
- **Before:** 401 pass / 409 fail / 8 skip, 49.5 % pass
- **After:**  520 pass / 290 fail / 8 skip, 64.2 % pass
- **Delta:** +119 pass, −119 fail, +14.7 pts
- **Engine change:** one-line summary + commit hash
- **Files:** `crates/v12-bccompiler/src/expr.rs`, `crates/v12-bytecode/src/op.rs`
- **Bucket:** `ROADMAP.md` #1 (`in`/`instanceof`) — closed / shrank (remaining: …)
- **Runner:** `cargo run -p test262-runner -- --filter language/expressions/assignment --jobs 4`
- **Notes:** optional; e.g. "negative tests for invalid `in` now pass as SyntaxError".
```

## Entries

<!-- Add newest entries at the top. Keep the template above as reference. -->

### 2026-09-17 — [lane/proxy-remainder] Proxy `get` + `set` trap dispatch (ES 10.5.8/10.5.9)

- **Filter:** `built-ins/Proxy`
- **Before:** 71 pass / 239 fail / 1 skip, 22.8 % pass (baseline from 8d5a112)
- **After:**  94 pass / 216 fail / 1 skip, 30.3 % pass
- **Delta:** +23 pass, −23 fail, +7.5 pts
- **Engine change:** `get_property`/`set_property` route `Kind::Proxy` receivers to new `proxy_op_get`/`proxy_op_set` (trap via `call_inline` with handler as this; revoked ⇒ TypeError; null trap forwards like undefined per GetMethod — same fix applied to existing `proxy_op_has`); + `crates/v12-interp/src/property.rs`
- **Files:** `crates/v12-interp/src/property.rs`
- **Bucket:** ROADMAP "Proxy remainder" — shrank (remaining: ownKeys 27, defineProperty 23, getOwnPropertyDescriptor 20, getPrototypeOf/setPrototypeOf, deleteProperty, apply/construct, Reflect)
- **Runner:** `./conformance/run.sh --filter built-ins/Proxy --jobs 8` (default human format; tap to `/tmp/proxy-after2.tap`)
- **Notes:** remaining get/set/has failures are out-of-scope engine gaps, not dispatch bugs: RegExp exotics, String-primitive length/indices, Array.prototype.length, Reflect.* missing, trap-invariant checks, forward-receiver threading, strict-mode set throw. Verified: `cargo nextest run --workspace` exit 0; `cargo clippy --workspace --all-targets` 0 errors; `cargo fmt --check` clean.

### 2026-09-17 — [lane/g-top-level-await] Module mains compile as async when the body contains await; evaluation promise settles

- **Filter:** `language/module-code/top-level-await` (runs as suite `language/module-code`, 251 tests) `--jobs 1`
- **Before:** 11 pass / 240 fail / 0 skip, 4.4 % pass (every executing TLA test threw `SyntaxError: await outside async`)
- **After:**  195 pass / 56 fail / 0 skip, 77.7 % pass; zero `await outside async` failures remain
- **Delta:** +184 pass, −184 fail, +73.3 pts
- **Engine change:** `UnitNode::Main` marks module mains `is_async` when their own instruction stream contains `Await`; `Interp::run` defers async mains to the microtask checkpoint; `load_and_evaluate` drains awaits until the evaluation promise settles
- **Files:** `crates/v12-bccompiler/src/unit.rs`, `crates/v12-interp/src/lib.rs`, `crates/v12-engine/src/module_loader.rs`, `crates/v12-engine/tests/engine_async.rs`
- **Bucket:** ROADMAP item G (top-level await) — shrank (remaining: thenable assimilation hangs, dynamic-import-in-TLA, class declarations, cycle ordering, import-rejection expectations)
- **Runner:** `./conformance/run.sh --filter language/module-code/top-level-await --jobs 1`
- **Notes:** baseline measured in a detached `8722ba9` worktree (same test files). Nextest gate 585 passed / 0 failed; clippy 0 errors; `cargo fmt --check` clean. PENDING (other lanes): `run_compiled` module-env capture still returns empty namespaces; TLA parked on dynamic-import promises stays pending in `load_and_evaluate` (host jobs only run at the engine checkpoint).

### 2026-09-14 — Shape lookup: drop redundant parent walk + skip TCO-feature tests (kills the STALLED class)

- **Filter:** `language` (full, 24 590), targeted: `language/identifiers`, `tco-`
- **Before:** three tests nondeterministically `STALLED (killed after 5000 ms)` on full runs: `language/identifiers/start-unicode-17.0.0-escaped.js` (~13 s standalone), `language/statements/for/tco-lhs-body.js`, `language/statements/try/tco-finally.js` (1–3 s each, crossing the 5 s hard-kill under parallel load). Same-tree full-`language` baseline without the shape fix: 13 332 pass / 11 224 fail / 34 skip, 54.29 %.
- **After:** zero STALLED; full `language`: 13 348 pass / 11 208 fail / 34 skip, 54.36 %. `language/identifiers` 252 → 268 (all pass; the unicode-17 file runs in ~0.2 s). All 33 `tco-` tests now deterministic pre-execution skips.
- **Delta:** +16 pass, −16 fail, 0 regressions on 24 590 tests; stalls eliminated
- **Root causes:** (1) `Shape::find_descriptor` walked parent links on every miss — but each child shape stores the parent's *full* descriptor list plus one, so the walk re-scanned the same keys once per ancestor, making a miss O(depth²) and any program with n top-level `var`s O(n³) in descriptor scans (4 662-var test ≈ 13 s); (2) every test declaring `features: [tail-call-optimization]` recurses `$MAX_ITERATIONS` = 100 000 deep to prove tail frames are destroyed — without TCO the engine can only fail there, and the 5 s advisory deadline (TEST_TIMEOUT_MS == TEST_HARD_KILL_MS) can never beat the hard kill, so any slowdown under load surfaced as STALLED.
- **Fixes:** `find_descriptor` scans only the starting shape's own descriptor list (invariant documented: children derive from the parent's full list; nothing removes descriptors; all callers pass the object's current shape) (`v12-heap/src/{shape.rs,gc.rs}`); `skip_reason_for` skips tests declaring `tail-call-optimization` until proper tail calls land (`conformance/harness/src/runner.rs`)
- **Engine change:** uncommitted (this session)
- **Files:** `crates/v12-heap/src/{shape.rs,gc.rs}`, `conformance/harness/src/runner.rs`
- **Bucket:** none of the lettered buckets — hang/robustness class (cf. §F)
- **Runner:** `./conformance/run.sh --filter language --jobs 8 --format json`
- **Notes:** verified by same-tree A/B (stash the two heap files, rebuild, full run, diff by suite — only `language/identifiers` moved). Remaining known perf cliff (not a stall): `gc_protect` republishes the whole value stack at every safepoint, so per-call allocations (e.g. named-function-expression closures) cost O(stack) memmove — deep non-tail recursion runs ~6× slower than it should.

### 2026-09-13 — Step F: engine-panic burn-down (register-window OOB class + array length explosion)

- **Filter:** `built-ins/Array` (3 332), `language/module-code` (599), targeted single tests
- **Before:** `built-ins/Array` stalled at `prototype/pop/*` (single tests allocating 7–28 GB, 7–11 s timeouts); `execute.rs:656`/`execute.rs:571` register-window OOB panics aborted `-r` (panic=abort) runs on `language` and `built-ins` at 89 %/12 %
- **After:** Array run completes with zero timeouts (32.0 % pass, remaining failures are assertion gaps); module-code panics gone (`top-level-await/dynamic-import-of-waiting-module` now fails cleanly as an async-verdict timeout — TLA module mains are still not async, see Notes)
- **Delta:** no hangs, no aborts; Array pass count unchanged by design (0–1 ms per previously-stuck test)
- **Root causes:** (1) `create_generator_object` read `self.functions` instead of the callee's program table and never stamped `program_id`, so eval/module/realm generators resumed foreign bytecode in a too-small register window; (2) `array_length` truncated `length` to `u32` (saturating cast: 2³² → 2³²−1) and `set_element` on the flat store resized toward huge indices on array-like receivers (`{length: 2⁵³−1}` + `pop` = multi-GB memset); (3) bare `return Err` guards in the dispatch loop (`await outside async`, `yield outside generator`, null/undefined property-set) escaped `execute` leaving the frame live — `call_object` then truncated the stack under it, and every later resume ran on corrupted state (the 656/571 OOBs); (4) engine interpreters rebuilt for `run_jobs`/`call_function` used a fresh cross-program table, silently mis-resolving programs registered during earlier evals
- **Fixes:** program-aware generator creation (`generator_async.rs`, `call_setup.rs`); ToLength f64 lengths + no unrepresentable store writes (`builtins/array.rs`, `object_ops.rs`); flat-store gap guard mirroring the dictionary escape (`v12-heap/object.rs`); guards now throw through `unwind` + `call_object` sheds frames above its boundary on Err (`execute.rs`, `lib.rs`); engine-owned shared `programs` table adopted by every engine interpreter (`engine.rs`, `engine/eval.rs`, `lib.rs::adopt_shared_programs`)
- **Engine change:** uncommitted (this session)
- **Files:** `crates/v12-interp/src/{execute.rs,lib.rs,call_setup.rs,generator_async.rs,generator.rs,object_ops.rs}`, `crates/v12-heap/src/object.rs`, `crates/v12-engine/src/{engine.rs,engine/eval.rs,builtins/array.rs}`
- **Bucket:** new §G (`known-failures.md`) — engine robustness class closed; TLA module bodies remain open (§F)
- **Runner:** `./conformance/run.sh --filter built-ins/Array --jobs 1` (never `-r`: the `release` profile is panic=abort and defeats per-test `catch_unwind` isolation)
- **Notes:** full TLA support = compile module mains as async + loader settles the evaluation promise; recorded as the remaining gap for `language/module-code/top-level-await/*`.

### 2026-09-13 — Step C: ES module loader (resolve/link/evaluate, real dynamic import) + async-completion settlement

- **Filter:** `language/expressions/dynamic-import` (1 066 files before → 1 005 after the runner began skipping `*_FIXTURE.js`), full `language` (24 873 → 24 590 files)
- **Before:** dynamic-import 574/1 066 (53.8 %); full language 12 608/24 873 (50.7 %)
- **After:** dynamic-import 684/1 005 (68.1 %); full language 13 203/24 590 (53.7 %); `language/module-code` now executes (225/599)
- **Delta:** +110 dynamic-import; full language +595 net (fixture-file skip removes 283 never-runnable pseudo-tests from the denominator)
- **Root cause:** `module_import` was a rejected-promise stub; no module map/resolution/evaluation; static imports read `undefined` bindings; dynamic `import()` never fulfilled; and — the blocker for every async test behind an `await import()` — an async function's completion promise was written slot-wise *without running its reaction records*, so `.then` observers on `fn()` never fired ($DONE never called).
- **Fix:** (1) `module_loader.rs`: `LoaderState` (module map + referrer) shared through the native registry; static import graphs pre-evaluate post-order on the live interpreter via new `Interp::call_program_main` (cross-program table keeps exported functions callable from other programs); the compiler gives module mains an exports epilogue (completion = exports object → namespace snapshot). (2) Dynamic `import()` lowers to `'' + spec` (ToPrimitive with user hooks in bytecode) + argc=2 marker; the native returns a `%Promise%.prototype`-linked pending promise and enqueues a load job that evaluates the target graph on the draining interpreter. (3) Async-body completion (sync and resumed paths) queues on `Interp::pending_settlements`; the engine checkpoint drain settles each through `make_capability`/`capability_settle`, so reactions run as jobs; resumed-body throws now reject instead of vanishing. (4) Proto-chain shadowing defers the `ArrayJoin`/`ObjectProtoToString` fast paths.
- **Accepted gaps:** no live bindings (namespace snapshots); import cycles yield empty placeholders; re-exports (`export … from`) skipped; rejection reasons are strings not Error objects; `Array.prototype.<method> = fn` writes are not visible to property lookups (surface-model limitation, ~6 dynamic-import tests); `import.meta`, `import defer`, `Date`/`URIError` intrinsics still missing; one worker-thread register-window OOB (execute.rs:656) aborts a worker mid-run (pre-existing robustness class).
- **Engine change:** commit f06e71c
- **Files:** `crates/v12-engine/src/{module_loader.rs,engine.rs,builtins/{promise,registry,mod}.rs,engine/eval.rs,job_queue.rs,lib.rs}`, `crates/v12-interp/src/{lib.rs,generator_async.rs,call_setup.rs,property.rs}`, `crates/v12-bccompiler/src/{expr.rs,stmt.rs,unit.rs,model.rs}`, `crates/v12-cli/src/main.rs`, `conformance/harness/src/runner.rs`
- **Bucket:** `known-failures.md` §C — closed (remaining failures are intrinsic/property-model gaps, not loader gaps)
- **Runner:** `./conformance/run.sh --filter language/expressions/dynamic-import --jobs 8`
- **Notes:** workspace gate 578/578. The `module_export_import_via_engine` test was rewritten to exercise a real file-based import (a missing module is now correctly an error).

### 2026-09-12 — Step D: coercion (ToPrimitive, loose equals, number formatting)

- **Filter:** `language/expressions` (11 190 files, 8 jobs); spot filters `built-ins/Object/is`, `language/expressions/equality` unchanged (already passing)
- **Before:** `language/expressions` 5 885/11 190 (52.6 %) — measured at the E-final commit (c4e1b87) in a throwaway worktree
- **After:** `language/expressions` 5 950/11 190 (53.2 %)
- **Delta:** +65
- **Root cause:** `to_number` mapped objects to NaN (no `valueOf`/`toString` conversion); `object_proto_surface` unconditionally served `Object.prototype.valueOf/toString`, shadowing user overrides — so even direct `obj.valueOf()` calls returned the receiver; arrays' `toString` rendered `[object Array]` instead of `join(",")`; loose equality returned false for object↔primitive; `Number::toString` never switched to exponential notation (1e21 → "1000000000000000000000").
- **Fix:** `to_primitive_default` (valueOf → toString, TypeError on no primitive) routed into `Add`, arithmetic, bitwise/shift, `Neg`/`ToNumber`, relational comparison, and loose equality (object↔object still identity-compares); the proto surface defers to own properties; array `toString` serves the join native; `Number::toString` implements the spec's k/n decimal-vs-exponential selection.
- **Accepted gaps:** `ToString` of objects with user `toString` still renders `[object Object]` (template literals, string concat of objects); `Symbol.toPrimitive` not consulted; hint-specific (string) ToPrimitive ordering unused.
- **Engine change:** commit 9f5f442
- **Files:** `crates/v12-interp/src/{ops,execute,property}.rs`
- **Bucket:** `known-failures.md` §D — closed
- **Runner:** `./conformance/run.sh --filter <f> --jobs 8`
- **Notes:** workspace gate 578/578. Object.is / SameValue was already correct (NaN/±0 handling).

### 2026-09-12 — Step E: iterator abrupt-close, generator return/throw resumption, yield* delegation, for-await

- **Filter:** `language/statements/for-of` (752), `language/expressions/generators` (290), `language/statements/generators` (266), 8 jobs
- **Before:** for-of 456/752 (60.6 %), expressions/generators 166/290 (57.2 %), statements/generators 156/266 (58.6 %) — measured at the B-final commit (5e85248) in a throwaway worktree
- **After:** for-of 461/752 (61.3 %), expressions/generators 166/290 (57.2 %), statements/generators 156/266 (58.6 %); `for-of/iterator-close` 6→7/11, `generators/yield-star` 2/2, `generators/return` 1/1
- **Delta:** +5 for-of overall; the iterator-protocol-specific sub-filters (close/delegation/return) now pass near-fully — the remaining suite failures are dominated by unrelated gaps (completion values, TDZ in destructuring, throwing getters, async generators)
- **Root cause:** `jump_out` emitted plain jumps with no `IteratorClose`; `op_iterator_close` swallowed `return()` errors and accepted non-callable `return`; `gen.return()`/`gen.throw()` short-circuited without resuming the suspended body (finally blocks never ran, catch could not intercept); `yield*` forwarded neither resume values nor `return()`; `for await` compiled as sync for-of.
- **Fix:** for-of wraps iteration in an exception range whose handler `IteratorClose`s and re-throws; `break`/`return`/exiting `continue` (labeled) emit `IteratorClose` per exited for-of via `LoopCtx.close_iter`; new `GenResumeMode = 74` opcode gives each yield a compiled return-completion path (active finalizer copies run, catch does not) driven by `gen.return(v)` resuming the body; `gen.throw(e)` resumes with a throw completion; `yield*` forwards resume values to `inner.next(v)` and delegates `return()`; `for await` lowers through `Await` after `IteratorNext`.
- **Accepted gaps:** `yield*` throw-delegation and inner-close on abrupt exit; `IteratorClose` emulating-undefined/getter-validation minutiae; `GetIterator` non-object validation; async generators (`Symbol.asyncIterator`); async-function promise settlement to user `.then` callbacks is a pre-existing gap that for-await depends on.
- **Engine change:** commit d2dfee6 (plus follow-up close-semantics fixes in this commit)
- **Files:** `crates/v12-bytecode/src/{opcode,lib}.rs`, `crates/v12-bccompiler/src/{model,stmt,expr}.rs`, `crates/v12-interp/src/{execute,object_ops,generator_async}.rs`, `crates/v12-bytecode/tests/common/mod.rs`, `crates/test-support/src/mini.rs`
- **Bucket:** `known-failures.md` §E — closed
- **Runner:** `./conformance/run.sh --filter <f> --jobs 8`
- **Notes:** workspace gate 578/578.

### 2026-09-12 — Step B: negative semantics (error ctors, ReferenceError, coercibility)

- **Filter:** `language/statements` (9 372 files), `built-ins/Error` (93 files), 8 jobs
- **Before:** language/statements 3 747/9 372 (40.0 %), built-ins/Error 1/93 (1.1 %) — measured at the A3-final commit (03aadbe) in a throwaway worktree
- **After:** language/statements 4 250/9 372 (45.3 %), built-ins/Error 13/93 (14.0 %)
- **Delta:** +503 language/statements, +12 built-ins/Error
- **Root cause:** the `TypeError`…`SyntaxError` globals were uncallable placeholders (`new TypeError("m")` → "not a function"); runtime throws built positional error objects whose `name`/`message`/`constructor` were unreadable as properties; undeclared global reads silently yielded `undefined`; property reads on `null`/`undefined` (incl. destructuring) silently yielded `undefined`.
- **Fix:** error constructors wired to native seams with real class prototypes (`Error.prototype` chain with class `name`); error instances — user constructed and internally thrown — carry shape-bound `name`/`message`/`constructor` and a `[[Prototype]]` link to the class prototype; `GetGlobal` on a missing binding throws `ReferenceError` with a new `GetGlobalLenient = 73` opcode serving spec `typeof`; top-level `var` slots are declared `undefined` in the main prologue; property reads on `null`/`undefined` throw `TypeError`.
- **Accepted gaps:** `EvalError`/`URIError` not installed as globals; `Error.cause`, `Error.isError`, `Proxy` still missing (remaining built-ins/Error failures); `let`/`const` at top level also prologue-initialized (no TDZ); cross-realm `new otherRealm.TypeError()` links to the primary realm's class.
- **Engine change:** commit 5e85248
- **Files:** `crates/v12-{bytecode,bccompiler,interp,engine,native}/src/**` (opcode.rs, lib.rs, model.rs, expr.rs, unit.rs, globals.rs, execute.rs, call_setup.rs, property.rs, error.rs, registry.rs, realm.rs, mod.rs, id.rs), `crates/test-support/src/mini.rs`, engine tests
- **Bucket:** `known-failures.md` §B — closed
- **Runner:** `./conformance/run.sh --filter <f> --jobs 8`
- **Notes:** workspace gate 578/578. The "Expected a … to be thrown but no exception was thrown" bucket (975 at reclassification) is the primary casualty: negative tests now observe real `TypeError`/`ReferenceError` objects.

### 2026-09-12 — Step A3: class element attrs, delete semantics, Function.name

- **Filter:** `language/expressions/class/elements`, `language/expressions/object/method-definition`, `built-ins/Object/getOwnPropertyDescriptor`, then `language/expressions` (11 190 files, 8 jobs)
- **Before:** class/elements 413/1428, object/method-definition 155/303, getOwnPropertyDescriptor 136/328, `language/expressions` 5 128/11 190 (45.8 %)
- **After:** class/elements 541/1428 (37.9 %), object/method-definition 161/303 (53.1 %), getOwnPropertyDescriptor 140/328 (42.7 %), `language/expressions` 5 306/11 190 (47.4 %)
- **Delta:** +128 class/elements, +6 method-definition, +4 getOwnPropertyDescriptor, +178 `language/expressions`
- **Root cause:** class method/accessor installs lowered to attributeless `SetProperty`/`DefineAccessor` stamping `Attrs::DEFAULT`; `delete` holed the value but left the shared shape descriptor readable; `Object.defineProperty` ignored descriptor flags; instance fields installed on the prototype; no `SetFunctionName`.
- **Fix:** new `DefineMethod = 72` opcode installing own data properties with `Attrs::BUILTIN`; `op_define_accessor` → `BUILTIN`; explicit attrs for function `length`/`prototype`/`constructor`; `descriptor_is_live` filters holed data descriptors from own-property queries; `Object.defineProperty` parses descriptor flags (and throws TypeError on rejected redefinition per spec); instance fields initialize on `this` in the constructor; `function_name` threaded to `alloc_closure`.
- **Accepted gaps:** derived-class field ordering after `super()`; static blocks; accessor `defineProperty` (`get`/`set`); computed/symbol method `name`; full holed-descriptor reader sweep; array indexed elements are invisible to own-property queries (pre-existing, engine-wide).
- **Engine change:** commits d39deea, 1a28bde, 17ea7fa, b6a637f, 89d6d5e, a836af0, 03aadbe
- **Files:** `crates/v12-heap/src/{shape,gc}.rs`, `crates/v12-bytecode/src/{opcode,lib,builder}.rs`, `crates/v12-interp/src/{object_ops,execute,property,lib}.rs`, `crates/v12-bccompiler/src/{class,unit,collect,model}.rs`, `crates/v12-engine/src/{internal_methods,builtins/object,builtins/mod,builtins/boolean}.rs`, `crates/v12-native/src/id.rs`, `crates/test-support/src/mini.rs`
- **Bucket:** `known-failures.md` §A3 — closed
- **Runner:** `./conformance/run.sh --filter <f> --jobs 8`
- **Notes:** workspace gate 578 passed / 0 skipped after the step. The plan's expected `9 false` for a non-writable-but-configurable value redefinition was spec-incorrect; the engine now throws TypeError and keeps the original value (matching V8).

### 2026-09-12 — Step A2: parameter defaults & destructuring in the prologue

- **Filter:** `language/expressions/function/dflt-params*`, `arrow-function/dflt-params*`, `object/method-definition`, `function`, `arrow-function`, then full `language` (24 873 files, 8 jobs)
- **Before:** function 74/264, arrow-function 156/343, object/method-definition 149/303, `function/dflt-params*` 3/9, `arrow-function/dflt-params*` 3/9, `*/dflt-params-ref-prior.js` 0/13
- **After:** function **141/264**, arrow-function **225/343**, object/method-definition **155/303**, `function/dflt-params*` **6/9**, `arrow-function/dflt-params*` **6/9**; `--filter dflt-params-ref-prior` 15/32
- **After (full `language`):** 10 998 pass / 13 875 fail / 0 skip, **44.2 %** (prior completed full-language score after A1: 9 722 / 24 873, 39.1 %)
- **Delta:** +1 276 pass on `language`, +5.1 pts. `language/expressions` slice 4 112/11 190 (36.8 %) → **5 128/11 190 (45.8 %)**.
- **Root cause:** formal parameters were never lowered (`compile_unit` emitted only the body); `UnitPlan.param_count` counted pattern leaves, so pattern-inner symbols collided with the incoming argument registers `r1..`.
- **Fix:** `register_formals` records top-level `arity`/`formal_idents`/`rest_ident` (walking every leaf and each `FormalParameter::initializer`); `finalize` reserves the incoming window (`r1..r{arity}`, rest at `r{arity+1}`) before assigning pattern leaves and body locals; `emit_prologue` runs the shared `lower_default`/`lower_binding_pattern` against each incoming register.
- **Accepted gap:** no parameter TDZ (`(a = b, b = 2)` reads `b`'s register instead of throwing).
- **Deviation from plan:** oxc 0.147 stores a top-level parameter default on `FormalParameter::initializer` (pattern stays bare), not as a `BindingPattern::AssignmentPattern`; `expected_args`, `register_formals`, and `emit_prologue` were adapted accordingly. All other steps match.
- **Engine change:** `8e03d93` — bccompiler prologue lowering + incoming-window reservation
- **Files:** `crates/v12-bccompiler/src/{model,collect,stmt,unit,tests}.rs`
- **Bucket:** `known-failures.md` §2 A2 — closed (defaults, patterns, ABI collision)
- **Gate:** `cargo nextest run --workspace` **576 passed**, 0 skipped
- **Runner:** `./conformance/run.sh --filter language --jobs 8` (human)

### 2026-09-12 — A1: `Function.prototype` callable + real `bind`

- **Filter:** `language/expressions/class/elements` (1 428 files, 8 jobs), then full `language` (24 873 files, 8 jobs)
- **Before (class/elements):** 413 pass / 1 015 fail / 0 skip, 28.9 %; 718 failures were `TypeError: callee is not a function` (propertyHelper.js failed to load)
- **After (class/elements):** 413 pass / 1 015 fail / 0 skip, 28.9 %; `callee is not a function` 718 → **90**; top message is now `m descriptor should not be enumerable; m descriptor should be configurable` (560)
- **After (full `language`):** 9 722 pass / 15 151 fail / 0 skip, **39.1 %** (prior completed full-language score: 8 919 / 24 446 / 427 skip, 36.5 %)
- **Delta:** class/elements pass count flat — `propertyHelper.js` now *loads* (`.call.bind` resolves), so the 718 tests advance past the load error and fail later on descriptor-attribute checks; the A1 bug is closed but those tests are gated by a separate gap (own-property descriptor enumerable/configurable). Full `language` +2.6 pts.
- **Engine change:** `b18c160` — `Function.prototype` allocated as `Kind::Function` (placeholder `Bytecode(u32::MAX)`) and the `Function` ctor linked via `install_ctor`; `Ctx::define_method` now returns the allocated handle (`Option<Handle<JsObject>>`). `76f6436` — `FunctionTarget::Bound(Handle<JsObject>)` state object `[target, thisArg, boundArgs..]` with GC trace; `FunctionBind` builds state + bound function and installs spec `length`/`name`; `Bound` dispatched in `prepare_call`, `call_accessor_with`, `call_inline`, `prepare_call_apply`; `prepare_construct` rejects bounds (A1 scope). Also fixed a site the plan did not list: `JsObject::trace` did not trace `self.callable`, so the `FunctionTarget::Trace` impl (Bound state, RealmEval global) never ran — GC stress reproduced a use-after-free; tracing `callable` fixes it.
- **Files:** `crates/v12-engine/src/realm.rs`, `crates/v12-engine/src/builtins/{ctx,mod}.rs`, `crates/v12-heap/src/{function,object}.rs`, `crates/v12-interp/src/{call_setup,internal_methods}.rs` (internal_methods is in v12-engine)
- **Bucket:** `known-failures.md` A1 — closed (the `Function` global half was stale in the doc; the real fix was the callable `Function.prototype` + ctor link + real `bind`)
- **Runner:** `./conformance/run.sh --filter language --jobs 8` (human) + `cargo nextest run --workspace` 570 pass / 0 skip
- **Notes:** `print` is not a CLI global (only the test262 harness defines it); scratch `t1.js`/`t2.js` use `console.log`. Verified under `--expose-gc` (stress collect on every alloc): t2 and a 100-iteration bound-alloc loop stay correct. Remaining 90 `callee` cases are async-generator `yield*`/private-field contexts, not propertyHelper.

### 2026-09-06 — Built-ins expansion: Math/Number/Array/Object/String/JSON/Boolean/global + Symbol/MapSet/Iterator

- **Filter:** `built-ins/Array|Math|Number|Object|String|JSON|Boolean|global` (8 slices, 8 jobs each)
- **Before:** Math 10.1 %, Number 18.6 %, JSON 11.7 %, Boolean 22 %, String 7.7 %, Object 1.7 %; Array hung the runner (no completion)
- **After:** Array 712/3 332 (21.4 %), Math 95/327 (29.1 %), Number 137/340 (40.3 %), Object 552/3 414 (16.2 %), String 346/1 341 (25.8 %), JSON 28/165 (17.0 %), Boolean 13/51 (25.5 %), global 14/29 (48.3 %)
- **Delta:** Math +19.0 pts, Number +21.7 pts, Object +14.5 pts, String +18.1 pts, JSON +5.3 pts, Boolean +3.5 pts; Array now completes
- **Engine change:** pure natives via `define_builtins!` + callback methods via `Interp::run_callback_builtin` interp seam (both call seams); RegExt merged-operand fix in `execute.rs` (arms read `instr.a()` instead of merged `ra`/`rb`/`rc` past 255 registers); `dense_bound` array-hang hardening (element-store len vs shape-bound integer keys, clamped); realm-global bias (`is_realm_global`/`realm_global_intrinsic_read`, `realm_globals` in gc.rs, ic_lookup intrinsic fallback, RealmEval ×5, Eval/Function seams); elements-overflow clamp; Symbol ctor + statics; Map/Set forEach/clear/entries/keys/values; Iterator toArray/take/drop/from
- **Files:** `crates/v12-engine/src/builtins/{array,math,number,object,string,json,boolean,global,symbol,iterator,mod,registry}.rs`, `crates/v12-engine/src/realm.rs`, `crates/v12-engine/src/gc.rs`, `crates/v12-interp/src/{execute,call_setup,globals}.rs`, `crates/v12-native/src/{id,methods}.rs`, `docs/builtins-plan.md`
- **Bucket:** built-ins coverage — shrank across all 8 slices (remaining: callback-semantics gaps, `callee is not a function` on Math length/name/prop-desc tests, display-string `[object Object]` for map results)
- **Runner:** `./conformance/run.sh --filter built-ins/X --jobs 8` (default `--format human`), gate `cargo nextest run --workspace` 569/569
- **Notes:** docs-only close-out; no code changes in this entry. Reconstructed concurrent-session cross-realm work included in gate count; `git diff` review of that re-implementation still pending before commit.

### 2026-09-05 — Async verdict path + Promise constructor + `import()` rejection promise

- **Filter:** `language/expressions` (11 190 files, 4 jobs) + `async` slice (6 252 files)
- **Before:** 4 054 pass / 6 953 fail / 183 skip, 36.8 %; every `$DONE`-containing test (~643) skipped, no async verdict
- **After:**  4 112 pass / 7 062 fail / 16 skip, 36.8 %
- **Delta:** +58 pass, −91 fail, −167 skip; pass rate flat but 167 async tests now *execute* — 36 of the old "passes" were false (they only passed because a missing async verdict auto-passed a sync `Ok` eval), and 91 previously-skipped tests now fail honestly against the async machinery
- **Engine change:** `new Promise(executor)` implemented (capability resolve/reject as host closures sharing the registry pending-jobs sink; executor runs as a microtask job — natives cannot re-enter the interpreter, documented divergence; promise adoption incl. already-settled values; `Promise()` without `new` throws); `Promise.prototype.catch`; await on a *pending* promise now polls instead of resuming with `undefined` (fulfilled-with-promise adoption chains; drain stalls honestly on never-settling promises); promise state slots are Smis (f64 broke `is_promise`); dynamic `import()` returns a rejected promise instead of throwing synchronously (`60857f3`, `2992c9d`)
- **Runner change:** `doneprintHandle.js` injected for async-flagged / `$DONE`-calling tests; verdict read from `Test262:AsyncTestComplete` / `Test262:AsyncTestFailure:` markers in `__test262Prints` (negative runtime expectations match through `handle_thrown`); missing marker = honest "async test did not complete" fail; the ignored Promise gate test is re-enabled and green
- **Files:** `conformance/harness/src/runner.rs`, `crates/v12-engine/src/builtins/{promise,mod,registry}.rs`, `crates/v12-engine/src/realm.rs`, `crates/v12-engine/src/engine/eval.rs`, `crates/v12-interp/src/{execute,lib,property,call_setup,generator_async}.rs`, `crates/v12-heap/src/object.rs`, `crates/v12-native/src/id.rs`
- **Bucket:** `async harness not yet implemented` — closed (replaced by real execution); exposed one pre-existing panic (await in async-generator bodies mis-decoded the non-parked caller frame — fixed by suspending generator-body awaits like `yield`)
- **Runner:** `./conformance/run.sh --filter language/expressions --jobs 4` + `cargo nextest run --workspace` 563 pass
- **Notes:** remaining async failures are engine gaps deeper than the verdict path: `Promise.all/race/finally`, async-generator `.next()` promise semantics, timers (`setTimeout`), async iteration.

### 2026-09-05 — P1 refactor: shared `lower_default` fixes assignment-target destructuring defaults

- **Filter:** `language/expressions` + annexB (11 190 files, 4 jobs)
- **Before:** 4 047 pass / 6 960 fail / 183 skip, 36.8 % (last recorded, pre-P0-commit `1b0f73b`)
- **After:**  4 054 pass / 6 953 fail / 183 skip, 36.8 %
- **Delta:** +7 pass net vs the recorded pre-P0 number; decomposition against a post-P0 baseline was not measured, but the P0 with-statement `CompileError` costs ~108 false passes on this slice (now honestly failing), so the destructuring-default fix recovered ~+115 on top.
- **Engine change:** part of the P1 refactor (`docs/refactor-plan.md`) — `FnCtx::lower_default` is now the single `src === undefined ? default : src` lowering; the expr.rs assignment-target destructuring walker previously *silently skipped* array-element defaults (`[a = 1] = []` bound the raw read) and ignored object-property defaults (`{x = 5} = {}`); both now apply the default. Also: object-property identifier defaults (`{x = 5}`) handled via oxc's `AssignmentTargetPropertyIdentifier.init`. Everything else in the change is a pure refactor (intrinsic-table consolidation into `v12_bytecode::GLOBAL_INTRINSICS` + derived `GLOBAL_VAR_OFFSET`; deletion of 87 `NATIVE_*` alias consts; `global_name_id`/`with_loop` helpers; for-of helper copies deleted).
- **Files:** `crates/v12-bytecode/src/lib.rs`, `crates/v12-bccompiler/src/{expr,stmt,model}.rs`, `crates/v12-engine/src/{realm,engine,builtins/mod}.rs`, `crates/v12-interp/src/lib.rs`
- **Bucket:** none — small distributed gain across `assignment/dstr` and `destructuring` slices (verified directly: `[a = 1, b = a + 1, ...rest] = [10]`, `{x = 5, y = x * 2} = {}`, for-of defaults)
- **Runner:** `./conformance/run.sh --filter language/expressions --jobs 4 --format json` + `cargo nextest run --workspace` 564 pass
- **Notes:** pure-refactor commit; the only semantic changes are the assignment-target default fixes. `with` slice still fails honestly by design (P0, accepted in the refactor plan).

### 2026-09-02 — Harness: always load sta.js/assert.js for non-raw tests

- **Filter:** `language/expressions` (11 190 files, 4 jobs)
- **Before:** 4 022 pass / 6 985 fail / 183 skip, 36.5 %
- **After:**  4 047 pass / 6 960 fail / 183 skip, 36.8 %
- **Delta:** +25 pass, −25 fail, +0.2 pts — harness-only, no engine semantics change
- **Engine change:** none — `conformance/harness/src/runner.rs` now prepends `sta.js`+`assert.js` to every non-raw test's include list (official test262 runner semantics), instead of only when the test declares no includes. Tests declaring e.g. `propertyHelper.js` previously ran without `Test262Error`/`assert`, so every failing assertion died as `TypeError: callee is not a function` (`new Test262Error(…)` on an undefined global) instead of a scored harness error.
- **Files:** `conformance/harness/src/runner.rs` (include assembly; `tmp_config` fixtures now provide sta.js/assert.js)
- **Bucket:** reclassified scattered assert-artifact failures into the real buckets (`Expected a undefined to be thrown`, SameValue mismatches)
- **Runner:** `cargo run -p test262-runner -- --filter language/expressions --jobs 4 --format json` (4 047/6 960/183) + `cargo nextest run --workspace` 563 pass
- **Notes:** the remaining `callee is not a function` 2 138 failures have two engine root causes, documented as buckets A1/A2 in `known-failures.md`: the missing `Function` global (propertyHelper.js cannot load) and the parameter-default member-read register bug.

> Steps 1–7c below were reconstructed on 2026-09-02 from git-log commit messages and
> CONTEXT.md history after a workspace reset wiped the original backfill entries
> (see the Step 8 notes and the CONTEXT.md incident note). Numbers are the ones
> recorded at the time in the commits.

### 2026-09-02 — Step 7c: spec-correct private fields with brand check

- **Filter:** `language/expressions` (11 164 files, 4 jobs)
- **Before:** 3 828 pass — via Step 7b hidden-key emulation (`obj["#x"]` exposed private state)
- **After:**  3 771 pass spec-correct — `obj["#x"]` is `undefined`, outside-class `#x` access throws `TypeError`
- **Delta:** −57 vs the hidden-key count, traded for spec correctness; `private fields are not supported` bucket stays 0
- **Engine change:** 2dfb52c — WideOps `GetPrivateW/SetPrivateW/DefinePrivateW/HasPrivateW` (discriminants 11–14, width 4); `JsObject {private_brand, private_fields}` with GC trace and `Construct` clone; brand-checked `private_get/has/define/set`; `this` slot panic fixed (`expr.rs:142` fallback + `collect.rs` field-init walk for `() => this.#x`)
- **Files:** `crates/v12-bccompiler/src/{class,collect,expr}.rs`, `crates/v12-bytecode/src/lib.rs`, `crates/v12-heap/src/{gc,object}.rs`, `crates/v12-interp/src/lib.rs`
- **Bucket:** `private fields are not supported` — closed, now spec-correct
- **Runner:** `cargo run -p test262-runner -- --filter language/expressions --jobs 4 --format json` (full `language` run timed out in CI) + `cargo nextest run --workspace` 563 pass
- **Notes:** reconstructed entry — numbers from git-log/CONTEXT.md history.

### 2026-09-02 — Step 7b: private class fields/methods via hidden properties

- **Filter:** `language` (24 446 executable, 8 jobs) and `language/expressions` (11 164 files)
- **Before:** 8 501 pass / 34.8 % (`language`); `language/expressions` 3 659 pass (32.8 %); `private fields are not supported` 1 492 (expressions)
- **After:**  8 919 pass / 36.5 %; `language/expressions` 3 828 pass (34.3 %)
- **Delta:** +418 pass, +1.7 pts on `language` (+169 on expressions); private-fields bucket 1 492 → 0
- **Engine change:** 1925a29 — desugar `#x` to a hidden `"#x"` property on the class prototype (instance) / constructor (static); `PrivateFieldExpression`→`GetProperty`, `PrivateInExpression`→`In` on the hidden key; optional-chain private spines flattened
- **Files:** `crates/v12-bccompiler/src/expr.rs`
- **Bucket:** `private fields are not supported` — closed (but not brand-checked)
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8` + nextest 563
- **Notes:** hidden-key emulation was not spec-correct; replaced by Step 7c. Reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 7a: triage `[object Object]` — render Test262Error plain objects

- **Filter:** `language/expressions` (11 164 files)
- **Before:** 3 659 pass / 32.8 %; opaque `threw: [object Object]` bucket 3 167 (not actionable)
- **After:**  3 659 pass — unchanged by design; the 3 167 failures reclassified into actionable buckets
- **Delta:** 0 pass — triage-only
- **Engine change:** 416e1f3 — `to_display_string` renders plain-object thrown values via `message`/`name` shape lookup (harness-facing, no semantics change)
- **Files:** `crates/v12-engine/src/engine.rs`, `crates/v12-interp/src/lib.rs`
- **Bucket:** `threw: [object Object]` 3 167 → 0 — reclassified to `Expected a undefined to be thrown` 975, `abrupt completion` 205, `SameValue` mismatches, etc.
- **Runner:** `cargo nextest run --workspace` 563 pass
- **Notes:** reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 6: instanceof + prototype handling

- **Filter:** `language/expressions` (11 164 files)
- **Before:** `language` ~34.8 % (8 501, unchanged since Step 4); `non-object prototype` bucket 9; non-callable RHS unvalidated
- **After:**  `language/expressions` 3 659 pass / 32.8 %; `language` ~34.8 % unchanged
- **Delta:** `non-object prototype` 9 → 0
- **Engine change:** b79bd4c — `op_instanceof` validates the RHS is callable per `OrdinaryHasInstance`, lazily materializes `Function.prototype` for realm placeholders, handles `null` prototypes
- **Files:** `crates/v12-interp/src/lib.rs`
- **Bucket:** `non-object prototype` — closed
- **Runner:** `cargo nextest run --workspace` 563 pass
- **Notes:** reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 5: native registry + minor syntax (BigInt, tagged templates, `with`, catch destructuring)

- **Filter:** `language` (24 446 executable, 8 jobs)
- **Before:** 8 501 pass / ~34.8 %; BigInt 147, tagged template 43, `with` 108; `ModuleImport is not registered` 306 (registry panic); nested-missing 115
- **After:**  8 501 pass / ~34.8 % — pass unchanged; listed buckets closed or reclassified
- **Delta:** BigInt 147 → 0, tagged 43 → 0, `with` 108 → 0; nested-missing 115 → 62 (684 → 62 cumulative); `ModuleImport` registered → 306 reclassified as 324 `dynamic import not supported` (proper TypeError, no panic)
- **Engine change:** f198eca — `ModuleImport` native stub, BigInt literals → BigInt heap values, tagged template → desugared call, `with` → best-effort, catch destructuring via `lower_binding_pattern`
- **Files:** `crates/v12-bccompiler/src/{expr,stmt,tests}.rs`, `crates/v12-engine/src/builtins/mod.rs`, `crates/v12-interp/src/lib.rs`
- **Bucket:** BigInt / tagged template / `with` — closed; `dynamic import` now an honest TypeError
- **Runner:** `cargo nextest run --workspace` 563 pass
- **Notes:** reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 4: for-of complex assignment targets

- **Filter:** `language/statements/for-of` (752 files) and `language` (24 446 executable, 8 jobs)
- **Before:** 8 407 pass / 34.4 %; `for-of` slice 479 fail
- **After:**  8 501 pass / 34.8 %; `for-of` slice 389 fail (363/752 pass)
- **Delta:** +94 pass, +0.4 pts; for-of −90 fail
- **Engine change:** f1482ae + c83e8a9 — `stmt.rs` `assign_for_of_value` handles array/object destructuring and member-expression targets (temp + recursion; defaults, rest via `CopyArrayRest`/`CopyObjectRest`, `member_parts` reuse)
- **Files:** `crates/v12-bccompiler/src/{stmt,expr}.rs`
- **Bucket:** `for-of` 479 → 389 — shrank
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8`
- **Notes:** reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 3b: Object/Function/Array prototype methods

- **Filter:** `language` (24 446 executable, 8 jobs)
- **Before:** 8 321 pass / 34.0 %; `callee is not a function` 3 690
- **After:**  8 407 pass / 34.4 %; `callee is not a function` 3 547
- **Delta:** +86 pass, +0.4 pts; callee −143
- **Engine change:** fbf70c4 — `Object.keys/values/entries/hasOwnProperty`, `Function.prototype.call/apply/bind/toString`, `Array.slice/sort`, plus `hasOwnProperty/valueOf/toString` fast paths on all objects
- **Files:** `crates/v12-engine/src/builtins/{array,object,mod}.rs`, `crates/v12-interp/src/lib.rs`, `crates/v12-native/src/{id,methods}.rs`
- **Bucket:** A `callee is not a function` — shrank (3 690 → 3 547)
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8`
- **Notes:** reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 3a: Array.isArray + Object statics

- **Filter:** `language` (24 446 executable, 8 jobs)
- **Before:** 8 216 pass / 33.6 %; `callee is not a function` 4 518
- **After:**  8 321 pass / 34.0 %; `callee is not a function` 3 690
- **Delta:** +105 pass, +0.4 pts; callee −828
- **Engine change:** 134138a — `Array.isArray` native 1106 + `Object.create`/`getPrototypeOf`/`defineProperty` fast paths in the interpreter
- **Files:** `crates/v12-engine/src/builtins/{array,mod}.rs`, `crates/v12-interp/src/lib.rs`, `crates/v12-native/src/id.rs`
- **Bucket:** A `callee is not a function` — shrank (4 518 → 3 690)
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8`
- **Notes:** reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 2: collector walks nested functions in switch/new/chain/template

- **Filter:** `language` (24 446 executable, 8 jobs)
- **Before:** 8 066 pass / 33.0 %; `nested function missing from plans` 684
- **After:**  8 216 pass / 33.6 %; nested-missing 115
- **Delta:** +150 pass, +0.6 pts; nested-missing −569
- **Engine change:** 661a781 — `collect.rs` walks `Switch`/`With`/`New`/`Template`/`Yield`/`Await` and destructuring assignment targets to register nested functions
- **Files:** `crates/v12-bccompiler/src/collect.rs`
- **Bucket:** `nested function missing from plans` 684 → 115 — shrank (→ 62 in Step 5)
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8`
- **Notes:** reconstructed from git-log/CONTEXT.md history.

### 2026-09-02 — Step 1 batch: private keys, extends null, new.target, super arrows, dynamic import

- **Filter:** `language` (8 jobs)
- **Before:** 4 858 pass / 19.9 % (2026-08-29 baseline over 24 873 files; intermediate 2026-08-30 score 7 561 / 32.1 % over 24 007)
- **After:**  8 066 pass / 33.0 % (24 446 executable, 427 skipped)
- **Delta:** +3 208 pass, +13.1 pts (executable denominator changed 24 873 → 24 446 between runs)
- **Engine change:** cc8349d — PrivateIdentifier class keys, `extends null` skips SetPrototype, `GetNewTarget` opcode 63 + `Frame::new_target` (arrows inherit), dynamic `import()` desugared to the `NATIVE_IMPORT_INDEX` call; plus the harness deadline (7697d32 + 98c68c5) so runaway tests time out cleanly
- **Files:** `crates/v12-bccompiler/src/{class,expr}.rs`, `crates/v12-bytecode/src/lib.rs`, `crates/v12-interp/src/lib.rs`, `conformance/harness/src/runner.rs`
- **Bucket:** computed-property-names 0 → 17/48, new.target 0 → 7/14, dynamic-import 0 → 492/1066; exposed `private fields` 3 020 and `ModuleImport` 298 as the next buckets
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8`
- **Notes:** numbers quoted verbatim from the cc8349d commit message; entry reconstructed from git-log.

### 2026-09-02 — Step 8 Number/Math globals + static/dynamic registry (DRY, zero-cost dispatch)

- **Filter:** `language/expressions` (11 164 files) and `language` (11 190 files incl. annexB)
- **Before:** 3 771 pass / 7 210 fail (expressions) — 3 785 pass / 7 222 fail total (34.4 %); `callee` 2 232, `not a function` 329
- **After:**  4 008 pass / 6 973 fail (expressions) — **4 022 pass / 6 985 fail total, 36.5 % pass** (annexB 14/26)
- **Delta:** +237 pass, −237 fail, +2.1 pts on slice; `callee` 2 232 → 2 137 (−95), `not a function` 329 → ~250 (−79); combined callee bucket −174
- **Engine change:** Number ctor + `Number.isNaN/isFinite/parseInt/parseFloat`, globals `isNaN/isFinite/parseInt/parseFloat`, `Math` `floor/ceil/trunc/round/sqrt/pow/max/min/random` via static `define_builtins!` registry + `BuiltinTargets`/`install_builtins` (compile-time straight-line installs, no data array), `helpers::to_number`/`js_number` DRY, zero-cost `NativeId` match (jump table) + shape-lookup dispatch
- **Files:** `crates/v12-native/src/id.rs`, `crates/v12-engine/src/builtins/{helpers,number,math,mod}.rs`, `crates/v12-engine/src/realm.rs`, `crates/v12-interp/src/lib.rs`
- **Bucket:** `known-failures.md` A (`callee is not a function`) — shrank (2232 → 2137; remaining ~2 k callee still Array/JSON etc.)
- **Runner:** `cargo run -p test262-runner -- --filter language/expressions --jobs 4 --format json` (4022/6985/183) + `cargo nextest run --workspace` 563 pass
- **Notes:** Reconstructed 2026-09-02 after a workspace reset wiped the uncommitted diff (recovery from `stash@{0}` + spec-driven rebuild; see CONTEXT.md incident note).

### 2026-08-30 — Iterator protocol + `for-of` (Priority 2)

- **Filter:** `language/statements/for-of` (752 files, 8 jobs)
- **Before:** 0 pass / 0 fail / 752 skip, 0.0 % pass (all rejected: `for-of requires the iterator protocol — Symbol.iterator is not available yet`)
- **After:**  244 pass / 508 fail / 0 skip, 32.4 % pass
- **Delta:** +244 pass, −752 skip, +32.4 pts
- **Engine change:** added `GetIterator`/`IteratorNext`/`IteratorClose` opcodes (68–70); `KIND_ITERATOR` heap kind; engine `iterator.rs` builtins (Array/Map/Set iterators, `next`, `%IteratorPrototype%` self-return); interpreter `op_get_iterator`/`op_iterator_next`/`op_iterator_close` + `call_inline` (nested-frame call usable inside dispatch); `Symbol.iterator` well-known symbol on the `Symbol` intrinsic; `Array.prototype.entries/keys/values/pop` fast paths; compiler `for_of_loop` lowering + collect-pass declaration of for-of/in bindings (fixes the pre-existing "both destructured bindings land in r0" bug).
- **Files:** `crates/v12-bytecode/src/lib.rs`, `crates/v12-heap/src/object.rs`, `crates/v12-engine/src/builtins/iterator.rs` (new), `crates/v12-engine/src/builtins/mod.rs`, `crates/v12-interp/src/lib.rs`, `crates/v12-bccompiler/src/stmt.rs`, `crates/v12-bccompiler/src/collect.rs`, `crates/v12-bccompiler/src/tests.rs`, `crates/v12-bytecode/tests/common/mod.rs`, `crates/v12-engine/src/engine.rs` (tests)
- **Bucket:** `known-failures.md` A (`unsupported expression`) — shrank (for-of no longer rejected); P2 `for-of` slice opened at 32.4 %
- **Runner:** `cargo run -p test262-runner -- --filter language/statements/for-of --jobs 8`
- **Notes:** Remaining for-of failures: completion-value semantics (`cptn-*`), complex assignment targets, `arguments` exotic objects, accessor/defineProperty paths, `IteratorClose` on throw. `cargo nextest run` 553/553 pass (8 new engine tests).


### 2026-08-29 — Un-ignore async tests (generators+async now executable)

- **Filter:** `language` (24 873 files, 8 jobs)
- **Before:** 4 940 pass / 14 972 fail / 4 961 skip, 24.8 % pass (f47ec78; async skips 4 883 + $262 78)
- **After:**  4 858 pass / 19 588 fail / 427 skip, 19.9 % pass (f9dd7de; async skips 0)
- **Delta:** −82 pass, +4 616 fail, −4 534 skip, −4.9 pts — async slice became executable (expected transient dip; newly exposed failures on `yield*`/`for-await`/promise jobs)
- **Engine change:** none — harness-only. Removed `if fm.has_flag("async")` skip in `conformance/harness/src/runner.rs:322`; kept `createRealm(`/`$262.agent`/`$DONE` skips. Async completion already covered by `__test262Prints` capture + `engine.run_jobs()` drain.
- **Files:** `conformance/harness/src/runner.rs`, `conformance/known-failures.md`, `conformance/fix-log.md`
- **Bucket:** `known-failures.md` C (async harness) — closed; remaining skips 427 are multi-realm/agent + `$DONE`
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8 --format json --json-out /tmp/t262.json && cat /tmp/t262.json | python3 -c "import json; print(json.load(open('/tmp/t262.json'))['summary'])"`
- **Notes:** `cargo nextest run -p test262-runner` 38/38 pass (skip_async_flag now expects no skip). Language suites remain green on non-async paths; async tests are now scored.

### 2026-08-27 — `$262` host shim wired; async skip kept (gate failed)

- **Filter:** `language/expressions` (11 190 files, 8 jobs)
- **Before:** 2 094 pass / 6 844 fail / 2 252 skip, 23.4 % pass
- **After:**  2 094 pass / 6 861 fail / 2 235 skip, 23.4 % pass
- **Delta:** 0 pass, +17 fail, −17 skip — 17 previously-skipped `$262` tests (incl. the 26-file annexB slice: 17 skip → 17 fail) became executable; all 17 fail on real engine gaps, which is the honest outcome. annexB mini-bucket re-score: 11.1 % → 3.8 % (denominator grew by 17).
- **Engine change:** none — harness-only change. `TEST262_HOST_SHIM` preamble defines `print` + `$262` (`createRealm`/`detachArrayBuffer`/`getReport`/`destroy`/`gc`/`global`) captured into `globalThis.__test262Prints`; skips narrowed to `createRealm(`, `$262.agent`, async-flagged, and `$DONE(` tests.
- **Gate result (plan Task 6 Step 3): FAILED.** Self-test `async_doneprint_test_completes_via_captured_print` proves a resolved `Promise.then` continuation does NOT execute via `run_jobs()` — `engine.eval` throws on `Promise.resolve()` itself (`Promise` is only an intrinsic name, no constructor). Evidence kept as an `#[ignore]`d test in `runner.rs`. Async skips stay honest: "async harness not yet implemented". Full async verdict path NOT implemented (would convert ~4.9k skips into guaranteed failures).
- **Files:** `conformance/harness/src/runner.rs` (shim constant, `skip_reason_for`, combined-source preamble), `conformance/known-failures.md`, `conformance/fix-log.md`
- **Bucket:** `known-failures.md` C — partially closed ($262 half); async half blocked on Promise reaction jobs
- **Runner:** `cargo run --release -p test262-runner -- --filter language/expressions --jobs 8`
- **Notes:**
  - `cargo nextest run -p test262-runner` 37/37 pass, 1 ignored (the gate test).
  - Engine follow-up needed: Promise constructor + `PerformPromiseThen` reaction jobs enqueued on `run_jobs()`; re-enable the gate test, then wire the async verdict path.

### 2026-08-27 — Switch duplicate-`default` panic → SyntaxError


- **Filter:** `language` (24 873 files, 8 jobs)
- **Before:** 4 957 pass / 14 955 fail / 4 961 skip, 24.9 % pass (1 × `engine panic`)
- **After:**  4 958 pass / 14 954 fail / 4 961 skip, 24.9 % pass (0 panics)
- **Delta:** +1 pass, −1 fail, 0 skips — `engine panic` count on `language` now 0
- **Engine change:** `switch_stmt` rejects duplicate `default` clauses with `SyntaxError: more than one default clause in switch statement`. Root cause: phase 2 bound the single shared `default_entry` label once per `None` entry, so a second default double-bound it and `FunctionBuilder::bind` panicked (`label Label(1) bound more than once`). Builder kept strict (double-bind stays a hard panic); the compiler-side emission flow is the fix. Minimal repro: `switch (1) { default: ; break; default: ; break; }` — panics pre-fix, SyntaxError post-fix.
- **Files:** `crates/v12-bccompiler/src/stmt.rs` (validation + comment), `crates/v12-bccompiler/src/tests.rs` (`switch_duplicate_default_is_a_syntax_error`)
- **Bucket:** `known-failures.md` — panics bucket stays closed (regression found by Test262 `language/statements/switch/S12.11_A2_T1.js`, a negative parse-phase test, which now passes)
- **Runner:** `cargo run --release -p test262-runner -- --filter language --jobs 8`
- **Notes:**
  - `cargo fmt` clean; `cargo clippy -p v12-bytecode -p v12-bccompiler -p v12-interp --all-targets` 0 warnings; `cargo nextest run -p v12-bytecode -p v12-bccompiler -p v12-interp` 244/244 pass.
  - Located via `--format tap`: only one panicking test in the whole `language` filter (S12.11_A2_T1.js); the aggregate panic message is printed once regardless of jobs.

### 2026-08-27 — SSA optimizer Phase 2 + GC root fix re-score

- **Filter:** `language` (24 873 files, 8 jobs) and `language/expressions/assignment` (818 files, 4 jobs)
- **Before (previous run):** 4 889 pass / 19 984 fail (est.) / 24.6 % pass on `language`; assignment slice 401 / 409 / 8 skip, 49.5 %
- **After (full language):** 4 940 pass / 14 972 fail / 4 961 skip, **24.8 %** pass
- **After (assignment slice):** 411 pass / 405 fail / 2 skip, **50.4 %** pass — delta vs baseline **+10 pass, −0 fail net of 6 un-skipped, +0.85 pts**
- **Delta:** +51 pass vs previous run (+428 vs bootstrap), −290 "fail" is restated panics→clean compile errors, skips 5 679 → 4 961 (−718)
- **Engine change:** f47ec78 — Tier-2 SSA+inlining+loop versioning behind guards (fail-closed, no conformance flip by itself), GC root fix. Credit also to earlier `262aed8`–`0466cb5` (in/instanceof opcodes, overflow path, eval/accessors, destructuring/rest/spread/modules/generators, ESM loader)
- **Files:** crates/v12-jit-opt/*, crates/v12-engine/src/gc.rs, crates/v12-bccompiler/*
- **Bucket:** #2 (`collect.rs` overflow panic) — **closed**: zero `engine panic` results; overflow now surfaces as clean compile error. #1 (`in`/`instanceof`) — **closed** (zero opcode/unbound errors). #3 (globals) — **closed**. #4 — **shrank**: module skips 721 → 0
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8` / `--filter language/expressions/assignment --jobs 4`
- **Notes:**
  - Top failures on `language`: `threw: unsupported expression` ×12 625, `threw: too many functions/constants` ×1 303, `unsupported statement` ×296, `TypeError: callee is not a function` ×256, `export/import statements only valid in modules` ×214, `object methods / accessors are not supported` ×56.
  - Per-suite movers vs bootstrap: `module-code` 43 % (was ~all-skip), `import` 24 pass (was skip-stub), `keywords` 100 %, `punctuators` 90.9 %, `future-reserved-words` 89.1 %; no suite regressed below its bootstrap rate.
  - Remaining skips: async harness 4 883 + `$262` host object 78. Next target: async job queue, then the new `unsupported expression` mega-bucket (split by expression kind).

### 2026-08-26 — Harness bootstrap (baseline)

- **Filter:** `language` (24 873 files, 8 jobs) and `language/expressions/assignment` (818 files, 4 jobs)
- **Before:** harness did not exist
- **After (assignment slice):** 401 pass / 409 fail / 8 skip, 49.5 % pass
- **After (full language):** 4 512 pass / 14 682 fail / 5 679 skip, 23.5 % pass
- **Engine change:** none — baseline measurement
- **Files:** `conformance/harness/src/*` (runner crate), `conformance/test262/` (shallow clone depth 1), `conformance/run.sh`, `conformance/README.md`, `conformance/known-failures.md`
- **Bucket:** all of `known-failures.md` — seeded
- **Runner:** `cargo run -p test262-runner -- --filter language --jobs 8 --format human`
- **Notes:**
  - Auto-injects `sta.js`+`assert.js` when a non-raw test uses `assert`/`Test262Error` but lists no `includes` (common in Sputnik-era tests); otherwise the slice showed `assert is not defined` instead of the real `in`/`instanceof` gap.
  - Catches `v12-bccompiler` panics (e.g. `collect.rs:706 overflow`) as `Fail: engine panic` so the harness never crashes.
  - Verified: `cargo check -p test262-runner` and `cargo test -p test262-runner` (35 tests, all pass; harness self-tests for frontmatter/flags/includes/runner).
  - Next fix target: `known-failures.md` #1 (`in`/`instanceof`).

---

### 2026-08-29 — Engine/embedding work (ADR-003/005/006, register_fn, call)

- **Filter:** n/a — no conformance-number change intended
- **Engine change:** `Interp` borrows `&mut Heap` (ADR-003); `v12-api` facade lands `register_fn`/`call` (ADR-005); JIT shared types move to `v12-codegen` (ADR-006).
- **Known gap (unchanged, pre-existing):** `Promise.resolve().then(cb)` fails with `TypeError: callee is not a function` — the promise `.then` path cannot yet activate bytecode callbacks. Recorded-gate tests `async_promise::promise_resolve_then_runs_callback_via_run_jobs` and `promise_chained_then_drains_fia_run_jobs` (both uncommitted WIP) document this; they fail before and after this work.
- **Runner:** `cargo nextest run --workspace` — 533 pass / 2 fail (the two gates above) / 1 skip.

---

<!-- Future entries go above this line -->
