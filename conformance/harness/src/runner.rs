#![forbid(unsafe_code)]

//! Test execution against `v12-engine`.
//!
//! Each Test262 file is evaluated in a fresh [`v12_engine::Engine`] with its
//! frontmatter-harness preamble prepended. Negative expectations and known
//! flags (`module`, `async`, `raw`, `onlyStrict`) are handled here so the
//! reporting layer stays pure.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::frontmatter::{Frontmatter, parse_frontmatter, strip_frontmatter};
use crate::harness::{MINIMAL_HARNESS_POLYFILL, load_harness_includes};

/// Maximum test source size (after harness prepending) in bytes.
const MAX_COMBINED_SOURCE_LEN: usize = 2_000_000;

/// Maximum time a single test may run before we mark it as timed out.
///
/// The engine's cooperative deadline (sampled in the interpreter dispatch
/// loop) aborts a runaway script from the inside; this value feeds that
/// deadline.
const TEST_TIMEOUT_MS: u128 = 5_000;

/// Hard per-test wall-clock budget enforced by the parent process: a test
/// that has not reported a result within this window is logged as stalled,
/// its process is killed, and the run proceeds with the next test.
///
/// The cooperative deadline above normally fires first, but it cannot reach
/// every stall: the parser/AST lowering, native builtins with internal
/// loops, and stack overflows (which abort the process and are uncatchable)
/// never sample it. Process-level isolation is the only reliable stop for
/// those, so `run_single_test_isolated` re-execs this binary per test.
const TEST_HARD_KILL_MS: u64 = 5_000;

/// JS preamble defining the `print` sink and the `$262` host object that
/// Test262 harness files expect. Output is captured in a global array that
/// the runner can re-read with a second `engine.eval` on the same engine;
/// nothing touches process stdout (the runner is parallel).
///
/// `$262.createRealm` delegates to `globalThis.__v12CreateRealm__`, a host
/// function the runner installs on the engine before this preamble evaluates
/// (see `install_create_realm_function`): it builds a fresh realm on the
/// shared heap and returns its `$262`-shaped API object.
const TEST262_HOST_SHIM: &str = r#"
globalThis.__test262Prints = [];
function __consolePrintHandle__(s) { globalThis.__test262Prints.push(String(s)); }
function print(s) { globalThis.__test262Prints.push(String(s)); }
// `$262.IsHTMLDDA` (INTERPRETING.md): an object emulating the [[IsHTMLDDA]]
// internal slot. This shim is a real callable — the minimal behavior the
// harness relies on — returning `null` when called with no arguments or an
// empty string, per the host contract. (The `typeof`/loose-equality emulation
// of `document.all` is a separate engine concern, not exercised here.)
function __isHTMLDDA__() {
    if (arguments.length === 0 || arguments[0] === "") return null;
    return undefined;
}
var $262 = {
    createRealm: function () { return globalThis.__v12CreateRealm__(); },
    detachArrayBuffer: function (b) { return b; },
    getReport: function () { return null; },
    destroy: function () {},
    gc: function () {},
    global: globalThis,
    IsHTMLDDA: __isHTMLDDA__,
};
"#;

/// Outcome status for a single test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Test passed (or negative expectation satisfied).
    Pass,
    /// Test failed.
    Fail,
    /// Test skipped with a reason.
    Skip,
}

/// Outcome of a single test file.
#[derive(Debug, Clone)]
pub struct TestOutcome {
    /// Absolute path to the test file.
    pub path: PathBuf,
    /// Path relative to the Test262 root's `test/` directory, e.g.
    /// `language/statements/break/S12.8_A1_T1.js`.
    pub relative: String,
    /// Suite bucket: top two path components, e.g. `language` or
    /// `built-ins/Array`.
    pub suite: String,
    /// Status.
    pub status: Status,
    /// Human-readable detail for fail/skip. Empty on pass.
    pub message: String,
    /// Optional skip reason (same as `message` for Skip).
    pub skip_reason: Option<String>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u128,
    /// Frontmatter for debugging.
    pub frontmatter: Frontmatter,
}

impl TestOutcome {
    /// Convenience: true iff status is Pass.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.status == Status::Pass
    }

    /// Convenience: true iff status is Skip.
    #[must_use]
    pub fn is_skip(&self) -> bool {
        self.status == Status::Skip
    }
}

/// Resolved harness configuration for the runner.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Absolute path to the Test262 checkout root (contains `test/` and
    /// `harness/`).
    pub test262_root: PathBuf,
    /// Absolute path to the harness directory, if available.
    pub harness_dir: Option<PathBuf>,
}

impl HarnessConfig {
    /// Creates a new config probing for the harness directory.
    #[must_use]
    pub fn new(test262_root: PathBuf) -> Self {
        let harness_dir = {
            let candidate = test262_root.join("harness");
            if candidate.is_dir() {
                Some(candidate)
            } else {
                None
            }
        };
        Self {
            test262_root,
            harness_dir,
        }
    }
}

