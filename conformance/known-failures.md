# Known failures — last scored 2026-08-30

> Latest run: `cargo run -p test262-runner -- --filter language --jobs 8 --format json --json-out /tmp/t262.json`
> Totals: **24 007 tests, 7 561 pass / 16 019 fail / 427 skip, 32.1 % pass** over `test/language`.
> Treatment: `pass%` is over executable tests (`pass + fail`). Skips are not counted.
> Since the 2026-08-29 score (19.9 %): the iterator protocol + `for-of`, the
> RegExp runtime, and the unified native dispatch landed (+12.2 pts).
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

### C. Remaining skips — 427 skips (async now executable)

- **Counts at f9dd7de:** async harness 0 (was 4 883 at f47ec78) — generators + async/await now executable via `__test262Prints` capture + `run_jobs` drain. `$262` host object skips are also gone except multi-realm/agent. Remaining skips are only `createRealm(` (multi-realm), `$262.agent`, and `$DONE` without async flag.
- **Progress (2026-08-29, this commit):** async skip removed from `skip_reason_for` in `conformance/harness/src/runner.rs:322`. `cargo run -p test262-runner -- --filter language --jobs 8 --format json --json-out /tmp/t262.json` now reports **4 858 pass / 19 588 fail / 427 skip, 19.9 % pass** (was 4 940 / 14 972 / 4 961, 24.8 %). Delta: −82 pass, +4 616 fail, −4 534 skip — the ~4.5k formerly-skipped async tests became executable; most fail on remaining gaps (`yield*`, `for-await-of`, promise reaction jobs).
- **Remaining gaps for async pass%:** `yield*` delegation and `for-await-of` are tracked under bucket A (`unsupported expression`); promise reaction jobs already drain via `run_jobs` but some async tests still need fuller job coverage.
- **Note:** Skips do not count toward the `pass%` denominator; un-skipping lowers pass% transiently until the newly-exposed failures are fixed.

## Done (moved out of the queue)

### D5. ~~Loose equality number↔string coercion~~ — **closed** (found by the differential suite, e4902d4)

- `loose_equals` (crates/v12-interp/src/ops.rs) was missing the number↔string arm of ES 7.2.14 (`1 == '1'` → `false`). Fixed: number↔string compares `ToNumber(string)` with the number, and boolean operands are coerced via ToNumber then re-dispatched (the old bool arm also panicked on bool↔number). Pinned differential test `known_gap_loose_equals_number_string_coercion` un-ignored and green.

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
