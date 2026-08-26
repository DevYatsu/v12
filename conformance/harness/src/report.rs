#![forbid(unsafe_code)]

//! Reporting for the Test262 harness: TAP, JSON, and the human summary table.
//!
//! The summary includes total / passed / failed / skipped plus a per-suite
//! breakdown and a pass percentage. All output is deterministic and sorted.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::runner::{Status, TestOutcome};

/// Aggregated results for a single suite bucket (e.g. `language/expressions`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SuiteStats {
    /// Suite bucket name.
    pub suite: String,
    /// Total tests in the bucket.
    pub total: usize,
    /// Passing tests.
    pub passed: usize,
    /// Failing tests.
    pub failed: usize,
    /// Skipped tests.
    pub skipped: usize,
}

impl SuiteStats {
    /// Pass rate as a percentage of non-skipped tests. `None` when the bucket
    /// has no executable tests.
    #[must_use]
    pub fn pass_rate(&self) -> Option<f64> {
        let executable = self.passed + self.failed;
        if executable == 0 {
            return None;
        }
        Some((self.passed as f64 / executable as f64) * 100.0)
    }
}

/// Full run summary.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// Total discovered tests.
    pub total: usize,
    /// Passing tests.
    pub passed: usize,
    /// Failing tests.
    pub failed: usize,
    /// Skipped tests.
    pub skipped: usize,
    /// Per-suite breakdown, sorted by suite name.
    pub by_suite: Vec<SuiteStats>,
    /// Overall pass rate over non-skipped tests, if any.
    pub pass_rate: Option<f64>,
}

impl Summary {
    /// Builds a summary from a slice of outcomes.
    #[must_use]
    pub fn from_outcomes(outcomes: &[TestOutcome]) -> Self {
        let total = outcomes.len();
        let passed = outcomes.iter().filter(|o| o.status == Status::Pass).count();
        let failed = outcomes.iter().filter(|o| o.status == Status::Fail).count();
        let skipped = outcomes.iter().filter(|o| o.status == Status::Skip).count();

        let mut by_suite_map: BTreeMap<String, SuiteStats> = BTreeMap::new();
        for o in outcomes {
            let entry = by_suite_map
                .entry(o.suite.clone())
                .or_insert_with(|| SuiteStats {
                    suite: o.suite.clone(),
                    total: 0,
                    passed: 0,
                    failed: 0,
                    skipped: 0,
                });
            entry.total += 1;
            match o.status {
                Status::Pass => entry.passed += 1,
                Status::Fail => entry.failed += 1,
                Status::Skip => entry.skipped += 1,
            }
        }

        let by_suite = by_suite_map.into_values().collect::<Vec<_>>();
        let pass_rate = {
            let executable = passed + failed;
            if executable == 0 {
                None
            } else {
                Some((passed as f64 / executable as f64) * 100.0)
            }
        };

        Self {
            total,
            passed,
            failed,
            skipped,
            by_suite,
            pass_rate,
        }
    }
}

/// Emits a TAP (Test Anything Protocol) report to `writer`.
///
/// TAP version 13 is used. Each outcome becomes one `ok`/`not ok` line
/// with a `SKIP` directive for skipped tests. Verbose mode appends the
/// failure message as a YAML diagnostics block.
pub fn emit_tap(outcomes: &[TestOutcome], verbose: bool, mut writer: impl std::io::Write) {
    let _ = writeln!(writer, "TAP version 13");
    let _ = writeln!(writer, "1..{}", outcomes.len());
    for (idx, o) in outcomes.iter().enumerate() {
        let n = idx + 1;
        let rel = &o.relative;
        match &o.status {
            Status::Pass => {
                let _ = writeln!(writer, "ok {n} - {rel}");
            }
            Status::Fail => {
                let _ = writeln!(writer, "not ok {n} - {rel}");
                if verbose && !o.message.is_empty() {
                    let _ = writeln!(writer, "  ---");
                    let _ = writeln!(writer, "  message: {}", escape_yaml(&o.message));
                    let _ = writeln!(writer, "  ...");
                }
            }
            Status::Skip => {
                let reason = o.skip_reason.as_deref().unwrap_or("skipped");
                let _ = writeln!(writer, "ok {n} - {rel} # SKIP {reason}");
            }
        }
    }
}

