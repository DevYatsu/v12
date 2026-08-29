//! Runtime factory for [`Context`]s.
//!
//! v1 ships a thin factory: `Runtime::context()` returns a fresh
//! [`Context`] backed by a new `Engine`. v2 will add tier-policy and
//! heap-size configuration.

/// Configuration entry point. v1 has no knobs; the type exists so future
/// versions can add `Runtime::with_tier_policy(…)` etc. without breaking
/// imports.
#[derive(Debug, Default)]
pub struct Runtime;

impl Runtime {
    /// Creates a runtime with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Spawns a fresh [`Context`]. Each call returns an independent
    /// engine; contexts are not `Send` and must be used from the thread
    /// that created them.
    #[must_use]
    pub fn context(&mut self) -> crate::Context {
        crate::Context::new()
    }
}
