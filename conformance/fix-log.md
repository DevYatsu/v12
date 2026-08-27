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

<!-- Future entries go above this line -->