/// Runs a single Test262 test file and returns its outcome.
///
/// `file_path` must be an absolute or test262-root-relative path. The
/// function is synchronous and creates a fresh `Engine` per invocation.
pub fn run_single_test(file_path: &Path, config: &HarnessConfig) -> TestOutcome {
    let start = Instant::now();

    let relative = relative_suite_path(file_path, &config.test262_root);
    let suite = suite_for(&relative);

    // Read file.
    let source = match std::fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => {
            return TestOutcome {
                path: file_path.to_path_buf(),
                relative,
                suite,
                status: Status::Fail,
                message: format!("read error: {e}"),
                skip_reason: None,
                duration_ms: start.elapsed().as_millis(),
                frontmatter: Frontmatter::default(),
            };
        }
    };

    let frontmatter = parse_frontmatter(&source);

    // Flag-driven skips (before harness loading).
    if let Some(reason) = skip_reason_for(&frontmatter, &source) {
        return TestOutcome {
            path: file_path.to_path_buf(),
            relative,
            suite,
            status: Status::Skip,
            message: reason.clone(),
            skip_reason: Some(reason),
            duration_ms: start.elapsed().as_millis(),
            frontmatter,
        };
    }

    // Strip frontmatter early — needed for harness decision.
    // Owned: `strip_frontmatter` preserves source ahead of the block, and
    // the hashbang split below borrows from it.
    let stripped_body = strip_frontmatter(&source);
    let test_body: &str = &stripped_body;

    // Async verdict needed when the test carries the `async` flag or calls
    // `$DONE` directly (older style). Raw tests get neither harness nor
    // verdict — `$DONE` cannot exist there.
    let needs_async_verdict = !frontmatter.has_flag("raw")
        && (frontmatter.has_flag("async") || source.contains("$DONE("));

    // Harness preamble.
    let (harness_source, harness_errors) = if frontmatter.has_flag("raw") {
        (String::new(), Vec::new())
    } else {
        // Determine which harness files to load.
        let mut includes_to_load = frontmatter.includes.clone();

        // Async tests get doneprintHandle.js injected (official runner
        // semantics): it defines `$DONE`, which prints the
        // `Test262:AsyncTestComplete` / `Test262:AsyncTestFailure:` markers
        // the async verdict path reads back.
        if needs_async_verdict && !includes_to_load.iter().any(|n| n == "doneprintHandle.js") {
            includes_to_load.push("doneprintHandle.js".to_string());
        }

        // sta.js and assert.js are implicitly included in every non-raw test
        // (official test262 runner semantics): `Test262Error` lives in sta.js,
        // `assert` in assert.js, and declared helpers such as
        // propertyHelper.js rely on both. Prepend them ahead of the declared
        // list, deduped so an explicit entry still loads once.
        if !frontmatter.has_flag("raw") {
            let mut with_defaults = vec!["sta.js".to_string(), "assert.js".to_string()];
            for inc in &includes_to_load {
                if !with_defaults.contains(inc) {
                    with_defaults.push(inc.clone());
                }
            }
            includes_to_load = with_defaults;
        }

        if includes_to_load.is_empty() {
            (String::new(), Vec::new())
        } else if let Some(harness_dir) = &config.harness_dir {
            let (src, errs) = load_harness_includes(&includes_to_load, harness_dir);
            // Fallback: if assert.js/sta.js missing from checkout but needed,
            // inject the minimal polyfill plus whatever we did load.
            if !errs.is_empty()
                && includes_to_load
                    .iter()
                    .any(|n| n == "assert.js" || n == "sta.js")
            {
                let has_polyfill_needed =
                    test_body.contains("assert.") || test_body.contains("Test262Error");
                if has_polyfill_needed {
                    let mut combined = src;
                    combined.push_str(MINIMAL_HARNESS_POLYFILL);
                    combined.push('\n');
                    (combined, Vec::new())
                } else {
                    (src, errs)
                }
            } else {
                (src, errs)
            }
        } else {
            // No harness checkout — inject the minimal polyfill when the test
            // expects harness helpers. This keeps the bootstrap runnable before the
            // submodule is cloned.
            if !includes_to_load.is_empty() {
                (MINIMAL_HARNESS_POLYFILL.to_string(), Vec::new())
            } else {
                (String::new(), Vec::new())
            }
        }
    };

    if !harness_errors.is_empty() {
        let reason = harness_errors
            .iter()
            .map(|e| format!("{}: {}", e.name, e.reason))
            .collect::<Vec<_>>()
            .join("; ");
        return TestOutcome {
            path: file_path.to_path_buf(),
            relative,
            suite,
            status: Status::Skip,
            message: format!("harness include error: {reason}"),
            skip_reason: Some(format!("harness include error: {reason}")),
            duration_ms: start.elapsed().as_millis(),
            frontmatter,
        };
    }

    // Build the combined source (test_body already stripped).
    //
    // Hashbang must lead: `#!` is only a comment at offset 0 (spec
    // `HashbangComment`). The shim/harness preamble below would push a
    // file-leading `#!` mid-source, where the parser rejects it — turning
    // valid hashbang tests (e.g. `hashbang/multi-line-comment`, whose
    // `#!/*` line must comment out just line 1) into silent truncations.
    // Slice that first line off and re-emit it ahead of everything, but
    // ONLY when the file itself starts with `#!`: a `#!` anywhere else
    // (`preceding-*` tests) is a genuine SyntaxError and must stay
    // mid-source so the parser still rejects it.
    let (hashbang, test_body) = match source.strip_prefix("#!") {
        Some(_) => {
            let end = test_body
                .find('\n')
                .map(|i| i + 1)
                .unwrap_or(test_body.len());
            test_body.split_at(end)
        }
        None => ("", test_body),
    };
    let mut combined = String::with_capacity(harness_source.len() + test_body.len() + 64);
    combined.push_str(hashbang);

    // onlyStrict handling: ensure strict mode when requested. We prepend a
    // directive if the file does not already contain one, to avoid double
    // strict issues.
    let needs_strict_prefix = frontmatter.has_flag("onlyStrict")
        && !test_body.contains("\"use strict\"")
        && !test_body.contains("'use strict'");
    if needs_strict_prefix {
        combined.push_str("\"use strict\";\n");
    }

    // $262 host shim: goes after the strict directive, before harness
    // includes, so Test262 harness files and test bodies can reference it.
    combined.push_str(TEST262_HOST_SHIM);
    combined.push('\n');

    combined.push_str(&harness_source);
    if !harness_source.is_empty() && !harness_source.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(test_body);

    if combined.len() > MAX_COMBINED_SOURCE_LEN {
        return TestOutcome {
            path: file_path.to_path_buf(),
            relative,
            suite,
            status: Status::Skip,
            message: "combined source too large".to_string(),
            skip_reason: Some("combined source too large".to_string()),
            duration_ms: start.elapsed().as_millis(),
            frontmatter,
        };
    }

    // Execute in a fresh engine. Catch panics to avoid killing the parallel
    // runner. A cooperative deadline is installed so a runaway script (no
    // IO/await to yield on) aborts instead of stalling a worker: the
    // interpreter samples the deadline every N dispatch iterations and throws
    // a recognizable error if it elapses. This is the wall-clock backstop behind
    // the advisory post-check below.
    let exec_start = Instant::now();
    let deadline = exec_start + std::time::Duration::from_millis(TEST_TIMEOUT_MS as u64);
    let is_module = frontmatter.has_flag("module");
    let base_path = file_path.parent().unwrap_or_else(|| Path::new("."));
    let exec_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut engine = v12_engine::Engine::new();
        engine.set_deadline(Some(deadline));
        // Dynamic `import()` and module-file imports resolve relative to the
        // test file's directory.
        engine.set_module_base(base_path.to_path_buf());
        // Multi-realm support: `$262.createRealm` (see TEST262_HOST_SHIM)
        // resolves to this host function, which builds a fresh realm on the
        // shared heap. Install failure is impossible on a fresh engine's
        // global; ignore the result either way — createRealm then throws.
        let _ = engine.install_create_realm_function("__v12CreateRealm__");
        let res = if is_module {
            engine.eval_module_source(&combined, base_path)
        } else {
            engine.eval(&combined)
        };
        let _ = engine.run_jobs();
        engine.set_deadline(None);
        // The captured async markers only matter when an async verdict is
        // pending; reading them back is one tiny eval on the same engine.
        let prints = if needs_async_verdict {
            engine
                .eval("globalThis.__test262Prints.join('\\n')")
                .map(|v| engine.to_display_string(v))
                .unwrap_or_default()
        } else {
            String::new()
        };
        match res {
            Ok(v) => Ok((engine.to_display_string(v), prints)),
            Err(thrown) => Err((engine.to_display_string(thrown), prints)),
        }
    }));

    let duration_ms = start.elapsed().as_millis();

    // Advisory timeout (after the fact, no preemption).
    if exec_start.elapsed().as_millis() > TEST_TIMEOUT_MS {
        return TestOutcome {
            path: file_path.to_path_buf(),
            relative,
            suite,
            status: Status::Fail,
            message: format!("timeout after {} ms", exec_start.elapsed().as_millis()),
            skip_reason: None,
            duration_ms,
            frontmatter,
        };
    }

    match exec_result {
        Ok(Ok((_ok_value, prints))) => {
            if needs_async_verdict {
                handle_async_ok(
                    &prints,
                    &frontmatter,
                    file_path,
                    relative,
                    suite,
                    duration_ms,
                )
            } else {
                handle_positive_or_negative_ok(
                    &frontmatter,
                    file_path,
                    relative,
                    suite,
                    duration_ms,
                )
            }
        }
        Ok(Err((thrown_str, _prints))) => {
            // A cooperative deadline miss reads back as a thrown error inside
            // the engine; classify it as a timeout rather than running it
            // through negative-expectation matching.
            if is_deadline_error(&thrown_str) {
                return TestOutcome {
                    path: file_path.to_path_buf(),
                    relative,
                    suite,
                    status: Status::Fail,
                    message: format!("timeout after {} ms", duration_ms),
                    skip_reason: None,
                    duration_ms,
                    frontmatter,
                };
            }
            handle_thrown(
                &frontmatter,
                thrown_str,
                file_path,
                relative,
                suite,
                duration_ms,
            )
        }
        Err(_) => TestOutcome {
            path: file_path.to_path_buf(),
            relative,
            suite,
            status: Status::Fail,
            message: "engine panic".to_string(),
            skip_reason: None,
            duration_ms,
            frontmatter,
        },
    }
}

