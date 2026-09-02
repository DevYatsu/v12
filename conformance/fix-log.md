# Fix log — Test262 harness burn-down

Append-only log. Each entry records one fix, its before/after harness numbers, and which bucket in `known-failures.md` it closed or shrank.

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
- **Bucket:** `known-failures.md` #1 (`in`/`instanceof`) — closed / shrank (remaining: …)
- **Runner:** `cargo run -p test262-runner -- --filter language/expressions/assignment --jobs 4`
- **Notes:** optional; e.g. "negative tests for invalid `in` now pass as SyntaxError".
```

## Entries

<!-- Add newest entries at the top. Keep the template above as reference. -->

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
