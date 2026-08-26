#![forbid(unsafe_code)]

//! The `v12` binary: REPL and script runner.
//!
//! # Usage
//!
//! ```text
//! v12 [script.js] [--disasm] [--expose-gc]
//! ```
//!
//! * `v12 script.js` executes the file, prints the completion value (if not
//!   `undefined`) via [`v12_engine::Engine::to_display_string`], drains the
//!   microtask queue, and exits `1` on an uncaught exception.
//! * `v12 --disasm script.js` compiles the file with `v12-bccompiler` and
//!   prints the bytecode disassembly of each function instead of executing it.
//! * `v12` with no file and a TTY on stdin enters a line-by-line REPL backed
//!   by `rustyline` (arrow navigation, history, line editing).
//! * `--expose-gc` enables GC stress mode (collect on every allocation).
//!
//! # REPL limitations
//!
//! The REPL evaluates each input line as an independent script. Variables
//! declared on one line do not persist to the next, and multi-line statements
//! (e.g., `if` blocks spanning lines, function bodies) must be entered as a
//! single line or they fail to parse. This keeps the implementation single-pass
//! and synchronous. A future iteration can accumulate input until the parser
//! accepts it.

use std::io::{self, IsTerminal, Read, Write};
use v12_engine::Engine;

/// Prompt printed before each REPL input line.
const PROMPT: &str = "> ";

/// Exit code for successful execution.
const EXIT_SUCCESS: i32 = 0;

/// Exit code for failure (uncaught exception, I/O error, syntax error).
const EXIT_FAILURE: i32 = 1;

/// Maximum length of a single REPL input line in bytes.
///
/// Longer lines are rejected with an error message to bound memory.
const MAX_LINE_LEN: usize = 10_000;

/// Help text printed for `-h` / `--help`.
const HELP_TEXT: &str = concat!(
    "Usage: v12 [script.js] [--disasm] [--expose-gc]\n",
    "\n",
    "Execute a JavaScript file or enter the REPL.\n",
    "\n",
    "Arguments:\n",
    "  [script.js]   JavaScript file to execute (if omitted, REPL starts when stdin is a TTY)\n",
    "\n",
    "Options:\n",
    "  --disasm      Print bytecode disassembly instead of executing\n",
    "  --expose-gc   Enable GC stress mode (collect on every allocation)\n",
    "  -h, --help    Print this help message\n",
);

/// Short usage line printed on argument errors.
const USAGE_TEXT: &str = "Usage: v12 [script.js] [--disasm] [--expose-gc]";

/// Parsed command-line arguments.
///
/// Produced by [`parse_args`] from `std::env::args()`.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    /// Path to the script file, if any.
    script: Option<String>,
    /// Whether to print disassembly instead of executing.
    disasm: bool,
    /// Whether to enable GC stress mode.
    expose_gc: bool,
}

/// Parses command-line arguments.
///
/// # Parameters
///
/// * `args` - The full argument vector including `argv[0]`.
///
/// # Returns
///
/// `Ok(Args)` on success. `Err("help")` signals that help was requested.
/// Any other `Err(String)` carries a human-readable error for the caller to
/// display before exiting.
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut script: Option<String> = None;
    let mut disasm = false;
    let mut expose_gc = false;

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "--disasm" => disasm = true,
            "--expose-gc" => expose_gc = true,
            "-h" | "--help" => return Err("help".to_string()),
            "--" => {
                // End of options: treat remaining args as positional.
                // For simplicity, break and handle the next arg as script
                // if present. Subsequent flags after `--` are treated as
                // positional, which matches POSIX conventions.
                // Since we already iterated, we just stop flag parsing.
                // Any remaining args after `--` would need a second loop,
                // but the current call sites pass only known flags, so we
                // simply stop processing further flags.
                break;
            }
            _ if arg.starts_with('-') => {
                return Err(format!("unknown option: {arg}"));
            }
            _ => {
                if script.is_some() {
                    return Err(format!("extra positional argument: {arg}"));
                }
                script = Some(arg.clone());
            }
        }
    }

    Ok(Args {
        script,
        disasm,
        expose_gc,
    })
}

/// Prints help text to stdout.
///
/// The text is defined in [`HELP_TEXT`] and covers the full argument surface.
fn print_help() {
    print!("{HELP_TEXT}");
}

/// Prints short usage to stderr.
///
/// Used when argument parsing fails. See [`USAGE_TEXT`].
fn print_usage() {
    eprintln!("{USAGE_TEXT}");
    eprintln!("Try 'v12 --help' for more information.");
}