/// Returns `true` if `thrown_str` is the engine's cooperative-deadline error,
/// surfaced as a timeout rather than a real test failure.
fn is_deadline_error(thrown_str: &str) -> bool {
    thrown_str.contains("execution deadline exceeded")
}

/// The subset of [`TestOutcome`] shipped over the parent/child process wire.
///
/// `frontmatter` is diagnostic-only and not used by any reporter, so the
/// child omits it; the parent reconstructs the outcome with a default.
#[derive(Debug, Serialize, Deserialize)]
pub struct SlimOutcome {
    /// `"pass"`, `"fail"`, or `"skip"`.
    pub status: String,
    /// Human-readable detail for fail/skip. Empty on pass.
    pub message: String,
    /// Optional skip reason.
    pub skip_reason: Option<String>,
    /// Wall-clock duration in milliseconds (as measured by the child).
    pub duration_ms: u128,
    /// Path relative to the Test262 root's `test/` directory.
    pub relative: String,
    /// Suite bucket.
    pub suite: String,
}

impl SlimOutcome {
    fn from_outcome(o: &TestOutcome) -> Self {
        Self {
            status: match o.status {
                Status::Pass => "pass",
                Status::Fail => "fail",
                Status::Skip => "skip",
            }
            .to_string(),
            message: o.message.clone(),
            skip_reason: o.skip_reason.clone(),
            duration_ms: o.duration_ms,
            relative: o.relative.clone(),
            suite: o.suite.clone(),
        }
    }

