//! Embedding [`Context`]: one engine, one realm, one heap.
//!
//! The facade surface. Internally wraps a `v12_engine::Engine`; the
//! wrapper is `!Send` (matching the single-mutator model) and exposes
//! the typed [`Context::eval<T>`] entry point.

use crate::error::V12Error;

/// One isolated JavaScript execution context.
pub struct Context {
    engine: v12_engine::Engine,
}

impl Context {
    /// Creates a fresh context backed by a new engine.
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: v12_engine::Engine::new(),
        }
    }

    /// Evaluates `source` and decodes the script's completion value into
    /// `T` via `v12_engine::FromValue`.
    ///
    /// `T` must implement `FromValue`; the supported primitives today
    /// are `f64`, `i32`, `i64`, `bool`, `String`, `Option<T>`, `Vec<T>`.
    pub fn eval<T>(&mut self, source: &str) -> Result<T, V12Error>
    where
        T: v12_engine::FromValue,
    {
        match self.engine.eval_with_completion(source) {
            Ok(v) => T::from_value(self.engine.heap(), v)
                .ok_or_else(|| V12Error::Thrown(format!("could not decode completion into {}", std::any::type_name::<T>()))),
            Err(e) => {
                // Convert the structured engine error to a facade error.
                // Throws carry a `JsValue`; for primitives we flatten
                // through the engine's display helper.
                match e {
                    v12_engine::EngineError::Thrown(js) => {
                        let msg = self.engine.to_display_string(js);
                        Err(V12Error::Thrown(msg))
                    }
                    v12_engine::EngineError::Compile(c) => Err(V12Error::Compile(c.message)),
                    v12_engine::EngineError::Host(msg) => Err(V12Error::Host(msg)),
                }
            }
        }
    }

    /// Drains the engine's microtask queue, returning the number of jobs
    /// that ran. v1 source-text scripts cannot enqueue jobs from JS, so
    /// this returns `0` unless a host previously called
    /// [`Context::enqueue_job`].
    pub fn pump(&mut self) -> usize {
        self.engine.run_jobs()
    }

    /// Enqueues a host-driven microtask.
    pub fn enqueue_job<F>(&mut self, job: F) -> bool
    where
        F: FnOnce(&mut v12_engine::job_queue::JobCtx<'_, '_>) + 'static,
    {
        self.engine.enqueue_job(job)
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}