/// Emits a JSON report: `{summary, results}`.
pub fn emit_json(outcomes: &[TestOutcome], summary: &Summary, mut writer: impl std::io::Write) {
    #[derive(Serialize)]
    struct JsonReport<'a> {
        summary: &'a Summary,
        results: Vec<JsonResult<'a>>,
    }
    #[derive(Serialize)]
    struct JsonResult<'a> {
        path: &'a str,
        suite: &'a str,
        status: &'a str,
        message: &'a str,
        duration_ms: u128,
    }
    let results = outcomes
        .iter()
        .map(|o| JsonResult {
            path: &o.relative,
            suite: &o.suite,
            status: match o.status {
                Status::Pass => "pass",
                Status::Fail => "fail",
                Status::Skip => "skip",
            },
            message: &o.message,
            duration_ms: o.duration_ms,
        })
        .collect::<Vec<_>>();
    let report = JsonReport { summary, results };
    let json = serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string());
    let _ = writeln!(writer, "{json}");
}

/// Prints a human-readable summary table to `writer`.
///
/// Example:
///
/// ```text
///    suite                          total   pass   fail  skip  pass%
///    ────────────────────────────────────────────────────────────────
///    built-ins/Array                    2      1      0     1   100.0%
///    language/expressions               3      2      1     0    66.7%
/// ```
pub fn emit_summary(summary: &Summary, mut writer: impl std::io::Write) {
    let _ = writeln!(writer);
    let _ = writeln!(
        writer,
        "{:<32} {:>6} {:>6} {:>6} {:>6} {:>7}",
        "suite", "total", "pass", "fail", "skip", "pass%"
    );
    let _ = writeln!(writer, "{}", "─".repeat(68));
    for s in &summary.by_suite {
        let rate = s
            .pass_rate()
            .map(|r| format!("{r:>6.1}%"))
            .unwrap_or_else(|| "    — ".to_string());
        let _ = writeln!(
            writer,
            "{:<32} {:>6} {:>6} {:>6} {:>6} {rate}",
            s.suite, s.total, s.passed, s.failed, s.skipped,
        );
    }
    let _ = writeln!(writer, "{}", "─".repeat(68));
    let total_rate = summary
        .pass_rate
        .map(|r| format!("{r:.1}%"))
        .unwrap_or_else(|| "—".to_string());
    let _ = writeln!(
        writer,
        "{:<32} {:>6} {:>6} {:>6} {:>6} {:>7}",
        "TOTAL", summary.total, summary.passed, summary.failed, summary.skipped, total_rate
    );
    let _ = writeln!(writer);
}

fn escape_yaml(s: &str) -> String {
    // Minimal: quote if contains `:` or newline.
    if s.contains('\n') || s.contains(':') {
        format!("\"{}\"", s.replace('"', "\\\"").replace('\n', "\\n"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::Frontmatter;
    use std::path::PathBuf;

    fn outcome(suite: &str, status: Status) -> TestOutcome {
        TestOutcome {
            path: PathBuf::from(format!("{suite}/x.js")),
            relative: format!("{suite}/x.js"),
            suite: suite.to_string(),
            status,
            message: String::new(),
            skip_reason: None,
            duration_ms: 1,
            frontmatter: Frontmatter::default(),
        }
    }

    #[test]
    fn summary_counts() {
        let outcomes = vec![
            outcome("language", Status::Pass),
            outcome("language", Status::Fail),
            outcome("language", Status::Skip),
            outcome("built-ins/Array", Status::Pass),
        ];
        let s = Summary::from_outcomes(&outcomes);
        assert_eq!(s.total, 4);
        assert_eq!(s.passed, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.by_suite.len(), 2);
    }

    #[test]
    fn suite_pass_rate() {
        let ss = SuiteStats {
            suite: "language".to_string(),
            total: 3,
            passed: 2,
            failed: 1,
            skipped: 0,
        };
        let rate = ss.pass_rate().unwrap();
        assert!((rate - 66.666).abs() < 0.1);
    }

    #[test]
    fn empty_pass_rate_none() {
        let ss = SuiteStats {
            suite: "x".to_string(),
            total: 2,
            passed: 0,
            failed: 0,
            skipped: 2,
        };
        assert!(ss.pass_rate().is_none());
    }

    #[test]
    fn emit_tap_format() {
        let outcomes = vec![
            outcome("language/a.js", Status::Pass),
            outcome("language/b.js", Status::Fail),
        ];
        let mut buf = Vec::new();
        emit_tap(&outcomes, false, &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("TAP version 13"));
        assert!(s.contains("1..2"));
        assert!(s.contains("ok 1"));
        assert!(s.contains("not ok 2"));
    }

    #[test]
    fn emit_json_valid() {
        let outcomes = vec![outcome("language/a.js", Status::Pass)];
        let summary = Summary::from_outcomes(&outcomes);
        let mut buf = Vec::new();
        emit_json(&outcomes, &summary, &mut buf);
        let s = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("summary").is_some());
        assert!(v.get("results").is_some());
    }
}