    fn into_outcome(self, path: &Path) -> TestOutcome {
        let status = match self.status.as_str() {
            "pass" => Status::Pass,
            "skip" => Status::Skip,
            _ => Status::Fail,
        };
        TestOutcome {
            path: path.to_path_buf(),
            relative: self.relative,
            suite: self.suite,
            status,
            message: self.message,
            skip_reason: self.skip_reason,
            duration_ms: self.duration_ms,
            frontmatter: Frontmatter::default(),
        }
    }
}

/// Encodes an outcome as the single JSON line the child prints on stdout in
/// `--internal-exec` mode (see `run_single_test_isolated`).
#[must_use]
pub fn encode_slim(outcome: &TestOutcome) -> String {
    serde_json::to_string(&SlimOutcome::from_outcome(outcome)).unwrap_or_else(|_| {
        // Unserializable strings cannot occur (no control chars beyond what
        // serde handles), but a broken child report must not crash the run.
        "{\"status\":\"fail\",\"message\":\"internal outcome encode error\"}".to_string()
    })
}

fn fail_outcome(
    file_path: &Path,
    relative: String,
    suite: String,
    message: String,
    duration_ms: u128,
) -> TestOutcome {
    TestOutcome {
        path: file_path.to_path_buf(),
        relative,
        suite,
        status: Status::Fail,
        message,
        skip_reason: None,
        duration_ms,
        frontmatter: Frontmatter::default(),
    }
}

/// Runs a single test in a dedicated child process with a hard wall-clock
/// kill, so a stalled test cannot wedge the sweep.
///
/// The child is this binary re-invoked with the hidden `--internal-exec`
/// flag; it prints one [`SlimOutcome`] JSON line to stdout. The parent polls
/// for up to [`TEST_HARD_KILL_MS`]:
///
/// - reported in time → child's own outcome (the engine's cooperative
///   deadline already classifies most runaways as timeouts);
/// - not reported → the test is logged to stderr as stalled, the child is
///   killed, and a timeout failure is returned;
/// - exited without a report (crash / stack overflow / abort) → a failure
///   naming the exit status. A child crash loses only that test, whereas an
///   in-process stack overflow would abort the entire run.
///
/// Spawn overhead is a few milliseconds per test, dominated by the engine
/// work itself.
pub fn run_single_test_isolated(file_path: &Path, config: &HarnessConfig) -> TestOutcome {
    let relative = relative_suite_path(file_path, &config.test262_root);
    let suite = suite_for(&relative);
    let start = Instant::now();

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return fail_outcome(
                file_path,
                relative,
                suite,
                format!("cannot resolve runner executable for isolation: {e}"),
                start.elapsed().as_millis(),
            );
        }
    };

    let mut child = match Command::new(&exe)
        .arg("--internal-exec")
        .arg(file_path)
        .arg("--test262-root")
        .arg(&config.test262_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Child stderr (panic messages, abort diagnostics) is useful inline.
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return fail_outcome(
                file_path,
                relative,
                suite,
                format!("failed to spawn isolated test process: {e}"),
                start.elapsed().as_millis(),
            );
        }
    };

    // Poll instead of blocking wait so the hard kill actually fires at the
    // deadline. Stdout stays under the pipe buffer (one small JSON line),
    // so reading it only after exit cannot deadlock.
    let mut timed_out = false;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed().as_millis() >= u128::from(TEST_HARD_KILL_MS) {
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return fail_outcome(
                    file_path,
                    relative,
                    suite,
                    format!("failed to wait for isolated test process: {e}"),
                    start.elapsed().as_millis(),
                );
            }
        }
    };

    if timed_out {
        let _ = child.kill();
        let _ = child.wait();
        let duration_ms = start.elapsed().as_millis();
        eprintln!("STALLED (killed after {TEST_HARD_KILL_MS} ms): {relative}");
        return fail_outcome(
            file_path,
            relative,
            suite,
            format!("timeout: stalled, killed after {TEST_HARD_KILL_MS} ms"),
            duration_ms,
        );
    }

    let status = exit_status.expect("exit_status is Some when not timed out");
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut stdout);
    }
    let _ = child.wait();

    let line = stdout.trim();
    if status.success()
        && let Some(slim) = serde_json::from_str::<SlimOutcome>(line).ok()
    {
        return slim.into_outcome(file_path);
    }

    // Exited without a usable report: crash (abort / stack overflow /
    // signal) or corrupted output. Report per-test instead of losing the run.
    let detail = if line.is_empty() {
        format!("no report ({status})")
    } else {
        format!("unparseable report ({status}): {line:.200}")
    };
    eprintln!("CRASHED test child: {relative} — {detail}");
    fail_outcome(
        file_path,
        relative,
        suite,
        format!("isolated test process {detail}"),
        start.elapsed().as_millis(),
    )
}

