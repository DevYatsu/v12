#![forbid(unsafe_code)]
#![allow(dead_code)]

//! `test262-runner`: execute Test262 tests via `v12-engine`.
//!
//! Each test file is parsed for its YAML frontmatter, skipped or prepared
//! according to `flags`/`includes`/`negative`, then evaluated in a fresh
//! `Engine`. Results are reported as TAP, JSON, and a human summary table.
//!
//! Exit code `0` iff every non-skipped test passes (CI gate).

mod frontmatter;
mod harness;
mod report;
mod runner;

use std::path::{Path, PathBuf};

use clap::Parser;
use rayon::prelude::*;

use crate::report::{Summary, emit_json, emit_summary, emit_tap};
use crate::runner::{HarnessConfig, Status, TestOutcome, discover_tests, run_single_test};

/// Default parallelism: number of logical CPUs, capped at 16 to bound memory.
const DEFAULT_JOBS: usize = 8;

/// Maximum value for `--jobs`.
const MAX_JOBS: usize = 64;

/// CLI for the Test262 runner.
#[derive(Debug, Parser)]
#[command(name = "test262-runner", version, about, long_about = None)]
struct Cli {
    /// Glob or substring filter over the test path relative to `test/`.
    ///
    /// Examples: `language/expressions`, `language/statements/*break*`,
    /// `built-ins/Array`.
    #[arg(long, value_name = "GLOB")]
    filter: Option<String>,

    /// Number of parallel workers. `0` means auto (num_cpus).
    #[arg(long, value_name = "N", default_value_t = 0)]
    jobs: usize,

    /// Verbose per-test output (prints failure messages inline; with `--format tap`
    /// emits YAML diagnostics).
    #[arg(long, short)]
    verbose: bool,

    /// Include skipped tests in `failed`-style reporting (by default they are
    /// just tallied and shown in the summary).
    #[arg(long)]
    include_skipped: bool,

    /// Output format.
    ///
    /// `human` (default) prints a summary table to stdout. `tap` emits
    /// TAP version 13. `json` emits a JSON object with `summary` and `results`.
    /// Formats can be combined as `tap,json` (comma-separated) to emit both.
    #[arg(long, value_name = "FORMAT", default_value = "human")]
    format: String,

    /// Path to the Test262 checkout root (contains `test/` and `harness/`).
    ///
    /// Defaults to `conformance/test262` relative to the current directory,
    /// then to `test262` and `../test262` for flexibility.
    #[arg(long, value_name = "PATH")]
    test262_root: Option<PathBuf>,

    /// Write TAP output to this file in addition to stdout (if applicable).
    #[arg(long, value_name = "PATH")]
    tap_out: Option<PathBuf>,

    /// Write JSON output to this file (when `--format` includes `json`, stdout
    /// still gets JSON; this copies it to a file as well).
    #[arg(long, value_name = "PATH")]
    json_out: Option<PathBuf>,

    /// List discovered tests and exit (dry run).
    #[arg(long)]
    list: bool,
}

