# Known failures — seeded 2026-08-26

> Bootstrap run: `cargo run -p test262-runner -- --filter language --jobs 8`
> on commit `HEAD` (v12 early Tier-0, interpreter-only, no modules/async).
> Totals: **24 873 tests, 4 512 pass / 14 682 fail / 5 679 skip, 23.5 % pass** over `test/language`.
> Treatment: `pass%` is over executable tests (`pass + fail`). Skips are not counted.
>
> The assignment-expression slice (`--filter language/expressions/assignment`, 818 files)
> on the same build: **401 pass / 409 fail / 8 skip, 49.5 % pass**.
> See `fix-log.md` for the burn-down log. Move a bucket there when green.

This file is the fix-it queue. Each bullet is a bucket — a single engine gap that, once closed, will flip a visible swath of red to green. Keep the buckets small and ordered by estimated lift.

## How to use

- The harness is the scoreboard. After a fix, re-run the filter for that bucket and paste the before/after into `fix-log.md`.
- Delete the bullet here when the bucket is green on `test/language`.
- Do not add new buckets without a filter that reproduces them: `cargo run -p test262-runner -- --filter <filter> --jobs 8 --verbose | head -n 50`.

## Buckets

### 1. `in` / `instanceof` opcodes — ~2k–3k failures, largest single bucket

- **Symptom:** `threw: `in` / `instanceof` have no bytecode opcodes yet` (from `v12-bccompiler` limits, §1 docs).
- **Filter reproducing it:** `--filter language/expressions/assignment` (current: 49.5 %, many `8.12.5-*`, `11.13.1-*`).
- **Scope:** Every test that checks `in`, `instanceof`, or `Object` property-descriptor helpers that touch `instanceof` inherits it.
- **Fix location:** `crates/v12-bccompiler/src/expr.rs` + `v12-bytecode` ISA (new `In`/`InstanceOf` ops) + `v12-interp` dispatch. Add `ToBoolean` / `HasProperty` internal method path, not a stub.
- **Gating:** assignment slice should jump from 49 % to ~65 % when closed; full `language` should gain ~8–10 pts.

### 2. `collect.rs` overflow panic — all `engine panic` buckets

- **Symptom:** `thread '<unnamed>' panicked at crates/v12-bccompiler/src/collect.rs:706: attempt to add with overflow`. Harness catches it as `Fail: engine panic`, not a crash, but it hides the real result.
- **Filter:** `--filter language` (14k fails, ~200+ distinct panics counted in stderr).
- **Fix location:** `crates/v12-bccompiler/src/collect.rs:706` — `checked_add` / saturated `u32`/`u16` counters for function/constant indices. Add a compile-failure path (`Err(CompileError{..})`) instead of panicking so negative tests can still pass.
- **Verification:** panics in stderr should go to zero; `engine panic` cases become either `Pass` (negative) or `Fail` with a real compiler message.

### 3. Global object & property model — many `reference to an unbound variable` fails

- **Symptom:** `threw: reference to an unbound (global) variable Object/String/Array/... is not supported` or `assignment to an unbound variable`. Realm installs intrinsics but the compiler still treats naked identifiers as locals.
- **Filter:** same assignment slice; also `language/literals`, `language/statements/variable`.
- **Fix location:** `crates/v12-bccompiler/src/model.rs` (scope resolution: globals vs locals), `crates/v12-engine/src/realm.rs` + `builtins/*` (publish `Object`, `Array`, etc. on the global and wire `Heap::get` lookups), and the interpreter's `GetGlobal` / `SetGlobal`.
- **Phase 1 impact:** literals go 59 % → ~75 %, block-scope stays at 74 % but stops regressing.

### 4. Module / async / `$262` skips — 5.6k skips

- **Symptom:** skipped as `module not yet wired (ESM stub)` (598 in `module-code` + 123 in `import`), `async harness not yet implemented` (2.2k in `expressions`+`statements`), `requires $262 host object` (many `annexB`/`eval-code`).
- **Counts in bootstrap:** `module` 721 skips, `async` 2 261, `$262` included in broader skip set; overall 5 679 skips on `language`.
- **Fix order:**
  1. `$262` host stub (`$262.createRealm`, `detachArrayBuffer`, `getReport`) — lowest effort, unblocks `annexB`.
  2. Module compile-as-module + stub loader (`compile_source_as_module` / `SourceType::module`), keep `resolution` negative handling already in runner.
  3. Async `done` via `print`-watched job queue (`doneprintHandle.js` prints `Test262:AsyncTestComplete`).
- **Note:** Skips do not count toward `pass%` denominator, so wiring them does not hurt the percentage — it only adds executable tests that must then pass.

### 5. Remaining Tier-0 compiler gaps — visible in per-suite tail

- **Symptom:** various `threw: unsupported expression`, `threw: null literals`, `threw: null`, `threw: substring becomes SlicedString`, etc., per `crates/v12-bccompiler/src/lib.rs` "Tier coverage" docs.
- **Most visible suites still near 0:** `computed-property-names` (0/48), `eval-code` (0/347), `source-text` (0/1), `white-space` (9 %), `types` (17 %), `function-code` (15 %). Each is a focused built-in or grammar item (e.g. computed keys, getters/setters, `null`, destructuring post-fix).
- **Fix guidance:** Pick the next suite with the smallest surface — `computed-property-names` or `rest-parameters` (3/11) — and extend the compiler op coverage + interpreter handler tables in `v12-bccompiler`/`v12-bytecode`/`v12-interp` before moving to `built-ins` breadth.

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
