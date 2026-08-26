# Test262 conformance harness

This directory gates v12's correctness against [Test262](https://github.com/tc39/test262) — the ECMA-262 conformance suite. The harness is the exit gate for the plan's numeric targets: **Phase 1 ≥60 % of `test/language`**, **Phase 2 ≥85 % overall**, both with zero interpreter-vs-JIT delta.

## Layout

```
conformance/
├── test262/            # checkout of https://github.com/tc39/test262
│   ├── test/           # suite (≈ 90k files; language/ ≈ 25k, built-ins/ ≈ 24k)
│   └── harness/        # assert.js, sta.js, propertyHelper.js, …
├── harness/            # Rust crate `test262-runner` (this repo)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs        # CLI + orchestration
│       ├── frontmatter.rs # /*--- YAML ---*/ parsing
│       ├── harness.rs     # include prepending + polyfill
│       ├── runner.rs      # per-test Engine::eval + negative/flag handling
│       └── report.rs      # TAP / JSON / human summary
├── run.sh              # one-shot entry point
├── known-failures.md   # seeded from the bootstrap run
└── fix-log.md          # template for recording fixes
```

### test262 checkout

The suite is a shallow clone:

```sh
git clone --depth 1 https://github.com/tc39/test262 conformance/test262
```

If your network blocks the clone, the runner still works with the minimal harness polyfill, but you will see `harness include error` skips. To wire as a submodule instead:

```sh
git submodule add https://github.com/tc39/test262 conformance/test262
git submodule update --init --depth 1
```

The harness auto-detects `conformance/test262` relative to `cwd`, `test262`, or an explicit `--test262-root`.

## Runner: `test262-runner`

A Rust binary that links `v12-engine` as a library. Design constraints: `#![forbid(unsafe_code)]`, named constants, no `unsafe`, self-contained docs.

### Build & check

```sh
cargo check -p test262-runner
cargo test -p test262-runner          # harness self-tests (frontmatter, flags, includes, runner)
cargo nextest run -p test262-runner     # if you use nextest
```

### CLI surface

```
test262-runner [OPTIONS]

Options:
  --filter <GLOB>         Glob or substring over the path relative to test/
                          e.g. language/expressions, language/statements/*break*
                          (no filter = all tests)

  --jobs <N>              Parallelism. 0 = auto (num_cpus), capped at 64.
                          Default 0 (currently 8 logical CPUs → 8 workers).

  --verbose               Print per-test lines + failure messages and
                          per-suite detail to stderr. With --format tap
                          emits YAML diagnostics.

  --include-skipped       Also enumerate skipped tests in verbose listing.

  --format <FORMAT>       human (default), tap, json, or comma combo
                          e.g. --format tap,json

  --test262-root <PATH>   Over-ride auto-discovery of the checkout.

  --tap-out <PATH>        Also write TAP to a file.
  --json-out <PATH>       Also write JSON to a file.
  --list                  List matching tests and exit (dry run).
```

Examples:

```sh
# Run ~800 assignment-expression tests, human summary + 4 workers
cargo run -p test262-runner -- --filter language/expressions/assignment --jobs 4

# Language subset with TAP on stdout and JSON to a file
cargo run -p test262-runner -- --filter language --format tap,json --json-out /tmp/t262.json

# Built-ins, verbose, 16 jobs
cargo run -p test262-runner -- --filter built-ins/Array --jobs 16 --verbose

# Equivalent xtask shorthand (if you add the alias):
cargo xtask test262 --filter language/expressions
# or documented as:
cargo run -p test262-runner -- --filter language/expressions
```

### Frontmatter handling

Parses the YAML between `/*---` and `---*/`:

- `description`, `esid`/`es5id`, `info` — recorded for diagnostics.
- `features: [BigInt, Symbol]` — parsed but not gated; the engine runs and fails closed.
- `flags: [module, async, raw, noStrict, onlyStrict]` — dispatch:
  - `module` → **skip** `module not yet wired (ESM stub)`. A future `compile_source_as_module` path will replace the skip.
  - `async` or `$DONE(` usage → **skip** `async harness not yet implemented`. The plan allows a minimal `done` callback via the microtask queue; currently skipped for determinism.
  - `raw` → no harness prepending at all.
  - `onlyStrict` → prepends `"use strict";` when the source lacks a directive.
  - `noStrict` → evaluated as written (sloppy).
  - `$262` host references → **skip** `requires $262 host object`.
