//! Structured engine errors (ADR-004).
//!
//! The old [`crate::Engine::eval`] returned `Result<JsValue, JsValue>` and
//! hard-coded normal completion to `undefined`. Hosts had to stringify
//! thrown values to classify them, and `eval("1+1")` could not return `2`.
//!
//! [`EngineError`] names the three failure modes the spec distinguishes:
//!
//! * [`EngineError::Compile`] — front-end refused the source. Carries a
//!   [`v12_bccompiler::CompileError`] with message and (optional) span.
//! * [`EngineError::Thrown`] — the script ran but threw a value. Carries
//!   the thrown [`v12_heap::JsValue`].
//! * [`EngineError::Host`] — the embedder (or our own guard) rejected the
//!   call. Carries a description string.
//!
//! `Ok(JsValue)` from `eval` is the *real* script completion value: `1+1`
//! returns `2`, `({a:1})` returns the object, etc. The legacy `Result<_, _>`
//! signature is preserved on a small shim ([`crate::Engine::eval_unwrap_value`])
//! for one release; the canonical entry point is
//! [`crate::Engine::eval_with_completion`].

use std::fmt;

use v12_bccompiler::CompileError;
use v12_heap::JsValue;

/// Engine failure with a typed discriminant.
#[derive(Debug, Clone)]
pub enum EngineError {
    /// The front-end refused the source text. Carries the compiler's
    /// structured diagnostic; `to_string` includes the byte span when
    /// present.
    Compile(CompileError),
    /// The script completed by throwing a value (ES `throw` statement,
    /// uncaught exception, etc.). `value` is the thrown value, allocated
    /// in the engine's heap.
    Thrown(JsValue),
    /// The engine refused the call for a non-script reason: source too
    /// long, I/O error on `eval_module_file`, or a host-side precondition.
    /// Hosts should treat this as a process-level failure (not a JS throw).
    Host(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Compile(e) => write!(f, "compile error: {e}"),
            EngineError::Thrown(_) => write!(f, "script threw an uncaught value"),
            EngineError::Host(msg) => write!(f, "host error: {msg}"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Compile(e) => Some(e),
            _ => None,
        }
    }
}

impl EngineError {
    /// Renders the error as a JS-style message string. For `Thrown`, this
    /// is the text of the thrown value when it is a string; otherwise a
    /// category label (e.g. `"uncaught number"`).
    ///
    /// The engine holds the heap, so the rendering is provided by
    /// [`crate::Engine::to_display_string`]; this is a best-effort label
    /// for log lines that don't have a heap handy.
    pub fn kind_label(&self) -> &'static str {
        match self {
            EngineError::Compile(_) => "compile",
            EngineError::Thrown(_) => "thrown",
            EngineError::Host(_) => "host",
        }
    }
}

impl From<CompileError> for EngineError {
    fn from(e: CompileError) -> Self {
        EngineError::Compile(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_kind_label() {
        let host = EngineError::Host("file not found".into());
        assert!(format!("{host}").contains("file not found"));
        let compile = EngineError::Compile(CompileError {
            message: "unexpected token".into(),
            span: Some((0, 1)),
        });
        let s = format!("{compile}");
        assert!(s.contains("compile error"));
        assert!(s.contains("unexpected token"));
        let thrown = EngineError::Thrown(JsValue::undefined());
        let s = format!("{thrown}");
        assert!(s.contains("script threw"));
    }

    #[test]
    fn kind_label_is_stable() {
        assert_eq!(
            EngineError::Host(String::new()).kind_label(),
            "host"
        );
        assert_eq!(
            EngineError::Thrown(JsValue::undefined()).kind_label(),
            "thrown"
        );
        assert_eq!(
            EngineError::Compile(CompileError {
                message: String::new(),
                span: None,
            })
            .kind_label(),
            "compile"
        );
    }

    #[test]
    fn from_compile_error() {
        let ce = CompileError {
            message: "x".into(),
            span: None,
        };
        let e: EngineError = ce.into();
        assert!(matches!(e, EngineError::Compile(_)));
    }
}
