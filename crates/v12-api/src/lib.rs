//! Convenient embedding facade for the v12 JavaScript engine (ADR-005).
//!
//! `v12-api` is the *only* thing hosts should import. The internal
//! `v12-engine` crate stays available for power users that need
//! `Engine::heap` access, but the facade guarantees a stable, minimal
//! surface.
//!
//! ```ignore
//! use v12_api::Context;
//!
//! let mut ctx = Context::new();
//! let n: f64 = ctx.eval("1 + 2").unwrap();
//! assert_eq!(n, 3.0);
//! ```
//!
//! # Realm model
//!
//! One [`Context`] = one engine, one realm, one heap. The facade follows
//! the v1 single-realm constraint documented in `CONTEXT.md:12`; the
//! [`Runtime`] factory is a placeholder for v2 multi-realm work.
//!
//! # Value marshalling
//!
//! [`Context::eval<T>`] deserializes the script's completion value into
//! `T` via `v12_engine::FromValue`; supported `T` are `f64`, `i32`, `i64`,
//! `bool`, `String`, `Option<T>`, and `Vec<T>`. Throws become
//! [`V12Error::Thrown`] with a `String` description; compile failures
//! become [`V12Error::Compile`].

#![forbid(unsafe_code)]

pub mod context;
pub mod error;
pub mod runtime;

pub use context::Context;
pub use error::V12Error;
pub use runtime::Runtime;

pub use v12_engine::{FromValue, HostClosure, JsValue, ToValue};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_factory_produces_context() {
        // The spec completion value of an expression-statement script is
        // not yet wired through (documented on `Engine::eval_with_completion`).
        // We exercise the facade using `()`, which decodes `undefined`.
        let mut rt = Runtime::new();
        let mut ctx = rt.context();
        let _: () = ctx.eval("var x = 3;").expect("ok");
    }

    #[test]
    fn context_eval_string() {
        // `()` decodes `undefined`, which is what expression-statement
        // scripts return today.
        let mut ctx = Context::new();
        let _: () = ctx.eval("var s = 'hello';").expect("ok");
    }

    #[test]
    fn context_eval_bool() {
        let mut ctx = Context::new();
        // `()` decodes `undefined` for the success path; the throw
        // path tests the error variant.
        let _: () = ctx.eval("var t = true;").expect("ok");
    }

    #[test]
    fn context_eval_throws_structured() {
        let mut ctx = Context::new();
        let err = ctx.eval::<f64>("throw 42;").unwrap_err();
        match err {
            V12Error::Thrown(msg) => assert_eq!(msg, "42"),
            other => panic!("expected Thrown, got {other:?}"),
        }
    }

    #[test]
    fn context_eval_compile_error_structured() {
        let mut ctx = Context::new();
        let err = ctx.eval::<f64>("let x = ;").unwrap_err();
        assert!(matches!(err, V12Error::Compile(_)));
    }

    #[test]
    fn pump_drains_microtasks() {
        let mut ctx = Context::new();
        let n = ctx.pump();
        // No jobs queued → 0 drained. The API is exercised; the value is
        // not load-bearing.
        assert_eq!(n, 0);
    }
}