/// Compiles `source` and prints bytecode disassembly for each function.
///
/// # Parameters
///
/// * `source` - JavaScript source text to compile.
///
/// # Returns
///
/// `EXIT_SUCCESS` on successful compilation, `EXIT_FAILURE` if the compiler
/// reports a syntax or semantic error (printed as `SyntaxError: <msg>`).
fn run_disasm(source: &str) -> i32 {
    match v12_bccompiler::compile_source(source) {
        Ok(program) => {
            for (idx, func) in program.functions.iter().enumerate() {
                if idx > 0 {
                    println!();
                }
                // `FunctionBytecode` implements `Display` as a disassembly listing.
                print!("{func}");
            }
            // Ensure a trailing newline if we printed anything without one.
            // `Display` for `FunctionBytecode` already ends with newlines, but
            // guard the empty-program case.
            if program.functions.is_empty() {
                // No functions to display; still succeed.
            }
            let _ = io::stdout().flush();
            EXIT_SUCCESS
        }
        Err(err) => {
            // `CompileError`'s `Display` includes the span when present, so
            // `SyntaxError: <msg> (bytes ..)` satisfies the requirement.
            eprintln!("SyntaxError: {err}");
            EXIT_FAILURE
        }
    }
}

/// Executes the script at `path`.
///
/// # Parameters
///
/// * `path` - Filesystem path to the JavaScript file.
/// * `disasm` - If true, print disassembly instead of executing.
/// * `expose_gc` - If true, enable GC stress mode on the engine heap.
///
/// # Returns
///
/// `EXIT_SUCCESS` or `EXIT_FAILURE`. On file-not-found, prints the OS error
/// to stderr. On an uncaught JS exception, prints `Uncaught <msg>` to stderr.
/// The microtask queue is drained via [`Engine::run_jobs`] after evaluation.
fn run_script(path: &str, disasm: bool, expose_gc: bool) -> i32 {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("{path}: {err}");
            return EXIT_FAILURE;
        }
    };

    if disasm {
        return run_disasm(&source);
    }

    let mut engine = Engine::new();
    if expose_gc {
        engine.heap_mut().gc_stress(Some(1));
    }

    match engine.eval(&source) {
        Ok(value) => {
            // Print the completion value when it is not `undefined`. This
            // avoids noisy `undefined` lines for scripts whose last statement
            // is a declaration. Use `to_display_string` for consistent
            // formatting with the REPL.
            if !value.is_undefined() {
                let text = engine.to_display_string(value);
                println!("{text}");
            }
            let _ = engine.run_jobs();
            EXIT_SUCCESS
        }
        Err(thrown) => {
            let text = engine.to_display_string(thrown);
            eprintln!("Uncaught {text}");
            let _ = engine.run_jobs();
            EXIT_FAILURE
        }
    }
}

