//! Facade error type (ADR-005).
//!
//! Hosts see a flattened, string-based error; structured details live on
//! [`V12Error::Compile`] and [`V12Error::Thrown`]. The goal is "one type
//! the host imports; one `Display` impl the host prints."

use std::fmt;

/// Failure of a [`crate::Context`] operation.
#[derive(Debug, Clone)]
pub enum V12Error {
    /// Front-end rejected the source.
    Compile(String),
    /// Script threw a value; payload is the stringification of the thrown
    /// value (host readable; the engine keeps the original `JsValue` alive
    /// for power users that go through `v12_engine` directly).
    Thrown(String),
    /// Embedder refused the call (source too large, I/O error, etc.).
    Host(String),
}

impl fmt::Display for V12Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            V12Error::Compile(msg) => write!(f, "compile error: {msg}"),
            V12Error::Thrown(msg) => write!(f, "uncaught: {msg}"),
            V12Error::Host(msg) => write!(f, "host error: {msg}"),
        }
    }
}

impl std::error::Error for V12Error {}

impl V12Error {
    /// Short category label, useful for log routing.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            V12Error::Compile(_) => "compile",
            V12Error::Thrown(_) => "thrown",
            V12Error::Host(_) => "host",
        }
    }
}

impl From<v12_engine::EngineError> for V12Error {
    fn from(e: v12_engine::EngineError) -> Self {
        match e {
            v12_engine::EngineError::Compile(c) => V12Error::Compile(c.message),
            v12_engine::EngineError::Thrown(_) => {
                // The thrown value is still alive on the engine; the host
                // can fetch it via `Context::last_thrown`. We leave the
                // `String` payload empty here and let the facade fill it
                // in the eval path that has heap access.
                V12Error::Thrown(String::new())
            }
            v12_engine::EngineError::Host(msg) => V12Error::Host(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_uses_kind_label() {
        let e = V12Error::Compile("unexpected token".into());
        assert!(format!("{e}").contains("compile error"));
        let e = V12Error::Thrown("oops".into());
        assert!(format!("{e}").contains("uncaught: oops"));
        let e = V12Error::Host("file missing".into());
        assert!(format!("{e}").contains("file missing"));
    }

    #[test]
    fn kind_label_is_stable() {
        assert_eq!(V12Error::Compile(String::new()).kind(), "compile");
        assert_eq!(V12Error::Thrown(String::new()).kind(), "thrown");
        assert_eq!(V12Error::Host(String::new()).kind(), "host");
    }
}
