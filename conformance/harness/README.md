# test262-runner

Run the [Test262](https://github.com/tc39/test262) conformance suite against v12.

You point it at a Test262 checkout; it discovers tests, evaluates each in a fresh `v12-engine`, and reports a summary that gates CI (exit 0 iff every non-skipped test passes).

## Prerequisites

- Rust 1.90+ and `cargo`
- A Test262 checkout — the runner looks for `conformance/test262/test/` by default

```sh
# first time
git clone --depth 1 https://github.com/tc39/test262 conformance/test262

# or, if you use submodules
git submodule update --init
```

Verify the checkout:

```sh
ls conformance/test262/test/language  # should list `expressions`, `statements`, …
ls conformance/test262/harness        # assert.js, sta.js, …
```

## Build

```sh
cargo build -p test262-runner          # debug build
cargo build -p test262-runner --release
```

No extra setup — the runner is a binary in the workspace (`conformance/harness/`).

## Quick start

Run the whole `test/language` subset (the Phase-1 gate) with auto parallelism:

```sh
cargo run -p test262-runner -- --filter language
```

Run a single file verbosely:

```sh
cargo run -p test262-runner -- --filter language/expressions/assignment --verbose
```

Dry-run: list what a filter would execute without running:

```sh
cargo run -p test262-runner -- --filter "built-ins/Array" --list | head
```

## How it works

For each discovered file:

1. **Frontmatter** — the `/*--- … ---*/` YAML block is parsed for `description`, `features`, `flags`, `negative`, and `includes`.
2. **Includes** — every entry in `includes` is prepended from `test262/harness/` (e.g., `assert.js`, `sta.js`). If the harness directory is missing, a minimal polyfill is used so the runner still executes.
3. **Flags**
   - `module` → compiled as a module (`compile_source_as_module`); if the engine's ESM loader is not yet wired, the test is skipped with reason `module not wired`.
   - `raw` → harness files are *not* prepended.
   - `async` / `uses `done`` → currently skipped (`async not yet supported`) — the runner detects the `done()` callback pattern.
   - `noStrict` / `onlyStrict` control the strict-mode wrapper.
4. **Negative** — `negative: { phase: parse, type: SyntaxError }` expects a compile-time error; `phase: runtime` expects a throw. The runner inverts pass/fail accordingly.
5. **Evaluation** — a fresh `Engine::new()` evaluates the combined harness+test source, then `engine.run_jobs()` drains microtasks. No state leaks between files.

Skipped tests are tallied, not failed. A skipped test never gates a release.

## CLI reference

```
test262-runner --help
```

| Flag | Default | Description |
|------|---------|-------------|
| `--filter <GLOB>` | *(all)* | Substring/glob over the path relative to `test/` — `language/expressions`, `built-ins/Map`, `*arrow*` |
| `--jobs <N>` | `0` (auto) | Parallel workers (`0` = `num_cpus`, capped at 64) |
| `--verbose` / `-v` | off | Per-test lines inline; with `--format tap` emits YAML diagnostics |
| `--include-skipped` | off | Show skipped tests in failure-style reporting |
| `--format <FORMAT>` | `human` | `human`, `tap`, `json`, or `tap,json` (comma-separated) |
| `--test262-root <PATH>` | `conformance/test262` | Override the checkout location |
| `--tap-out <PATH>` | — | Also write TAP to a file |
| `--json-out <PATH>` | — | Also write JSON to a file |
| `--list` | — | List discovered tests and exit |

## Output formats

**Human (default)** — summary table to stdout, one-line `summary:` to stderr for CI tailing:

```
suite language               total= 120 pass=  98 fail=   2 skip=  20 81.7%
summary: total=120 pass=98 fail=2 skip=20 pass_rate=81.7%
```

**TAP version 13** — one line per test, YAML diagnostics when `--verbose`:

```
TAP version 13
1..120
ok 1 - language/expressions/assignment/cstr-assignment.js
not ok 2 - language/expressions/arrow/missing-brace.js # TODO async not yet supported
```

**JSON** — machine-readable for dashboards:

```json
{
  "summary": { "total": 120, "passed": 98, "failed": 2, "skipped": 20, "pass_rate": 81.7, "by_suite": [...] },
  "results": [ { "relative": "language/...", "status": "pass", "message": "" }, … ]
}
```

Combine formats: `--format tap,json --tap-out tap.log --json-out results.json`

Exit codes: `0` iff `failed == 0`; `1` if any non-skipped test failed; `2` if the Test262 checkout is missing or the filter matched nothing.

## Examples

Run only `Array` built-ins, 4 workers, verbose TAP to a file:

```sh
cargo run -p test262-runner -- \
  --filter "built-ins/Array" --jobs 4 --verbose --format tap --tap-out tap.log
```

JSON for a dashboard, human summary also on stdout:

```sh
cargo run -p test262-runner -- \
  --filter language --format human,json --json-out results.json | tee human.log
```

In CI, gate on the exit code and upload the JSON:

```yaml
- run: cargo run -p test262-runner -- --filter language --format json --json-out results.json
- uses: actions/upload-artifact@v4
  with: { name: results, path: results.json }
```

## CI integration

Add to `.github/workflows/ci.yml`:

```yaml
test262-language:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
      with: { submodules: true }  # if test262 is a submodule
    - uses: dtolnay/rust-toolchain@stable
    - run: cargo run -p test262-runner -- --filter language --format human,json --json-out results.json
```

Phase gates from the project plan: Phase 1 requires `≥60%` of `test/language`; Phase 2 `≥85%` overall with zero non-skipped failures when the baseline JIT is on.

## Troubleshooting

**`error: Test262 checkout not found`** — you cloned to the wrong place. Check `conformance/test262/test` exists or pass `--test262-root /path/to/test262`.

**Many tests skipped as `async not yet supported`** — expected until the engine's async `done()` bridge lands. Filter them out with `--filter 'language/statements'` or similar.

**A test fails only under `--jobs >1`** — likely shared global state. The runner creates a fresh `Engine` per file; if you added global statics, make them thread-local.

**Need to run one file quickly** — use a substring filter that matches its path:

```sh
cargo run -p test262-runner -- --filter language/expressions/object/literal --verbose
```

## Contributing

The runner lives in `conformance/harness/src/`:

- `frontmatter.rs` — YAML comment parsing
- `runner.rs` — discovery + per-test execution (`run_single_test`)
- `report.rs` — `Summary` aggregation, `emit_tap` / `emit_json` / `emit_summary`

Add a harness helper by editing `harness.rs`; add a new output format in `report.rs`; both are covered by unit tests in the crate (`cargo nextest run -p test262-runner`).