- `negative: {phase: parse|early|resolution|runtime, type: SyntaxError|…}` — validated: parse/early/resolution expects a compile-time `Err`; runtime expects a throw. Type is matched leniently (`SyntaxError` ↔ "parse error"/"unexpected" etc.).
- `includes: [assert.js, propertyHelper.js]` — concatenated in order from `test262/harness/`. Traversal is rejected. When `includes` is empty but the body uses `assert.` / `Test262Error` / `$DONOTEVALUATE` (common in older Sputnik tests), `sta.js` + `assert.js` are auto-injected to avoid a trivial failure wave. When the checkout is missing, `MINIMAL_HARNESS_POLYFILL` (assert.sameValue, Test262Error, compareArray) is injected instead so the bootstrap can run offline.

### Execution model

For each matched `*.js` file:

1. Read + parse frontmatter; decide skip.
2. Load harness includes (or polyfill).
3. Build `combined = (strict prefix?) + harness + stripped test body`. Reject if `> 2 MiB`.
4. `Engine::new()` → `engine.eval(&combined)` → `engine.run_jobs()` (microtask checkpoint).
5. `catch_unwind` so one engine panic becomes one `Fail: engine panic`, never a harness crash.
6. Compare throw/result against `negative`. Panics and timeouts (`> 5 s` advisory, no preemption) are fails.

Parallelism is data-parallel over files via `rayon` when `jobs > 1`; files are sorted for deterministic TAP/JSON output.

### Reporting and exit code

- `--format human` (default): sorted table

  ```
  suite                          total   pass   fail   skip   pass%
  ─────────────────────────────────────────────────────────────────
  language/expressions             809    401    402      6   49.9%
  ```

  Percentages are over executable tests (`pass + fail`); skipped buckets show `—`.

- `--format tap`: TAP 13 (`1..N`, `ok`/`not ok`, `SKIP` directive, optional YAML diagnostics in verbose mode).
- `--format json`: `{"summary": {"total":…, "pass_rate":…}, "results": [{"path":…, "status":"pass|fail|skip"}]}`.

**Exit code is `0` iff every non-skipped test passes.** Fails and panics make it `1`; missing checkout is `2`; no matches is `1`. This is the CI gate.

## Trial run (bootstrap, 2026-08-26)

Full `test/language` (24 873 files, 8 jobs, auto-injected assert/sta):

```
suite                             total   pass   fail   skip   pass%
annexB                              845      9    792     44    1.1%
intl402                              21      0     21      0    0.0%
language/arguments-object           263      1    202     60    0.5%
language/asi                        102     74     28      0   72.5%
language/block-scope                145    108     37      0   74.5%
language/comments                    52     38     13      1   74.5%
language/computed-property-names     48      0     48      0    0.0%
language/destructuring               19     12      7      0   63.2%
language/directive-prologue          62     11     51      0   17.7%
language/eval-code                  347      0    294     53    0.0%
language/export                       3      0      0      3     —
language/expressions              11164   2037   6866   2261   22.9%
language/function-code              217     34    183      0   15.7%
language/future-reserved-words       55     49      6      0   89.1%
language/global-code                 42     14     15     13   48.3%
language/identifier-resolution       14      1     13      0    7.1%
language/identifiers                268    152    116      0   56.7%
language/import                     191      7     61    123   10.3%
language/keywords                    25     25      0      0  100.0%
language/line-terminators            41     17     24      0   41.5%
language/literals                   534    319    215      0   59.7%
language/module-code                755      6    151    598    3.8%
language/punctuators                 11     10      1      0   90.9%
language/reserved-words              27     14     12      1   53.8%
language/rest-parameters             11      3      8      0   27.3%
language/source-text                  1      0      1      0    0.0%
language/statementList               80     24     56      0   30.0%
language/statements                9350   1522   5308   2520   22.3%
language/types                      113     19     92      2   17.1%
language/white-space                 67      6     61      0    9.0%
TOTAL                             24873   4512  14682   5679   23.5%
```

- `cargo run -p test262-runner -- --filter language/expressions/assignment` (818 files) on the same build: **401 pass / 409 fail / 8 skip (49.5 % pass)**, now with harness auto-injection so `assert`-without-`includes` tests report real engine reasons (`in`/`instanceof` missing) instead of `assert is not defined`.

Top engine gaps surfaced (first 20 failures all share these):

- `in` / `instanceof` have no bytecode opcodes yet — blocks many `assignment`/`Object` descriptor tests.
- `attempt to add with overflow` panic in `v12-bccompiler/src/collect.rs:706` — collection phase overflows on some language files, caught as `Fail: engine panic`. Needs `checked_add` / saturated counter.
- Most built-ins and destructuring still stubbed; 22–23 % on `language/expressions` and `language/statements` is the Tier-0 baseline.

