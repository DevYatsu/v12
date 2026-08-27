# Known failures — last scored 2026-08-27

> Latest run: `cargo run -p test262-runner -- --filter language --jobs 8`
> on commit `f47ec78` (Tier-0 + SSA optimizer Phase 2, fail-closed guards).
> Totals: **24 873 tests, 4 940 pass / 14 972 fail / 4 961 skip, 24.8 % pass** over `test/language` (bootstrap was 23.5 %).
> Treatment: `pass%` is over executable tests (`pass + fail`). Skips are not counted.
>
> The assignment-expression slice (`--filter language/expressions/assignment`, 818 files)
> on the same build: **411 pass / 405 fail / 2 skip, 50.4 % pass** (baseline 49.5 %).
> See `fix-log.md` for the burn-down log. Move a bucket there when green.

This file is the fix-it queue. Each bullet is a bucket — a single engine gap that, once closed, will flip a visible swath of red to green. Keep the buckets small and ordered by estimated lift.

## How to use

- The harness is the scoreboard. After a fix, re-run the filter for that bucket and paste the before/after into `fix-log.md`.
- Delete the bullet here when the bucket is green on `test/language`.
- Do not add new buckets without a filter that reproduces them: `cargo run -p test262-runner -- --filter <filter> --jobs 8 --verbose | head -n 50`.

## Buckets

### A. `unsupported expression` — 12 625 failures, new largest bucket (was #5)

- **Symptom:** `threw: unsupported expression` from `v12-bccompiler` Tier coverage limits. Dominates every large suite: `expressions` (6 840 fail), `statements` (5 302), plus all of annexB/eval-code.
- **Filter reproducing it:** `--filter language --verbose | head -n 50`.
- **Scope:** getters/setters & object methods (`object methods / accessors are not supported`, ×56), RegExp literals ×47, class declarations ×25 — but the bulk is unnamed expression-kind gaps; must be split by sub-suite before fixing.
- **Fix location:** crates/v12-bccompiler/src/expr.rs Tier coverage docs + v12-interp dispatch tables. Work smallest surface first (`computed-property-names` still 0/48).
- **Gating:** each closed expression kind flips its sub-suite visibly on the per-suite table.

### B. `too many functions/constants` — 1 303 failures (successor of old #2)

- **Symptom:** was a panic at collect.rs:706 (`attempt to add with overflow`, caught as engine panic); since `262aed8` it is a clean compile error, but the index counters still saturate on large files.
- **Fix location:** widen counters, dedupe constants in plans, or spill to a second plan segment. Zero panics now, so this is purely a capacity fix.

### C. Async harness + `$262` skips — 4 944 skips remaining

- **Counts at f47ec78:** async harness 4 883 (`expressions`+`statements`, mostly generators/async), `$262` host object 78 (annexB/eval-code).
- **Progress (2026-08-27, this commit):** the `$262` host shim landed — tests using `$262.global`/`detachArrayBuffer`/`gc`/`getReport` now run. Skips narrowed from 4 961; remaining skips are only `createRealm(` (multi-realm), `$262.agent`, and async-flagged/`$DONE` tests.
- **Blocked on engine, not harness:** the async verdict path is gated behind a failing self-test — `Promise.resolve().then(...)` never executes (Promise reaction jobs are not scheduled through `run_jobs()`; `Promise` exists only as an intrinsic name). Self-test kept `#[ignore]`d in `runner.rs` as evidence (`async_doneprint_test_completes_via_captured_print`). Do not narrow the async skip until it passes.
- **Fix order (remaining):**
  1. Engine: wire Promise reaction jobs into the job queue (`Promise` intrinsics only exist as names today).
  2. Then the async `done` print-watched verdict (`doneprintHandle.js` prints `Test262:AsyncTestComplete` → re-read captured `__test262Prints` array) — adds ~4.9k executable tests without hurting pass%.
- **Note:** Skips do not count toward the `pass%` denominator, so wiring them does not hurt the percentage — it only adds executable tests that must then pass.

### E. Loose equality lacks number↔string coercion — found by the new differential suite (e4902d4)

- **Symptom:** `1 == '1'` → `false` (should be `true`); `0 == '0'` → `false`. Strict equality and same-type `==` are correct.
- **Root cause:** `loose_equals` (crates/v12-interp/src/ops.rs:300) is missing the number↔string coercion arm of ES 7.2.14 (compare `ToNumber(string)` with the number).
- **Reproducing filter:** `cargo nextest run -p v12-interp --test differential --  known_gap_loose_equals_number_string_coercion --include-ignored`.
- **Fix location:** crates/v12-interp/src/ops.rs:300 — add the ToNumber(string) arm; un-ignore the pinned test after fixing.

## Done (moved out of the queue)

### D1. ~~`in` / `instanceof` opcodes~~ — **closed** (262aed8 → verified f47ec78)

- Zero opcode/unbound errors remain across all 14 972 fails. Assignment slice gained ~+10 net via this and follow-ups. Gate predicted 65 % on the slice; actual plateau is 50.4 % because buckets A/B cap it.

### D2. ~~`collect.rs` overflow panic~~ — **closed** (262aed8 → successor bucket B)

- Zero `engine panic` results in the full run (was ~200+ distinct panics). Overflow path returns a clean compile error so negative tests can still pass.

### D3. ~~Global object & property model~~ — **closed** (aaa339b)

- `GetGlobal`/`SetGlobal`; no more "unbound variable" errors anywhere in the failure stream. Literals sit at 59.7 % with remaining losses owned by buckets A/B.

### D4. ~~Module / ESM skips~~ — **closed** (e534394 loader → 0466cb5 buckets)

- Module skips 721 → 0. `module-code` runs end-to-end: 755 total, 309 pass / 410 fail / 36 skip (43 %). Residual `export/import statements only valid in modules` ×214 is script-mode negative handling, tracked under bucket A.

## Re-run commands

```sh
# Full language gate (Phase 1 ≥60 %)
./conformance/run.sh --filter language --jobs 8

# Quick health check on the biggest current bucket
./conformance/run.sh --filter language/expressions/assignment --jobs 4 --verbose | head -n 100

# TAP + JSON artifacts for nightly
cargo run -p test262-runner -- --filter language --format tap,json --json-out /tmp/t262.json --tap-out /tmp/t262.tap
```

## Exit criteria

- Remove this file's buckets one by one into `fix-log.md`.
- Phase 1 gate: ≥60 % on `test/language`, zero `engine panic` cases, jit vs no-jit JSON diff is zero when the `jit` feature is on.
- Phase 2 gate: ≥85 % overall (`--filter` none) and Tier 1 default-on.