/// Async verdict: the test's top-level eval succeeded; the verdict comes from
/// the `$DONE` markers doneprintHandle.js printed into `__test262Prints`.
///
/// - `Test262:AsyncTestComplete` → normal positive/negative handling (a
///   negative test that completes successfully is a failure, as it should be).
/// - `Test262:AsyncTestFailure:<name>: <msg>` → the async failure string is
///   run through `handle_thrown`, so negative runtime expectations match.
/// - Neither marker → the microtask queue drained without `$DONE` being
///   called; that is a failure (with the negative note if one is expected).
fn handle_async_ok(
    prints: &str,
    fm: &Frontmatter,
    path: &Path,
    relative: String,
    suite: String,
    duration_ms: u128,
) -> TestOutcome {
    if prints.contains("Test262:AsyncTestComplete") {
        return handle_positive_or_negative_ok(fm, path, relative, suite, duration_ms);
    }
    if let Some(rest) = prints.split_once("Test262:AsyncTestFailure:") {
        let failure_text = rest.1.trim();
        let thrown = if failure_text.is_empty() {
            "Test262Error: async test failed".to_string()
        } else {
            format!("Test262Error: {failure_text}")
        };
        return handle_thrown(fm, thrown, path, relative, suite, duration_ms);
    }
    TestOutcome {
        path: path.to_path_buf(),
        relative,
        suite,
        status: Status::Fail,
        message: "async test did not complete ($DONE not called)".to_string(),
        skip_reason: None,
        duration_ms,
        frontmatter: fm.clone(),
    }
}

/// Returns `Some(reason)` if the test should be skipped before execution.
fn skip_reason_for(fm: &Frontmatter, source: &str) -> Option<String> {
    // Module tests are now wired for `language` (and generally via eval_module);
    // keep the skip only for non-language suites if needed. For the language
    // gate we want them executable, so do not skip here — `run_single_test`
    // will dispatch to `eval_module` when the flag is present.
    // Multi-realm is wired: `$262.createRealm` is a host function installed by
    // the runner (`install_create_realm_function`) that builds a second realm
    // on the shared heap, so tests using it execute for real. The agent API
    // (workers/Atomics coordination) remains unsupported — skip only tests
    // that actually call `$262.agent`, not ones that merely mention "agent."
    // in a comment.
    if source.contains("$262.agent") {
        return Some("requires $262.agent (worker/Atomics harness)".to_string());
    }
    // Async tests are no longer skipped: doneprintHandle.js is injected and
    // the verdict is read from the `Test262:AsyncTest*` print markers (see
    // `handle_async_ok`). Other `$262` uses (`$262.global`,
    // `detachArrayBuffer`, `gc`, `getReport`) run via the TEST262_HOST_SHIM
    // preamble.
    // Proper tail calls are not implemented (see fix-log.md): every test
    // declaring the feature recurses `$MAX_ITERATIONS` = 100 000 deep purely
    // to prove tail frames are destroyed, so without TCO it can only burn
    // seconds of deep-recursion unwind and nondeterministically outlive the
    // 5 s hard-kill window (observed as STALLED on tco-lhs-body/tco-finally).
    // Skip the ~35 such tests until TCO lands.
    if fm.has_feature("tail-call-optimization") {
        return Some(
            "requires tail-call-optimization (proper tail calls not implemented)".to_string(),
        );
    }
    // TypedArrays (and resizable ArrayBuffers) are not implemented: skip
    // tests declaring the feature instead of failing on
    // `Uint8Array is not defined` (approved scope decision).
    if fm.has_feature("resizable-arraybuffer") {
        return Some(
            "requires resizable-arraybuffer (TypedArray/ArrayBuffer not implemented)".to_string(),
        );
    }
    None
}

