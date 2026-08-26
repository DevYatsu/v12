//! Embedding engine: heap, realm, interpreter, and job queue.

use v12_heap::{GcPolicy, Heap, JsValue, V12Str};
use v12_interp::{Interp, JSException};

use crate::builtins::{NativeRegistry, install_core};
use crate::job_queue::JobQueue;
use crate::realm::Realm;

/// Maximum length of a source text accepted by `eval`.
const MAX_SOURCE_LEN: usize = 1_000_000;

/// The JavaScript engine.
pub struct Engine {
    heap: Heap,
    realm: Realm,
    jobs: JobQueue,
    registry: NativeRegistry,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("realm", &self.realm)
            .field("jobs", &self.jobs)
            .finish()
    }
}

impl Engine {
    /// Creates a new engine with a fresh heap, realm, and job queue.
    #[must_use]
    pub fn new() -> Self {
        let mut heap = Heap::new(GcPolicy::default());
        let realm = Realm::new(&mut heap);
        let mut registry = NativeRegistry::new();
        install_core(&mut registry);
        Self {
            heap,
            realm,
            jobs: JobQueue::new(),
            registry,
        }
    }

    /// Access to the underlying heap.
    #[must_use]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// Mutable access to the heap.
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The engine's realm.
    #[must_use]
    pub fn realm(&self) -> &Realm {
        &self.realm
    }

    /// Mutable access to the job queue.
    pub fn jobs_mut(&mut self) -> &mut JobQueue {
        &mut self.jobs
    }

    /// Evaluates `source` as a script.
    ///
    /// On success returns the completion value (currently `undefined` for
    /// normal completions). On throw returns the thrown value. Both values
    /// are allocated in the engine's heap when they are strings.
    pub fn eval(&mut self, source: &str) -> Result<JsValue, JsValue> {
        if source.len() > MAX_SOURCE_LEN {
            let h = self
                .heap
                .intern_string(V12Str::latin1(b"RangeError: source too large".to_vec()));
            return Err(JsValue::string(h));
        }
        let (program, strings) =
            v12_bccompiler::compile_source_with_strings(source).map_err(|err| {
                let msg = err.message;
                let handle = if msg.is_ascii() {
                    self.heap.intern_string(V12Str::latin1(msg.into_bytes()))
                } else {
                    self.heap
                        .intern_string(V12Str::utf16(msg.encode_utf16().collect()))
                };
                JsValue::string(handle)
            })?;

        // Wire the compiler string table into the interpreter.
        let mut interp = Interp::new(program.functions, program.main, strings);
        // Install the engine's native registry by cloning the handler table
        // into a fresh registry that the interpreter owns.
        let natives = self.registry.clone();
        interp.set_natives(Box::new(natives));

        let outcome = interp.run();
        // Microtask checkpoint after top-level execution.
        let _ = self.jobs.drain(&mut self.heap);

        match outcome {
            Ok(()) => Ok(JsValue::undefined()),
            Err(JSException(thrown)) => {
                // Translate the thrown value from the interpreter's heap into
                // the engine's heap for handle values.
                let translated = translate_value(&mut self.heap, &mut interp, thrown);
                Err(translated)
            }
        }
    }

    /// Drains the microtask queue.
    ///
    /// Returns the number of jobs executed.
    pub fn run_jobs(&mut self) -> usize {
        self.jobs.drain(&mut self.heap)
    }

    /// Enqueues a microtask.
    pub fn enqueue_job<F>(&mut self, job: F) -> bool
    where
        F: FnOnce(&mut Heap) + 'static,
    {
        self.jobs.enqueue(Box::new(job))
    }

