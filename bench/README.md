# bench/

Micro-benchmarks for v12.

## Contents

- `hyperfine-results.md` — wall-clock timing via hyperfine (read-only runs, warmup 3 × 10, `cargo run` overhead vs direct binary).
- (Future) `criterion/` — Criterion.rs harnesses if added.

## How to reproduce

Prereqs: `hyperfine 1.20.0` (`cargo install hyperfine --locked` or `brew install hyperfine`), `cargo`, release build.

```sh
# One-time: build release binary (avoids cargo overhead in the measurement)
cargo build --release -p v12-cli

# Representative execution (cargo wrapper — includes ~250 ms cargo overhead)
hyperfine --warmup 3 --runs 10 \
  "cargo run --release -p v12-cli -- examples/01-basics.js" \
  "cargo run --release -p v12-cli -- examples/02-variables.js" \
  "cargo run --release -p v12-cli -- examples/04-control-flow.js" \
  "cargo run --release -p v12-cli -- examples/09-closures.js"

# Compilation-only (no execution)
hyperfine --warmup 3 --runs 10 \
  "cargo run --release -p v12-cli -- --disasm examples/01-basics.js"

# Isolated engine timing (no cargo overhead) — preferred for engine comparisons
hyperfine --warmup 3 --runs 10 --shell=none \
  "./target/release/v12 examples/01-basics.js" \
  "./target/release/v12 examples/02-variables.js" \
  "./target/release/v12 examples/04-control-flow.js" \
  "./target/release/v12 examples/09-closures.js"

# Disasm isolated
hyperfine --warmup 3 --runs 10 --shell=none \
  "./target/release/v12 --disasm examples/01-basics.js"
```

For sub-5 ms commands hyperfine warns about shell calibration noise. Use
`--shell=none` for direct-binary runs; warmup absorbs filesystem caches.
Median + relative stddev are the stable columns — cargo-run means are
dominated by process spawn, not engine time.

## Allocation / clone audit

Grep for `.clone()` across `crates/v12-*` and triage by cost:

- `Handle` / `JsValue` are `Copy` (u32/u64) — `clone()` is free, flagged by
  `clippy::clone_on_copy` but harmless. Keep.
- `Arc::clone` is atomic inc — cheap, keep but prefer sharing over `Vec` copy.
- `String` / `Vec<T>` / `HashMap` clones are heap allocations — audit per
  call site; replace with `&[T]` borrow, `Cow`, `Arc` sharing, or
  `std::mem::take` where low-risk.

`cargo clippy --workspace --all-targets -- -W clippy::redundant_clone -W clippy::clone_on_copy`
is the gate (currently 0 warnings for core crates).