See `known-failures.md` for the curated bucket list and `fix-log.md` for the next targeted fixes.

## Fix-it loop

1. Pick a bucket in `known-failures.md` (start with the smallest surface, e.g. `in`/`instanceof` or the overflow).
2. Fix the engine, then re-run:

   ```sh
   ./conformance/run.sh --filter language/expressions/assignment
   # or
   cargo run -p test262-runner -- --filter language/expressions/assignment
   ```

3. Append to `fix-log.md` (date, filter, before/after counts, files changed, failure delta).
4. Move the bucket from `known-failures.md` to `fix-log.md` when green. The harness summary is the only scoreboard — keep `known-failures.md` honest.

## CI integration

Add to `.github/workflows/ci.yml` (or a nightly workflow). The harness is the gate for the plan's phases.

```yaml
name: nightly-test262

on:
  schedule:
    - cron: "0 3 * * *"   # nightly 03:00 UTC
  workflow_dispatch:

jobs:
  test262-gate:
    runs-on: ubuntu-latest
    timeout-minutes: 40
    steps:
      - uses: actions/checkout@v4
        with:
          submodules: recursive
          # If you use a shallow clone instead of a submodule, add:
          #   run: git clone --depth 1 https://github.com/tc39/test262 conformance/test262
      - uses: dtolnay/rust-toolchain@stable
      - name: Build runner
        run: cargo build -p test262-runner
      - name: Harness self-tests
        run: cargo test -p test262-runner -- --nocapture
        # or: cargo nextest run -p test262-runner
      - name: Test262 — language (Phase 1 gate: ≥60% pass)
        run: |
          cargo run -p test262-runner -- \
            --filter language \
            --jobs 4 \
            --format human,json \
            --json-out /tmp/test262-language.json
          # The runner already exits 1 on any non-skipped failure, so for a
          # pure percentage gate you can also check pass_rate with jq:
          #   PASS=$(jq -r .summary.pass_rate /tmp/test262-language.json)
          #   awk -v r="$PASS" 'BEGIN{exit !(r+0 >= 60)}'
          # Phase 1 requires ≥60% on language and zero interpreter-vs-JIT delta
          # (run twice: --jobs 1 vs with jit feature, diff the JSON).
      - name: Test262 — overall (Phase 2 gate: ≥85% pass)
        if: github.ref == 'refs/heads/main'
        run: |
          cargo run -p test262-runner -- \
            --jobs 8 \
            --format json \
            --json-out /tmp/test262-overall.json
          # Weekly overall gate — Phase 2 is ≥85% with Tier 1 default-on.
          # Archive the JSON for trend tracking:
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: test262-results
          path: /tmp/test262-*.json
```

For the Phase 1 gate in CI you can either let the runner's exit code gate (strict: zero fails) or gate on percentage with the `jq` line above, depending on how far the `known-failures.md` burn-down has progressed. The plan calls for ≥60 % on `language` before Tier 1 work starts, then ≥85 % overall before Tier 2.

## Roadmap to wiring

- **Module**: replace the `module not yet wired` skip with `v12_bccompiler::compile_source_as_module` (or `SourceType::module()`) plus the host `resolve`/`load` hooks described in `plan_idea.md` §4 (`v12-engine` Modules). The harness's `resolution` negative handling is already in place.
- **Async**: replace the `$DONE` skip with a host `print` hook that watches for `Test262:AsyncTestComplete` / `Test262:AsyncTestFailure:…` and drains the job queue with a small event loop (timeout ~1 s). The harness `doneprintHandle.js` already prints those markers.
- **$262 host**: expose `createRealm`, `detachArrayBuffer`, etc. behind a `#[cfg(test262_host)]` feature so `$262`-dependent tests become runnable.

## Troubleshooting

- `harness include error: assert.js: read error: No such file…` — the checkout is missing or the path is wrong. Check `--test262-root` and that `conformance/test262/harness/assert.js` exists.
- `attempt to add with overflow` — known compiler bug on some files; tracked in `known-failures.md`. The runner turns it into a fail, not a harness crash.
- `combined source too large` skip — a test + harness exceeded 2 MiB; raise the constant if needed for spec-size stress tests.
- `Suite — pass%` — skipped tests are not counted in the denominator.

License: same as v12 workspace (MIT OR Apache-2.0).