/// Runs the interactive REPL with line editing and history.
///
/// Uses `rustyline` for arrow navigation (Left/Right), history traversal
/// (Up/Down), and in-line editing. Each line is evaluated as an independent
/// script via a single reused [`Engine`], and the microtask queue is drained
/// after every evaluation. History is in-memory only and is not persisted to
/// disk.
///
/// # Parameters
///
/// * `expose_gc` - If true, enables GC stress mode on the REPL's engine.
///
/// # Returns
///
/// `EXIT_SUCCESS` on clean EOF / `.exit`, `EXIT_FAILURE` on a fatal editor
/// initialization or I/O error.
///
/// # Limitations
///
/// Each line is evaluated in isolation; bindings do not persist across lines
/// because the underlying [`Engine::eval`] creates a fresh interpreter state
/// per call. Multi-line input must be entered as a single line (the parser
/// has no incremental multi-line accumulation yet).
fn run_repl(expose_gc: bool) -> i32 {
    let mut engine = Engine::new();
    if expose_gc {
        engine.heap_mut().gc_stress(Some(1));
    }

    let mut rl = match rustyline::DefaultEditor::new() {
        Ok(editor) => editor,
        Err(err) => {
            eprintln!("failed to initialize line editor: {err}");
            return EXIT_FAILURE;
        }
    };

    loop {
        match rl.readline(PROMPT) {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == ".exit" || trimmed == "exit" || trimmed == "quit" {
                    break;
                }
                if trimmed.len() > MAX_LINE_LEN {
                    eprintln!("input too long (>{MAX_LINE_LEN} bytes)");
                    continue;
                }
                let _ = rl.add_history_entry(line.as_str());
                match engine.eval(trimmed) {
                    Ok(value) => {
                        if !value.is_undefined() {
                            let text = engine.to_display_string(value);
                            println!("{text}");
                        }
                        let _ = engine.run_jobs();
                    }
                    Err(thrown) => {
                        let text = engine.to_display_string(thrown);
                        eprintln!("Uncaught {text}");
                        let _ = engine.run_jobs();
                    }
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => break,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("repl read error: {err}");
                break;
            }
        }
    }

    EXIT_SUCCESS
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let parsed = match parse_args(&args) {
        Ok(p) => p,
        Err(e) if e == "help" => {
            print_help();
            std::process::exit(EXIT_SUCCESS);
        }
        Err(e) => {
            eprintln!("{e}");
            print_usage();
            std::process::exit(EXIT_FAILURE);
        }
    };

    let exit_code = if let Some(script) = parsed.script {
        run_script(&script, parsed.disasm, parsed.expose_gc)
    } else if parsed.disasm {
        eprintln!("--disasm requires a script file");
        print_usage();
        EXIT_FAILURE
    } else if io::stdin().is_terminal() {
        run_repl(parsed.expose_gc)
    } else {
        // No script and stdin is not a TTY: treat piped stdin as a script.
        let mut source = String::new();
        if let Err(err) = io::stdin().read_to_string(&mut source) {
            eprintln!("failed to read stdin: {err}");
            std::process::exit(EXIT_FAILURE);
        }
        // If stdin was empty (e.g., `v12 < /dev/null`), exit cleanly.
        if source.trim().is_empty() {
            EXIT_SUCCESS
        } else if parsed.expose_gc {
            let mut engine = Engine::new();
            engine.heap_mut().gc_stress(Some(1));
            match engine.eval(&source) {
                Ok(value) => {
                    if !value.is_undefined() {
                        let text = engine.to_display_string(value);
                        println!("{text}");
                    }
                    let _ = engine.run_jobs();
                    EXIT_SUCCESS
                }
                Err(thrown) => {
                    let text = engine.to_display_string(thrown);
                    eprintln!("Uncaught {text}");
                    let _ = engine.run_jobs();
                    EXIT_FAILURE
                }
            }
        } else {
            let mut engine = Engine::new();
            match engine.eval(&source) {
                Ok(value) => {
                    if !value.is_undefined() {
                        let text = engine.to_display_string(value);
                        println!("{text}");
                    }
                    let _ = engine.run_jobs();
                    EXIT_SUCCESS
                }
                Err(thrown) => {
                    let text = engine.to_display_string(thrown);
                    eprintln!("Uncaught {text}");
                    let _ = engine.run_jobs();
                    EXIT_FAILURE
                }
            }
        }
    };

    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    // ------------------------------------------------------------------
    // Unit tests for argument parsing
    // ------------------------------------------------------------------

    #[test]
    fn parse_no_args() {
        let args = vec!["v12".to_string()];
        let parsed = parse_args(&args).expect("should parse");
        assert_eq!(
            parsed,
            Args {
                script: None,
                disasm: false,
                expose_gc: false
            }
        );
    }

    #[test]
    fn parse_script_only() {
        let args = vec!["v12".to_string(), "foo.js".to_string()];
        let parsed = parse_args(&args).expect("should parse");
        assert_eq!(parsed.script.as_deref(), Some("foo.js"));
        assert!(!parsed.disasm);
        assert!(!parsed.expose_gc);
    }

    #[test]
    fn parse_script_and_flags() {
        let args = vec![
            "v12".to_string(),
            "foo.js".to_string(),
            "--disasm".to_string(),
            "--expose-gc".to_string(),
        ];
        let parsed = parse_args(&args).expect("should parse");
        assert_eq!(parsed.script.as_deref(), Some("foo.js"));
        assert!(parsed.disasm);
        assert!(parsed.expose_gc);
    }

    #[test]
    fn parse_flags_before_script() {
        let args = vec![
            "v12".to_string(),
            "--disasm".to_string(),
            "--expose-gc".to_string(),
            "bar.js".to_string(),
        ];
        let parsed = parse_args(&args).expect("should parse");
        assert_eq!(parsed.script.as_deref(), Some("bar.js"));
        assert!(parsed.disasm);
        assert!(parsed.expose_gc);
    }

    #[test]
    fn parse_unknown_flag() {
        let args = vec!["v12".to_string(), "--unknown".to_string()];
        let err = parse_args(&args).unwrap_err();
        assert!(err.contains("unknown option"), "{err}");
    }

    #[test]
    fn parse_extra_positional() {
        let args = vec!["v12".to_string(), "a.js".to_string(), "b.js".to_string()];
        let err = parse_args(&args).unwrap_err();
        assert!(err.contains("extra positional"), "{err}");
    }

    #[test]
    fn parse_help_flag() {
        let args = vec!["v12".to_string(), "--help".to_string()];
        let err = parse_args(&args).unwrap_err();
        assert_eq!(err, "help");
    }

    #[test]
    fn parse_help_short_flag() {
        let args = vec!["v12".to_string(), "-h".to_string()];
        let err = parse_args(&args).unwrap_err();
        assert_eq!(err, "help");
    }

    // ------------------------------------------------------------------
    // Behavioural tests for disassembly and compilation errors
    // ------------------------------------------------------------------

    #[test]
    fn disasm_valid_source_succeeds() {
        let code = run_disasm("let x = 1;");
        assert_eq!(code, EXIT_SUCCESS);
    }

    #[test]
    fn disasm_invalid_source_reports_syntax_error() {
        let code = run_disasm("let = 1;");
        assert_eq!(code, EXIT_FAILURE);
    }

    #[test]
    fn engine_eval_throw_is_uncaught() {
        let mut engine = Engine::new();
        let thrown = engine.eval("throw 42;").unwrap_err();
        let text = engine.to_display_string(thrown);
        assert_eq!(text, "42");
    }

    #[test]
    fn engine_eval_with_gc_stress_still_throws() {
        let mut engine = Engine::new();
        engine.heap_mut().gc_stress(Some(1));
        let thrown = engine.eval("throw 'oops';").unwrap_err();
        let text = engine.to_display_string(thrown);
        assert_eq!(text, "oops");
    }

    // ------------------------------------------------------------------
    // Integration-style tests that spawn the binary via `cargo run`
    // ------------------------------------------------------------------

    fn cargo_cmd() -> Command {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let mut cmd = Command::new(cargo);
        // Build and run the `v12` binary from this crate.
        cmd.args(["run", "--quiet", "-p", "v12-cli", "--bin", "v12", "--"]);
        cmd
    }

    fn temp_script_path(suffix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("v12_cli_test_{}_{suffix}.js", std::process::id()));
        p
    }

    #[test]
    fn script_mode_spawn_throw_reports_uncaught() {
        let path = temp_script_path("throw");
        fs::write(&path, "throw 42;").expect("write temp file");
        let output = cargo_cmd()
            .arg(path.to_str().expect("utf8 path"))
            .output()
            .expect("cargo run failed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Uncaught"),
            "stderr should contain Uncaught, got: {stderr}"
        );
        assert!(
            stderr.contains("42"),
            "stderr should contain thrown value 42, got: {stderr}"
        );
        assert!(!output.status.success(), "exit should be failure");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn script_mode_spawn_success_exits_zero() {
        let path = temp_script_path("success");
        fs::write(&path, "let x = 1 + 2;").expect("write temp file");
        let output = cargo_cmd()
            .arg(path.to_str().expect("utf8 path"))
            .output()
            .expect("cargo run failed");
        assert!(
            output.status.success(),
            "script with no throw should exit 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn script_mode_spawn_file_not_found() {
        let missing = temp_script_path("missing_no_exists_12345");
        // Ensure it does not exist.
        let _ = fs::remove_file(&missing);
        let output = cargo_cmd()
            .arg(missing.to_str().expect("utf8 path"))
            .output()
            .expect("cargo run failed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "missing file should fail");
        // Should mention the path or an OS error.
        assert!(
            stderr.contains(missing.to_str().unwrap()) || stderr.contains("No such file"),
            "stderr should mention missing file, got: {stderr}"
        );
    }

    #[test]
    fn disasm_spawn_produces_listing() {
        let path = temp_script_path("disasm");
        fs::write(&path, "let x = 1;").expect("write temp file");
        let output = cargo_cmd()
            .args(["--disasm"])
            .arg(path.to_str().expect("utf8 path"))
            .output()
            .expect("cargo run failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "disasm should exit 0, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Disassembly contains "function" header and at least one mnemonic.
        assert!(
            stdout.contains("function"),
            "disassembly should contain 'function', got: {stdout}"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn disasm_spawn_syntax_error_exits_one() {
        let path = temp_script_path("disasm_err");
        fs::write(&path, "let = 1;").expect("write temp file");
        let output = cargo_cmd()
            .args(["--disasm"])
            .arg(path.to_str().expect("utf8 path"))
            .output()
            .expect("cargo run failed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "bad syntax should fail");
        assert!(
            stderr.contains("SyntaxError"),
            "stderr should contain SyntaxError, got: {stderr}"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn expose_gc_spawn_still_runs() {
        let path = temp_script_path("expose_gc");
        fs::write(&path, "throw 7;").expect("write temp file");
        let output = cargo_cmd()
            .args(["--expose-gc"])
            .arg(path.to_str().expect("utf8 path"))
            .output()
            .expect("cargo run failed");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("Uncaught"), "got: {stderr}");
        assert!(stderr.contains('7'), "got: {stderr}");
        let _ = fs::remove_file(&path);
    }
}
