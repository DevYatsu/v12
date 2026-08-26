#![forbid(unsafe_code)]

//! Harness file loading for Test262.
//!
//! Test262 tests list required harness files via `includes: [...]` in their
//! frontmatter. Those files live in `test262/harness/`. We prepend their
//! contents to the test source before evaluation so the test can call
//! `assert`, `verifyProperty`, etc.

use std::path::{Path, PathBuf};

/// Maximum total harness source size (all includes combined).
const MAX_HARNESS_BYTES: usize = 1_000_000;

/// Maximum single harness file size.
const MAX_SINGLE_HARNESS_BYTES: usize = 500_000;

/// Result of loading a single harness file.
#[derive(Debug, Clone)]
pub struct IncludeError {
    /// Name requested in `includes`.
    pub name: String,
    /// Human-readable reason.
    pub reason: String,
}

/// Loads `includes` from `harness_dir`, returning concatenated source and any
/// load errors.
///
/// Files are concatenated in the order listed, separated by a newline and a
/// `// --- harness: <name> ---` comment for debuggability.
#[must_use]
pub fn load_harness_includes(includes: &[String], harness_dir: &Path) -> (String, Vec<IncludeError>) {
    let mut out = String::new();
    let mut errors = Vec::new();
    let mut total = 0usize;

    for name in includes {
        // Guard against directory traversal: includes must be a plain file
        // name (no `/` or `\`). Test262 guarantees this, but we enforce.
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            errors.push(IncludeError {
                name: name.clone(),
                reason: "invalid harness name (path traversal)".to_string(),
            });
            continue;
        }

        let path: PathBuf = harness_dir.join(name);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                errors.push(IncludeError {
                    name: name.clone(),
                    reason: format!("read error: {e}"),
                });
                continue;
            }
        };

        if bytes.len() > MAX_SINGLE_HARNESS_BYTES {
            errors.push(IncludeError {
                name: name.clone(),
                reason: format!("file too large ({} bytes)", bytes.len()),
            });
            continue;
        }

        if total + bytes.len() > MAX_HARNESS_BYTES {
            errors.push(IncludeError {
                name: name.clone(),
                reason: "combined harness too large".to_string(),
            });
            break;
        }

        let text = String::from_utf8_lossy(&bytes);
        out.push_str(&format!("// --- harness: {name} ---\n"));
        out.push_str(&text);
        out.push('\n');
        total += bytes.len();
    }

    (out, errors)
}

/// Locates the harness directory relative to `test262_root`.
///
/// The standard layout is `<test262_root>/harness`. For shallow clones this
/// is `conformance/test262/harness`. We also probe a few fallback locations
/// so the runner can be invoked from different working directories.
#[must_use]
pub fn find_harness_dir(test262_root: &Path) -> Option<PathBuf> {
    let candidate = test262_root.join("harness");
    if candidate.is_dir() {
        return Some(candidate);
    }
    // Fallback: if test262_root itself is the `harness` dir.
    if test262_root.file_name().is_some_and(|n| n == "harness") && test262_root.is_dir() {
        return Some(test262_root.to_path_buf());
    }
    None
}

/// Minimal JS polyfill injected when harness files cannot be loaded.
///
/// Defines `Test262Error`, `$DONOTEVALUATE`, and a tiny `assert` subset so
/// that tests that expect them still run. This is intentionally minimal —
/// the real harness from Test262 is preferred. The polyfill is only used
/// when the harness dir is missing or a required include failed to load.
pub const MINIMAL_HARNESS_POLYFILL: &str = r#"
// --- minimal harness polyfill (test262 harness not available) ---
function Test262Error(message) {
  this.message = message || "";
}
Test262Error.prototype.toString = function() { return "Test262Error: " + this.message; };
function $DONOTEVALUATE() { throw "Test262: This statement should not be evaluated."; }
var assert = assert || {};
assert._isSameValue = function(a, b) {
  if (a === b) return a !== 0 || 1 / a === 1 / b;
  return a !== a && b !== b;
};
assert.sameValue = function(actual, expected, message) {
  if (assert._isSameValue(actual, expected)) return;
  var msg = message ? message + " " : "";
  msg += "Expected SameValue(" + String(actual) + ", " + String(expected) + ") to be true";
  throw new Test262Error(msg);
};
assert.notSameValue = function(actual, unexpected, message) {
  if (!assert._isSameValue(actual, unexpected)) return;
  var msg = message ? message + " " : "";
  msg += "Expected SameValue(" + String(actual) + ", " + String(unexpected) + ") to be false";
  throw new Test262Error(msg);
};
assert.throws = function(expectedErrorConstructor, func, message) {
  if (typeof func !== "function") throw new Test262Error("assert.throws requires a function");
  var threw = false;
  var thrown;
  try { func(); } catch (e) { threw = true; thrown = e; }
  if (!threw) throw new Test262Error(message || "Expected function to throw");
  if (typeof expectedErrorConstructor !== "function") return;
  if (!(thrown instanceof expectedErrorConstructor)) {
    throw new Test262Error(message || "Expected " + expectedErrorConstructor.name + " but got " + thrown);
  }
};
// compareArray is a common include not in assert.js
function compareArray(a, b) {
  if (a.length !== b.length) return false;
  for (var i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn invalid_name_rejected() {
        let dir = Path::new("/tmp");
        let (src, errs) = load_harness_includes(&["../evil.js".to_string()], dir);
        assert!(src.is_empty());
        assert_eq!(errs.len(), 1);
        assert!(errs[0].reason.contains("traversal"));
    }

    #[test]
    fn missing_file_reports_error() {
        let dir = Path::new("/tmp/does-not-exist-zzz");
        let (src, errs) = load_harness_includes(&["assert.js".to_string()], dir);
        assert!(src.is_empty());
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn loads_concatenated() {
        let tmp = std::env::temp_dir().join(format!("v12_harness_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        fs::write(tmp.join("a.js"), "var A = 1;").unwrap();
        fs::write(tmp.join("b.js"), "var B = 2;").unwrap();
        let (src, errs) = load_harness_includes(&["a.js".to_string(), "b.js".to_string()], &tmp);
        assert!(errs.is_empty());
        assert!(src.contains("var A = 1;"));
        assert!(src.contains("var B = 2;"));
        // Order preserved
        assert!(src.find("var A").unwrap() < src.find("var B").unwrap());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn polyfill_contains_assert() {
        assert!(MINIMAL_HARNESS_POLYFILL.contains("assert.sameValue"));
        assert!(MINIMAL_HARNESS_POLYFILL.contains("Test262Error"));
    }
}
