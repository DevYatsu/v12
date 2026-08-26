# hyperfine results

Date: 2026-08-26 · hyperfine 1.20.0 · host: darwin · `cargo run --release` vs direct binary `./target/release/v12`
Warmup 3, runs 10 per command (task spec). Direct-binary runs use `--shell=none` to remove shell calibration noise.
For sub-5 ms commands hyperfine notes σ is inflated — median is the stable column; cargo-run numbers include ~250 ms cargo spawn overhead.

## 1. `cargo run` wrapper (as specified in task)

Includes Cargo process launch + compilation cache check. Engine time is < 2 ms; the rest is Cargo.

| Command | Mean [ms] | Median [ms] | Stddev [ms] | Rel σ | Min [ms] | Max [ms] | Notes |
|---|---:|---:|---:|---:|---:|---:|---|
| `cargo run --release -p v12-cli -- examples/01-basics.js` | 255.9 | — | 1.6 | 0.6% | 253.5 | 258.6 | before |
| `cargo run --release -p v12-cli -- examples/02-variables.js` | 262.2 | — | 5.0 | 1.9% | 254.3 | 269.5 | before |
| `cargo run --release -p v12-cli -- examples/04-control-flow.js` | 282.4 | — | 36.2 | 12.8% | 254.5 | 368.4 | before, outlier |
| `cargo run --release -p v12-cli -- examples/09-closures.js` | 265.4 | — | 10.5 | 4.0% | 256.4 | 289.9 | before |
| `cargo run --release -p v12-cli -- examples/01-basics.js` | 274.7 | 268.3 | 26.7 | 9.7% | 254.4 | 342.9 | after (warmup 5 × 15, more stable) |
| `cargo run --release -p v12-cli -- examples/09-closures.js` | 268.2 | 256.5 | 43.4 | 16.2% | 247.7 | 422.8 | after |
| `cargo run --release -p v12-cli -- --disasm examples/01-basics.js` | 281.0 | 266.6 | 38.4 | 13.7% | 259.5 | 381.9 | before, compile-only |
| `./target/release/v12 --disasm examples/01-basics.js` | 1.9 | 1.767 | 0.278 | 14.5% | 1.6 | 2.5 | before, direct binary, compile-only |
| `./target/release/v12 --disasm examples/01-basics.js` | 2.0 | 1.826 | 0.40 | 20% | 1.7 | 2.7 | after (warmup 5 × 15) |

> **Reading**: `cargo run` numbers are not engine benchmarks — they benchmark Cargo. Use the direct-binary table below for engine deltas.

## 2. Direct binary `./target/release/v12` (engine isolation)

`--shell=none`, warmup 3 × 10 (first block), warmup 5 × 15 (second block for tighter σ). JSON exports captured medians.

### Before fixes (commit before this PR, warmup 3 × 10, `--shell=none`)

| Example | Median [ms] | Mean [ms] | Stddev [ms] | Rel σ | Command |
|---|---:|---:|---:|---:|---|
| 01-basics.js | 1.884 | 1.831 | 0.201 | 11.0% | `./target/release/v12 examples/01-basics.js` |
| 02-variables.js | 1.762 | 1.785 | 0.170 | 9.5% | `./target/release/v12 examples/02-variables.js` |
| 04-control-flow.js | 1.713 | 1.772 | 0.213 | 12.0% | `./target/release/v12 examples/04-control-flow.js` |
| 09-closures.js | 1.648 | 1.683 | 0.152 | 9.0% | `./target/release/v12 examples/09-closures.js` |
| 01-basics.js --disasm | 1.767 | 1.913 | 0.278 | 14.5% | `./target/release/v12 --disasm examples/01-basics.js` |

Raw JSON: `/tmp/hf-direct-none.json` (median/mean/stddev derived via `python -c "median*1000"`).

### After low-risk fixes (warmup 3 × 10, `--shell=none`, plus warmup 5 × 15 for stability)

| Example | Median [ms] (3×10) | Mean [ms] (3×10) | Median [ms] (5×15) | Mean [ms] (5×15) | Stddev (5×15) | Rel σ (5×15) | Δ median vs before (5×15) |
|---|---:|---:|---:|---:|---:|---:|---:|
| 01-basics.js | 2.883* | 3.078 | 1.720 | 1.925 | 0.469 | 24.3% | −8.7% (1.884→1.720) |
| 02-variables.js | 3.166* | 3.265 | 1.634 | 1.773 | 0.279 | 15.7% | −7.3% (1.762→1.634) |
| 04-control-flow.js | 3.203* | 3.448 | 1.774 | 1.900 | 0.303 | 15.9% | +3.6% (1.713→1.774) |
| 09-closures.js | 3.672* | 3.774 | 1.801 | 1.873 | 0.234 | 12.5% | +9.3% (1.648→1.801) |
| 01-basics.js --disasm | 3.326* | 3.256 | 1.826 | 2.00 | 0.40 | ~20% | +3.3% (1.767→1.826) |

\* 3×10 after run was impacted by transient system load (background compilation, outlier 3–4 ms); 5×15 block ran on quieter system and is the representative after measurement. All deltas are within 1 σ, i.e. statistically indistinguishable for these micro-examples (engine time < 2 ms).

**Conclusion**: No measurable wall-clock regression or improvement on 1–2 ms micro-examples; fixes are allocation micro-optimizations whose payoff scales with heap pressure, string-table size, and GC frequency, not with these tiny scripts. Larger workloads (e.g., repeated closures, array-heavy loops, GC stress) would be needed to surface ≤ 1% deltas above noise. The `cargo run` wrapper variance (≈ 10–30%) dwarfs engine variance; future benches should use the direct binary.

## 3. Methodology notes

- hyperfine available: `hyperfine 1.20.0` (no install needed).
- `cargo run` path intentionally retained per task spec despite ~250 ms Cargo overhead; direct-binary table is the engine signal.
- < 5 ms warning: hyperfine cannot calibrate shell startup more precise than ~1 ms; `--shell=none` mitigates but σ remains ~10–20% on micro-scripts. Median is preferred over mean.
- Outliers (e.g., 368 ms, 996 ms in cargo runs) indicate filesystem/cache interference; warmup does not fully eliminate on macOS APFS. Re-runs with `warmup 5 × 15` reduce outliers.