fn main() {
    let cli = Cli::parse();

    let jobs = normalize_jobs(cli.jobs);
    // Configure rayon for the chosen parallelism.
    if jobs > 1 {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global();
    }

    let test262_root = resolve_test262_root(cli.test262_root.as_deref());
    if !test262_root.join("test").is_dir() {
        eprintln!(
            "error: Test262 checkout not found at `{}`.\n\
             Hint: `git clone --depth 1 https://github.com/tc39/test262 {}` or \
             `git submodule update --init` if using submodules.\n\
             See conformance/README.md for setup.",
            test262_root.display(),
            test262_root.display()
        );
        std::process::exit(2);
    }

    let harness_config = HarnessConfig::new(test262_root.clone());
    if harness_config.harness_dir.is_none() {
        eprintln!(
            "warning: harness directory not found under `{}` — \
             tests requiring harness files will use the minimal polyfill.",
            test262_root.display()
        );
    }

    let files = discover_tests(&test262_root, cli.filter.as_deref());
    if cli.list {
        for f in &files {
            let rel = f
                .strip_prefix(test262_root.join("test"))
                .unwrap_or(f)
                .display();
            println!("{rel}");
        }
        println!("{} tests matched", files.len());
        return;
    }

    if files.is_empty() {
        eprintln!(
            "no tests matched filter {:?} under {}",
            cli.filter,
            test262_root.display()
        );
        std::process::exit(1);
    }

    eprintln!(
        "test262-runner: {} tests matched (filter={:?}, jobs={jobs}, root={})",
        files.len(),
        cli.filter,
        test262_root.display()
    );

    let outcomes: Vec<TestOutcome> = if jobs <= 1 {
        files
            .iter()
            .map(|p| run_single_test(p, &harness_config))
            .collect()
    } else {
        files
            .par_iter()
            .map(|p| run_single_test(p, &harness_config))
            .collect()
    };

    // Verbose per-test line during the run has already happened via stderr;
    // additionally echo each outcome if --verbose and not TAP (which already
    // lists them).
    if cli.verbose && !cli.format.contains("tap") {
        for o in &outcomes {
            match o.status {
                Status::Pass => println!("ok - {}", o.relative),
                Status::Fail => println!("not ok - {} — {}", o.relative, o.message),
                Status::Skip => {
                    if cli.include_skipped {
                        println!(
                            "skip - {} — {}",
                            o.relative,
                            o.skip_reason.as_deref().unwrap_or("skipped")
                        );
                    }
                }
            }
        }
    }

    let mut sorted = outcomes;
    sorted.sort_by(|a, b| a.relative.cmp(&b.relative));
    let summary = Summary::from_outcomes(&sorted);

    let wants_tap = cli.format.contains("tap");
    let wants_json = cli.format.contains("json");
    let wants_human = cli.format.contains("human") || (!wants_tap && !wants_json);

    if wants_tap {
        let mut buf = Vec::new();
        emit_tap(&sorted, cli.verbose, &mut buf);
        let text = String::from_utf8_lossy(&buf);
        print!("{text}");
        if let Some(path) = &cli.tap_out {
            let _ = std::fs::write(path, &buf);
        }
    }

    if wants_json {
        let mut buf = Vec::new();
        emit_json(&sorted, &summary, &mut buf);
        if !wants_tap {
            // If TAP already printed to stdout, avoid interleaving JSON there
            // unless the user explicitly asked for both on stdout. When both
            // are requested, TAP goes to stdout and JSON to --json-out; but
            // if no --json-out, we still emit JSON after TAP with a separator.
            if wants_human || wants_tap {
                if let Some(path) = &cli.json_out {
                    let _ = std::fs::write(path, &buf);
                } else {
                    println!("\n--- JSON ---");
                    print!("{}", String::from_utf8_lossy(&buf));
                }
            } else {
                print!("{}", String::from_utf8_lossy(&buf));
            }
        } else if let Some(path) = &cli.json_out {
            let _ = std::fs::write(path, &buf);
        }
        // If only json on stdout
        if !wants_tap && !wants_human && cli.json_out.is_none() {
            // already printed above
        } else if !wants_tap && cli.json_out.is_some() {
            // JSON to stdout plus copy already written; also print to stdout
            print!("{}", String::from_utf8_lossy(&buf));
        } else if wants_json && !wants_tap && wants_human {
            print!("{}", String::from_utf8_lossy(&buf));
        }
    }

    if wants_human || (!wants_tap && !wants_json) {
        let mut buf = Vec::new();
        emit_summary(&summary, &mut buf);
        print!("{}", String::from_utf8_lossy(&buf));
    }

    // Always print a one-line summary to stderr for CI log tailing.
    eprintln!(
        "summary: total={} pass={} fail={} skip={} pass_rate={}",
        summary.total,
        summary.passed,
        summary.failed,
        summary.skipped,
        summary
            .pass_rate
            .map(|r| format!("{r:.1}%"))
            .unwrap_or_else(|| "—".to_string())
    );

    // Per-suite detail to stderr in verbose mode.
    if cli.verbose {
        for s in &summary.by_suite {
            eprintln!(
                "  suite {:<28} total={:>4} pass={:>4} fail={:>4} skip={:>4} {}",
                s.suite,
                s.total,
                s.passed,
                s.failed,
                s.skipped,
                s.pass_rate()
                    .map(|r| format!("{r:.1}%"))
                    .unwrap_or_else(|| "—".to_string())
            );
        }
    }

    // Non-TAP failures to stderr when not already shown.
    if !cli.verbose && summary.failed > 0 {
        eprintln!("Failing tests (first 20):");
        for o in sorted.iter().filter(|o| o.status == Status::Fail).take(20) {
            eprintln!("  {} — {}", o.relative, o.message);
        }
        if summary.failed > 20 {
            eprintln!("  ... and {} more", summary.failed - 20);
        }
    }

    // Exit code 0 iff every non-skipped test passed.
    if summary.failed == 0 {
        std::process::exit(0);
    } else {
        std::process::exit(1);
    }
}

fn normalize_jobs(requested: usize) -> usize {
    if requested == 0 {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(DEFAULT_JOBS);
        cpus.clamp(1, MAX_JOBS)
    } else {
        requested.clamp(1, MAX_JOBS)
    }
}

fn resolve_test262_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    // Probe common locations relative to cwd.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let candidates = [
        cwd.join("conformance").join("test262"),
        cwd.join("test262"),
        cwd.join("../test262"),
        PathBuf::from("conformance/test262"),
        PathBuf::from("test262"),
    ];
    for c in &candidates {
        if c.join("test").is_dir() {
            return c.clone();
        }
    }
    // Default even if not present — lets main print a helpful error.
    cwd.join("conformance").join("test262")
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn normalize_jobs_zero_is_auto() {
        let n = normalize_jobs(0);
        assert!((1..=MAX_JOBS).contains(&n));
    }

    #[test]
    fn normalize_jobs_clamped() {
        assert_eq!(normalize_jobs(1), 1);
        assert_eq!(normalize_jobs(1000), MAX_JOBS);
    }

    #[test]
    fn resolve_explicit_wins() {
        let p = PathBuf::from("/tmp/foo");
        assert_eq!(resolve_test262_root(Some(&p)), p);
    }
}