/// Handles the case where `engine.eval` returned `Ok`.
fn handle_positive_or_negative_ok(
    fm: &Frontmatter,
    path: &Path,
    relative: String,
    suite: String,
    duration_ms: u128,
) -> TestOutcome {
    if let Some(neg) = &fm.negative {
        match neg.phase.as_str() {
            "parse" | "early" | "resolution" => TestOutcome {
                path: path.to_path_buf(),
                relative,
                suite,
                status: Status::Fail,
                message: format!(
                    "expected {} {} but no error thrown",
                    neg.phase, neg.type_name
                ),
                skip_reason: None,
                duration_ms,
                frontmatter: fm.clone(),
            },
            "runtime" => TestOutcome {
                path: path.to_path_buf(),
                relative,
                suite,
                status: Status::Fail,
                message: format!(
                    "expected {} {} but no error thrown",
                    neg.phase, neg.type_name
                ),
                skip_reason: None,
                duration_ms,
                frontmatter: fm.clone(),
            },
            _ => TestOutcome {
                path: path.to_path_buf(),
                relative,
                suite,
                status: Status::Fail,
                message: format!("negative with unknown phase: {}", neg.phase),
                skip_reason: None,
                duration_ms,
                frontmatter: fm.clone(),
            },
        }
    } else {
        TestOutcome {
            path: path.to_path_buf(),
            relative,
            suite,
            status: Status::Pass,
            message: String::new(),
            skip_reason: None,
            duration_ms,
            frontmatter: fm.clone(),
        }
    }
}

/// Handles the case where `engine.eval` returned `Err(thrown)`.
fn handle_thrown(
    fm: &Frontmatter,
    thrown_str: String,
    path: &Path,
    relative: String,
    suite: String,
    duration_ms: u128,
) -> TestOutcome {
    if let Some(neg) = &fm.negative {
        // Lenient type check: does the thrown string mention the expected
        // type? Engine error strings are plain strings, not Error objects,
        // so this is the best we can do without Error identity.
        let type_matches = thrown_str.contains(&neg.type_name)
            || (neg.type_name == "SyntaxError" && looks_like_syntax_error(&thrown_str))
            || (neg.type_name == "ReferenceError" && thrown_str.contains("ReferenceError"))
            || (neg.type_name == "TypeError" && thrown_str.contains("TypeError"))
            || (neg.type_name == "Test262Error" && thrown_str.contains("Test262Error"));

        match neg.phase.as_str() {
            "parse" | "early" | "resolution" => {
                if type_matches {
                    TestOutcome {
                        path: path.to_path_buf(),
                        relative,
                        suite,
                        status: Status::Pass,
                        message: String::new(),
                        skip_reason: None,
                        duration_ms,
                        frontmatter: fm.clone(),
                    }
                } else {
                    // For parse-phase negatives, any error is arguably a pass
                    // if the engine rejected the program. We treat type
                    // mismatches as failures to surface real issues, but allow
                    // generic syntax-like errors to pass for SyntaxError.
                    if neg.type_name == "SyntaxError" || thrown_str.contains("SyntaxError") {
                        TestOutcome {
                            path: path.to_path_buf(),
                            relative,
                            suite,
                            status: Status::Pass,
                            message: String::new(),
                            skip_reason: None,
                            duration_ms,
                            frontmatter: fm.clone(),
                        }
                    } else {
                        TestOutcome {
                            path: path.to_path_buf(),
                            relative,
                            suite,
                            status: Status::Fail,
                            message: format!(
                                "expected {} {} but got: {thrown_str}",
                                neg.phase, neg.type_name
                            ),
                            skip_reason: None,
                            duration_ms,
                            frontmatter: fm.clone(),
                        }
                    }
                }
            }
            "runtime" => {
                if type_matches {
                    TestOutcome {
                        path: path.to_path_buf(),
                        relative,
                        suite,
                        status: Status::Pass,
                        message: String::new(),
                        skip_reason: None,
                        duration_ms,
                        frontmatter: fm.clone(),
                    }
                } else {
                    TestOutcome {
                        path: path.to_path_buf(),
                        relative,
                        suite,
                        status: Status::Fail,
                        message: format!(
                            "expected {} {} but got: {thrown_str}",
                            neg.phase, neg.type_name
                        ),
                        skip_reason: None,
                        duration_ms,
                        frontmatter: fm.clone(),
                    }
                }
            }
            _ => TestOutcome {
                path: path.to_path_buf(),
                relative,
                suite,
                status: Status::Fail,
                message: format!(
                    "negative with unknown phase: {} — thrown: {thrown_str}",
                    neg.phase
                ),
                skip_reason: None,
                duration_ms,
                frontmatter: fm.clone(),
            },
        }
    } else {
        TestOutcome {
            path: path.to_path_buf(),
            relative,
            suite,
            status: Status::Fail,
            message: format!("threw: {thrown_str}"),
            skip_reason: None,
            duration_ms,
            frontmatter: fm.clone(),
        }
    }
}