    /// Returns a display string for a value, using the engine heap.
    pub fn to_display_string(&mut self, value: JsValue) -> String {
        // For engine-heap values, intern and flatten via heap string ops.
        if let Some(handle) = value.as_string() {
            self.heap.flatten(handle);
            match &self.heap.get(handle).storage {
                v12_heap::StrStorage::Latin1(bytes) => {
                    return String::from_utf8_lossy(bytes).into_owned();
                }
                v12_heap::StrStorage::Utf16(units) => return String::from_utf16_lossy(units),
                _ => return String::new(),
            }
        }
        if let Some(n) = value.as_smi().map(f64::from).or(value.as_f64()) {
            if n.is_nan() {
                return "NaN".to_string();
            }
            if n == f64::INFINITY {
                return "Infinity".to_string();
            }
            if n == f64::NEG_INFINITY {
                return "-Infinity".to_string();
            }
            return format!("{n}");
        }
        if value.is_true() {
            return "true".to_string();
        }
        if value.is_false() {
            return "false".to_string();
        }
        if value.is_undefined() {
            return "undefined".to_string();
        }
        if value.is_null() {
            return "null".to_string();
        }
        if value.is_object() {
            return "[object Object]".to_string();
        }
        "<unprintable>".to_string()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

fn translate_value(engine_heap: &mut Heap, interp: &mut Interp, value: JsValue) -> JsValue {
    if value.is_smi()
        || value.is_f64()
        || value.is_undefined()
        || value.is_null()
        || value.is_boolean()
        || value.is_hole()
        || value.is_empty()
    {
        return value;
    }
    if let Some(_handle) = value.as_string() {
        let text = interp.to_display_string(value);
        let heap_handle = if text.is_ascii() {
            engine_heap.intern_string(V12Str::latin1(text.into_bytes()))
        } else {
            engine_heap.intern_string(V12Str::utf16(text.encode_utf16().collect()))
        };
        return JsValue::string(heap_handle);
    }
    // For objects and other reference types, return undefined as a placeholder
    // in the minimal embedding; a full structured clone would be needed for
    // complete fidelity.
    JsValue::undefined()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{FromValue, ToValue};

    #[test]
    fn engine_new_has_global_and_intrinsics() {
        let engine = Engine::new();
        assert!(engine.realm().get_intrinsic("Object").is_some());
        assert!(engine.realm().get_intrinsic("Array").is_some());
        assert!(engine.realm().get_intrinsic("String").is_some());
    }

    #[test]
    fn eval_returns_undefined_on_normal_completion() {
        let mut engine = Engine::new();
        let result = engine.eval("let x = 1;").expect("should run");
        assert!(result.is_undefined());
    }

    #[test]
    fn eval_throws_numeric_value() {
        let mut engine = Engine::new();
        let thrown = engine.eval("throw 42;").unwrap_err();
        assert_eq!(thrown.as_smi(), Some(42));
    }

    #[test]
    fn eval_throws_string_value_round_trips_through_engine_heap() {
        let mut engine = Engine::new();
        let thrown = engine.eval("throw 'hello';").unwrap_err();
        assert!(thrown.is_string());
        let text = engine.to_display_string(thrown);
        assert_eq!(text, "hello");
    }

    #[test]
    fn eval_compile_error_reports_string() {
        let mut engine = Engine::new();
        let err = engine.eval("let = 1;").unwrap_err();
        assert!(err.is_string());
        let text = engine.to_display_string(err);
        assert!(
            !text.is_empty(),
            "error text should not be empty, got {text:?}"
        );
    }

    #[test]
    fn eval_arithmetic_via_throw() {
        let mut engine = Engine::new();
        // Expression statement result is discarded, so we use throw to observe.
        let thrown = engine.eval("throw 1 + 2 * 3;").unwrap_err();
        assert_eq!(thrown.as_smi(), Some(7));
    }

    #[test]
    fn job_queue_enqueues_and_drains_after_eval() {
        let mut engine = Engine::new();
        let counter = std::rc::Rc::new(std::cell::RefCell::new(0i32));
        let c = std::rc::Rc::clone(&counter);
        engine.enqueue_job(move |_heap| {
            *c.borrow_mut() += 1;
        });
        // eval triggers checkpoint
        let _ = engine.eval("let x = 1;");
        assert_eq!(*counter.borrow(), 1);
    }

    #[test]
    fn run_jobs_drains_explicitly() {
        let mut engine = Engine::new();
        engine.enqueue_job(|_heap| {});
        engine.enqueue_job(|_heap| {});
        assert_eq!(engine.run_jobs(), 2);
        assert_eq!(engine.run_jobs(), 0);
    }

    #[test]
    fn to_value_and_from_value_round_trip() {
        let mut engine = Engine::new();
        let heap = engine.heap_mut();
        let v = 42i32.to_value(heap);
        assert_eq!(i32::from_value(heap, v), Some(42));
        let s = "hello".to_value(heap);
        assert_eq!(String::from_value(heap, s), Some("hello".to_string()));
        let b = true.to_value(heap);
        assert_eq!(bool::from_value(heap, b), Some(true));
    }

    #[test]
    fn eval_handles_large_source_limit() {
        let mut engine = Engine::new();
        let big = "a".repeat(1_000_001);
        let err = engine.eval(&big).unwrap_err();
        assert!(err.is_string());
    }
}
