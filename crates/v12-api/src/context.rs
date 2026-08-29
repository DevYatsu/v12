//! Embedding [`Context`]: one engine, one realm, one heap.
//!
//! The facade surface. Internally wraps a `v12_engine::Engine`; the
//! wrapper is `!Send` (matching the single-mutator model) and exposes
//! the typed [`Context::eval<T>`] entry point.

use crate::error::V12Error;

/// Wraps a user closure into a `HostClosure` (Rc<RefCell<dyn FnMut>>).
///
/// The double indirection lets the registry hold a cloneable handle while
/// the closure stays callable across interpreter re-entrancy.
fn wrap_host_closure<F>(f: F) -> v12_engine::HostClosure
where
    F: FnMut(&mut v12_heap::Heap, v12_engine::JsValue, &[v12_engine::JsValue])
        -> Result<v12_engine::JsValue, v12_engine::JsValue>
        + 'static,
{
    v12_engine::HostClosure::new(f)
}

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

    /// Registers a Rust closure as a global JS function named `name`.
    ///
    /// The closure receives the heap (for allocating return values), the
    /// `this` value, and the argument slice. Errors returned are thrown
    /// inside JS. Registering over an existing name replaces it.
    pub fn register_fn<F>(&mut self, name: &str, f: F) -> Result<(), V12Error>
    where
        F: FnMut(&mut v12_heap::Heap, v12_engine::JsValue, &[v12_engine::JsValue])
            -> Result<v12_engine::JsValue, v12_engine::JsValue>
            + 'static,
    {
        self.engine
            .create_host_function(name, wrap_host_closure(f))
            .map_err(|thrown| V12Error::Thrown(self.engine.to_display_string(thrown)))
    }

    /// Calls the global function `name` with `args` and converts the result
    /// to `T`.
    ///
    /// Requires a prior [`Context::eval`] (or `eval_module`) in this context
    /// so the function's bytecode is retained. `A` values are marshalled via
    /// `ToValue`, the result via `FromValue`.
    pub fn call<T, A>(&mut self, name: &str, args: &[A]) -> Result<T, V12Error>
    where
        T: v12_engine::FromValue,
        A: v12_engine::ToValue,
    {
        let js_args: Vec<v12_engine::JsValue> = args
            .iter()
            .map(|a| a.to_value(self.engine.heap_mut()))
            .collect();
        self.engine
            .call_global(name, &js_args)
            .map_err(|thrown| V12Error::Thrown(self.engine.to_display_string(thrown)))
            .and_then(|value| {
                T::from_value(self.engine.heap(), value).ok_or_else(|| {
                    V12Error::Thrown(format!(
                        "could not decode result into {}",
                        std::any::type_name::<T>()
                    ))
                })
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_fn_callable_from_js() {
        let mut ctx = Context::new();
        ctx.register_fn("add", |_heap, _this, args| {
            // Literal args arrive as Smis; decode both Smi and double forms.
            let num = |v: v12_engine::JsValue| {
                v.as_f64().or_else(|| v.as_smi().map(f64::from)).unwrap_or(0.0)
            };
            let a = args.first().copied().map(num).unwrap_or(0.0);
            let b = args.get(1).copied().map(num).unwrap_or(0.0);
            Ok(v12_engine::JsValue::from_f64(a + b))
        })
        .unwrap();
        // Expression-statement completion values are not yet wired through
        // `eval_with_completion` (ADR-004 gap), so surface the value through
        // a JS-side assertion that throws a distinguishable marker on
        // mismatch; the facade surfaces thrown strings.
        ctx.eval::<()>(
            "globalThis.__sum = add(20, 22); \
             if (globalThis.__sum !== 42) { throw 'sum was ' + globalThis.__sum; }",
        )
        .unwrap();
    }

    #[test]
    fn host_fn_closure_captures_state() {
        let mut ctx = Context::new();
        let counter = std::rc::Rc::new(std::cell::Cell::new(0_u32));
        let c2 = std::rc::Rc::clone(&counter);
        ctx.register_fn("bump", move |_heap, _this, _args| {
            c2.set(c2.get() + 1);
            Ok(v12_engine::JsValue::undefined())
        })
        .unwrap();
        ctx.eval::<()>("bump(); bump(); bump();").unwrap();
        assert_eq!(counter.get(), 3);
    }

    #[test]
    fn host_fn_thrown_string_becomes_js_exception() {
        let mut ctx = Context::new();
        ctx.register_fn("boom", |heap, _this, _args| {
            let h = heap.intern_string(v12_heap::V12Str::latin1(b"Error: kaput".to_vec()));
            Err(v12_engine::JsValue::string(h))
        })
        .unwrap();
        let err: Result<(), _> = ctx.eval::<()>("boom()");
        assert!(err.unwrap_err().to_string().contains("kaput"));
    }

    #[test]
    fn call_invokes_global_function_with_args() {
        let mut ctx = Context::new();
        ctx.eval::<()>("function greet(who) { return 'hi ' + who; }").unwrap();
        let s: String = ctx.call("greet", &["bob".to_string()]).unwrap();
        assert_eq!(s, "hi bob");
    }

    #[test]
    fn call_returns_typed_result() {
        let mut ctx = Context::new();
        ctx.eval::<()>("function sum(a, b) { return a + b; }").unwrap();
        let n: f64 = ctx.call("sum", &[2.0, 3.0]).unwrap();
        assert_eq!(n, 5.0);
    }

    #[test]
    fn call_unknown_function_is_error() {
        let mut ctx = Context::new();
        ctx.eval::<()>("function ok() {}").unwrap();
        let err: Result<f64, _> = ctx.call::<f64, f64>("missing", &[]);
        assert!(err.is_err());
    }

    #[test]
    fn call_throws_surface_as_v12_error() {
        let mut ctx = Context::new();
        // Throwing a string surfaces its text directly; error-object
        // constructors are not fully wired in the engine's builtins yet.
        ctx.eval::<()>("function bad() { throw 'out of bounds'; }").unwrap();
        let err: Result<f64, _> = ctx.call::<f64, f64>("bad", &[]);
        assert!(err.unwrap_err().to_string().contains("out"));
    }
}