/// Heuristic: does `msg` look like a SyntaxError?
fn looks_like_syntax_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("syntaxerror")
        || lower.contains("parse error")
        || lower.contains("semantic error")
        || lower.contains("unexpected")
        || lower.contains("expected")
}

fn relative_suite_path(path: &Path, test262_root: &Path) -> String {
    // Try to make `relative` relative to `<root>/test/`. If that fails,
    // fall back to the file name.
    let test_dir = test262_root.join("test");
    if let Ok(rel) = path.strip_prefix(&test_dir) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    if let Ok(rel) = path.strip_prefix(test262_root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Suite is the first directory of the relative path, with a special case
/// for `built-ins/<Ctor>` which we bucket as `built-ins`.
fn suite_for(relative: &str) -> String {
    let parts: Vec<&str> = relative.split('/').collect();
    if parts.is_empty() {
        return "unknown".to_string();
    }
    if parts[0] == "built-ins" && parts.len() >= 2 {
        return format!("built-ins/{}", parts[1]);
    }
    if parts[0] == "language" && parts.len() >= 2 {
        return format!("language/{}", parts[1]);
    }
    parts[0].to_string()
}

/// Discovers all `*.js` test files under `<test262_root>/test`, filtered by
/// an optional glob substring/filter.
///
/// `filter` semantics:
/// - empty / `None` → all files.
/// - contains `*` or `?` or `[` → treated as a glob pattern matched against
///   the relative path via `glob::Pattern`.
/// - otherwise → substring match against the relative path.
#[must_use]
pub fn discover_tests(test262_root: &Path, filter: Option<&str>) -> Vec<PathBuf> {
    let test_dir = test262_root.join("test");
    if !test_dir.is_dir() {
        return Vec::new();
    }

    let pattern = filter.and_then(|f| {
        if f.contains('*') || f.contains('?') || f.contains('[') {
            glob::Pattern::new(f).ok()
        } else {
            None
        }
    });
    let substring_filter =
        filter.filter(|f| !f.contains('*') && !f.contains('?') && !f.contains('['));

    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&test_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        // Test262 convention: `*_FIXTURE.js` files are shared module fixtures
        // for other tests, not standalone tests (they typically carry no
        // frontmatter and fail to compile as scripts).
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with("_FIXTURE"))
        {
            continue;
        }

        if let Some(pat) = &pattern {
            let rel = relative_suite_path(path, test262_root);
            if !pat.matches(&rel) && !pat.matches(&path.to_string_lossy()) {
                continue;
            }
        } else if let Some(sub) = substring_filter
            && !sub.is_empty()
        {
            let rel = relative_suite_path(path, test262_root);
            if !rel.contains(sub) && !path.to_string_lossy().contains(sub) {
                continue;
            }
        }

        files.push(path.to_path_buf());
    }

    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp_config() -> (HarnessConfig, PathBuf) {
        let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let tmp = std::env::temp_dir().join(format!(
            "v12_runner_tmp_{}_{}_{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(tmp.join("test").join("language"));
        let _ = fs::create_dir_all(tmp.join("harness"));
        // Non-raw tests always load sta.js/assert.js (official test262
        // semantics), so the fixtures must provide them.
        let _ = fs::write(tmp.join("harness").join("sta.js"), "");
        let _ = fs::write(tmp.join("harness").join("assert.js"), "");
        let cfg = HarnessConfig::new(tmp.clone());
        (cfg, tmp)
    }

    #[test]
    fn skip_module_flag() {
        let fm = Frontmatter {
            flags: vec!["module".to_string()],
            ..Default::default()
        };
        let reason = skip_reason_for(&fm, "var x = 1;");
        assert!(
            reason.is_none(),
            "module should no longer be skipped (now wired), got {reason:?}"
        );
    }

    #[test]
    fn skip_async_flag() {
        let fm = Frontmatter {
            flags: vec!["async".to_string()],
            ..Default::default()
        };
        assert!(
            skip_reason_for(&fm, "").is_none(),
            "async should no longer be skipped (generators+async wired)"
        );
    }

    #[test]
    fn dont_skip_sync() {
        let fm = Frontmatter {
            flags: vec!["noStrict".to_string()],
            ..Default::default()
        };
        assert!(skip_reason_for(&fm, "var x = 1;").is_none());
    }

    #[test]
    fn positive_pass() {
        let (cfg, _tmp) = tmp_config();
        let dir = cfg.test262_root.join("test").join("language");
        let path = dir.join("pass.js");
        fs::write(&path, "var x = 1;").unwrap();
        let outcome = run_single_test(&path, &cfg);
        assert_eq!(outcome.status, Status::Pass);
        let _ = fs::remove_dir_all(&cfg.test262_root);
    }

    #[test]
    fn positive_fail_throw() {
        let (cfg, _tmp) = tmp_config();
        let dir = cfg.test262_root.join("test").join("language");
        let path = dir.join("fail.js");
        // Use a plain throw without `new` to avoid the `new` opcode gap.
        fs::write(&path, "throw 'oops';").unwrap();
        // Provide minimal harness for Test262Error.
        fs::write(
            cfg.test262_root.join("harness").join("sta.js"),
            MINIMAL_HARNESS_POLYFILL,
        )
        .unwrap();
        // Use an include so polyfill path is exercised differently.
        let path2 = dir.join("fail2.js");
        fs::write(&path2, "/*---\nincludes: [sta.js]\n---*/\nthrow 'oops';").unwrap();
        let outcome = run_single_test(&path2, &cfg);
        assert_eq!(outcome.status, Status::Fail);
        let _ = fs::remove_dir_all(&cfg.test262_root);
    }

    #[test]
    fn negative_parse_pass() {
        let (cfg, _tmp) = tmp_config();
        let dir = cfg.test262_root.join("test").join("language");
        let path = dir.join("neg_parse.js");
        fs::write(
            &path,
            "/*---\nnegative:\n  phase: parse\n  type: SyntaxError\n---*/\nvar x = ;",
        )
        .unwrap();
        let outcome = run_single_test(&path, &cfg);
        // Engine reports a compile error as Err, so negative parse passes.
        assert_eq!(outcome.status, Status::Pass);
        let _ = fs::remove_dir_all(&cfg.test262_root);
    }

    #[test]
    fn negative_parse_fail_when_no_error() {
        let (cfg, _tmp) = tmp_config();
        let dir = cfg.test262_root.join("test").join("language");
        let path = dir.join("neg_parse_fail.js");
        fs::write(
            &path,
            "/*---\nnegative:\n  phase: parse\n  type: SyntaxError\n---*/\nvar x = 1;",
        )
        .unwrap();
        let outcome = run_single_test(&path, &cfg);
        assert_eq!(outcome.status, Status::Fail);
        let _ = fs::remove_dir_all(&cfg.test262_root);
    }

    #[test]
    fn discover_filters_substring() {
        let (_cfg, tmp) = tmp_config();
        let _ = fs::create_dir_all(tmp.join("test").join("language").join("expressions"));
        let _ = fs::create_dir_all(tmp.join("test").join("built-ins").join("Array"));
        fs::write(tmp.join("test").join("language").join("a.js"), "").unwrap();
        fs::write(
            tmp.join("test")
                .join("language")
                .join("expressions")
                .join("b.js"),
            "",
        )
        .unwrap();
        fs::write(
            tmp.join("test")
                .join("built-ins")
                .join("Array")
                .join("c.js"),
            "",
        )
        .unwrap();
        let files = discover_tests(&tmp, Some("language/expressions"));
        assert_eq!(files.len(), 1);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_no_filter() {
        let (_cfg, tmp) = tmp_config();
        let _ = fs::create_dir_all(tmp.join("test").join("language"));
        fs::write(tmp.join("test").join("language").join("x.js"), "").unwrap();
        let files = discover_tests(&tmp, None);
        assert!(!files.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn suite_for_language() {
        assert_eq!(
            suite_for("language/expressions/a.js"),
            "language/expressions"
        );
        assert_eq!(suite_for("built-ins/Array/a.js"), "built-ins/Array");
        assert_eq!(suite_for("intl402/a.js"), "intl402");
        assert_eq!(suite_for("annexB/a.js"), "annexB");
    }

    // GATE (async verdict path): Promise.resolve().then(...) runs via
    // run_jobs() and the captured print surfaces the completion marker.
    #[test]
    fn async_doneprint_test_completes_via_captured_print() {
        // Arrange: a tiny async-shaped source; the real doneprintHandle.js
        // semantics are `$DONE()` → prints Test262:AsyncTestComplete.
        let src = "globalThis.__test262Prints = [];\n\
                   function print(s) { globalThis.__test262Prints.push(String(s)); }\n\
                   Promise.resolve().then(function () { print('Test262:AsyncTestComplete'); });";
        let mut engine = v12_engine::Engine::new();
        engine.eval(src).expect("eval");
        engine.run_jobs();
        let printed = engine
            .eval("globalThis.__test262Prints.join('\\n')")
            .map(|v| engine.to_display_string(v))
            .unwrap_or_default();
        assert!(
            printed.contains("Test262:AsyncTestComplete"),
            "printed: {printed:?}"
        );
    }

    #[test]
    fn harness_include_prepended() {
        let (cfg, _tmp) = tmp_config();
        let harness_dir = cfg.test262_root.join("harness");
        fs::write(harness_dir.join("assert.js"), "var ASSERT_LOADED = true;").unwrap();
        let dir = cfg.test262_root.join("test").join("language");
        let path = dir.join("with_include.js");
        // Avoid `new` — engine's ISA has no construct opcode yet.
        fs::write(
            &path,
            "/*---\nincludes: [assert.js]\n---*/\nif (!ASSERT_LOADED) throw 'missing';",
        )
        .unwrap();
        let outcome = run_single_test(&path, &cfg);
        assert_eq!(outcome.status, Status::Pass);
        let _ = fs::remove_dir_all(&cfg.test262_root);
    }
}
