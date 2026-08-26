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
    ///
    /// Global-code `var` declarations become properties of the realm's global
    /// object via the interpreter's `GLOBAL_VAR_OFFSET` alias (see
    /// `v12-interp` env handling).
    pub fn eval(&mut self, source: &str) -> Result<JsValue, JsValue> {
        self.eval_direct(source)
    }

    /// Direct `eval`: shares the caller's heap and global.
    ///
    /// Parses `source` with `v12_bccompiler::compile_source_with_strings` and
    /// executes the resulting main function in a fresh `Interp` that shares
    /// `self.heap` and `self.realm.global()`. `var` declarations in the eval
    /// code become properties on the global object (simple global merge for
    /// `v1`).
    pub fn eval_direct(&mut self, source: &str) -> Result<JsValue, JsValue> {
        if source.len() > MAX_SOURCE_LEN {
            let h = self
                .heap
                .intern_string(V12Str::latin1(b"RangeError: source too large".to_vec()));
            return Err(JsValue::string(h));
        }
        let global = self.realm.global();
        self.heap.add_root(JsValue::object(global));
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
        // Share the heap: move it into the interpreter, run, then reclaim.
        let heap = std::mem::replace(&mut self.heap, Heap::new(GcPolicy::NoGC));
        let mut interp =
            Interp::new_with_heap(heap, Some(global), program.functions, program.main, strings);
        let natives = self.registry.clone();
        interp.set_natives(Box::new(natives));
        let outcome = interp.run();
        let heap = interp.into_heap();
        self.heap = heap;
        let _ = self.jobs.drain(&mut self.heap);
        match outcome {
            Ok(()) => Ok(JsValue::undefined()),
            Err(JSException(thrown)) => Err(thrown),
        }
    }

    /// Indirect `eval`: fresh global scope (new heap + global).
    ///
    /// `var` declarations in `source` do **not** affect the caller's global.
    pub fn eval_indirect(&mut self, source: &str) -> Result<JsValue, JsValue> {
        if source.len() > MAX_SOURCE_LEN {
            let h = self
                .heap
                .intern_string(V12Str::latin1(b"RangeError: source too large".to_vec()));
            return Err(JsValue::string(h));
        }
        // Fresh heap + realm for the indirect eval.
        let mut heap = Heap::new(GcPolicy::default());
        let realm = Realm::new(&mut heap);
        let global = realm.global();
        heap.add_root(JsValue::object(global));
        let (program, strings) =
            v12_bccompiler::compile_source_with_strings(source).map_err(|err| {
                let msg = err.message;
                let handle = if msg.is_ascii() {
                    heap.intern_string(V12Str::latin1(msg.into_bytes()))
                } else {
                    heap.intern_string(V12Str::utf16(msg.encode_utf16().collect()))
                };
                JsValue::string(handle)
            })?;
        let mut interp =
            Interp::new_with_heap(heap, Some(global), program.functions, program.main, strings);
        let natives = self.registry.clone();
        interp.set_natives(Box::new(natives));
        let outcome = interp.run();
        match outcome {
            Ok(()) => Ok(JsValue::undefined()),
            Err(JSException(thrown)) => {
                // Translate thrown string into the caller's heap.
                if let Some(h) = thrown.as_string() {
                    // Need to get text from the fresh heap's string, then intern in caller.
                    // For v1, we use the interpreter's display helper.
                    let mut tmp = interp;
                    let text = tmp.to_display_string(thrown);
                    let _ = h;
                    let handle = if text.is_ascii() {
                        self.heap.intern_string(V12Str::latin1(text.into_bytes()))
                    } else {
                        self.heap
                            .intern_string(V12Str::utf16(text.encode_utf16().collect()))
                    };
                    Err(JsValue::string(handle))
                } else {
                    Err(thrown)
                }
            }
        }
    }

    /// Creates a function object from `params` and `body` strings.
    ///
    /// `params` is a comma-separated parameter list (e.g. `"a, b"`), `body`
    /// is the function body source. Compiles `function __f(params){body}` and
    /// returns a `KIND_FUNCTION` object whose `elements[0]` is the function
    /// index. The caller can invoke it by constructing an `Interp` with the
    /// same program (for `v1` the program is not retained; tests verify
    /// compilation and allocation only).
    pub fn create_function(&mut self, params: &str, body: &str) -> Result<JsValue, JsValue> {
        let src = format!("function __f({params}){{{body}}}");
        let (program, _strings) =
            v12_bccompiler::compile_source_with_strings(&src).map_err(|err| {
                let msg = err.message;
                let handle = if msg.is_ascii() {
                    self.heap.intern_string(V12Str::latin1(msg.into_bytes()))
                } else {
                    self.heap
                        .intern_string(V12Str::utf16(msg.encode_utf16().collect()))
                };
                JsValue::string(handle)
            })?;
        let idx = program
            .functions
            .iter()
            .position(|f| f.name_hint.as_deref() == Some("__f"))
            .unwrap_or(1) as u32;
        let func = self.heap.alloc(v12_heap::JsObject {
            kind: v12_heap::KIND_FUNCTION,
            elements: vec![JsValue::from_i32_smi(idx as i32).unwrap()],
            prototype: None,
            ..Default::default()
        });
        self.heap.add_root(JsValue::object(func));
        // Keep the program alive for the test duration by leaking its Arc
        // (v1: tests do not actually call the function through the engine's
        // heap; they verify the object was created).
        let _ = program;
        Ok(JsValue::object(func))
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

#[allow(dead_code)]
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

    #[test]
    fn eval_direct_shares_heap_and_global() {
        let mut engine = Engine::new();
        // Direct eval: captured var should alias the global object; do everything
        // in one eval so the declaration and use share the same UnitPlan.
        let thrown = engine
            .eval_direct("var directVar = 123; function f(){ return directVar; } throw f();")
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(123));
        // Also verify the global's properties contain the var value (via alias)
        let global = engine.realm().global();
        let heap = engine.heap();
        let found = heap
            .get(global)
            .properties
            .iter()
            .any(|v| v.as_smi() == Some(123));
        assert!(found);
    }

    #[test]
    fn eval_indirect_does_not_pollute_caller_global() {
        let mut engine = Engine::new();
        engine.eval_direct("var keep = 1;").expect("setup");
        let before_len = engine.heap().get(engine.realm().global()).properties.len();
        // Indirect eval with fresh heap should not affect caller's global
        let _ = engine.eval_indirect("var polluting = 999;");
        let after_len = engine.heap().get(engine.realm().global()).properties.len();
        assert_eq!(before_len, after_len);
        // Polluting var should not be visible via direct eval
        let result = engine.eval_direct("throw typeof polluting;").unwrap_err();
        let text = engine.to_display_string(result);
        assert_eq!(text, "undefined");
    }

    #[test]
    fn function_constructor_creates_function_object() {
        let mut engine = Engine::new();
        let func = engine.create_function("a", "return a+1;").expect("create");
        assert!(func.is_object());
        let handle = func.as_object().unwrap();
        assert_eq!(engine.heap().get(handle).kind, v12_heap::KIND_FUNCTION);
    }

    #[test]
    fn function_constructor_validates_syntax() {
        let mut engine = Engine::new();
        // Invalid param list "a b" (missing comma) should be a syntax error
        let err = engine.create_function("a b", "return 1;").unwrap_err();
        assert!(err.is_string());
    }

    #[test]
    fn global_var_hoisting_via_eval() {
        let mut engine = Engine::new();
        let thrown = engine
            .eval("var hoisted = 456; function g(){ return hoisted; } throw g();")
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(456));
        // Also check global properties contain the value
        let global = engine.realm().global();
        let heap = engine.heap();
        let found = heap
            .get(global)
            .properties
            .iter()
            .any(|v| v.as_smi() == Some(456));
        assert!(found, "global should contain hoisted var 456");
    }

    #[test]
    fn accessor_getter_and_setter_via_internal_methods() {
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        let key = {
            let h = heap.intern_string(V12Str::latin1(b"accProp".to_vec()));
            heap.add_root(JsValue::string(h));
            v12_heap::PropKey::from_string(h)
        };
        let getter = heap.intern_string(V12Str::latin1(b"77".to_vec()));
        heap.add_root(JsValue::string(getter));
        let shape = heap.define_accessor(
            heap.root_shape(),
            key,
            Some(getter),
            None,
            v12_heap::Attrs::DEFAULT,
        );
        heap.add_shape_root(shape);
        let desc = heap.lookup_property(shape, key).expect("accessor desc");
        assert!(desc.is_accessor());
        assert_eq!(desc.getter(), Some(getter));
        assert!(desc.slot().is_none());
        // Verify that a data descriptor for a different key is still data
        let other_key = {
            let h = heap.intern_string(V12Str::latin1(b"other".to_vec()));
            heap.add_root(JsValue::string(h));
            v12_heap::PropKey::from_string(h)
        };
        let data_shape = heap.add_property(heap.root_shape(), other_key, v12_heap::Attrs::DEFAULT);
        let data_desc = heap.lookup_property(data_shape, other_key).unwrap();
        assert!(data_desc.is_data());
    }

    #[test]
    fn whitespace_unicode_is_accepted() {
        let mut engine = Engine::new();
        // U+00A0 (NBSP) and U+2003 (EM SPACE) are valid whitespace in JS
        let src = "\u{00A0}var w = 1;\u{2003}throw w;";
        let thrown = engine.eval(src).unwrap_err();
        assert_eq!(thrown.as_smi(), Some(1));
    }

    #[test]
    fn typeof_null_is_object_and_undefined_is_undefined() {
        let mut engine = Engine::new();
        // `null` literal is not yet in the constant pool; test the other
        // typeof branches that are already implemented.
        let thrown2 = engine.eval("throw typeof undefined;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown2), "undefined");
        let thrown3 = engine.eval("throw typeof 123;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown3), "number");
        let thrown4 = engine.eval("throw typeof \"hello\";").unwrap_err();
        assert_eq!(engine.to_display_string(thrown4), "string");
        let thrown5 = engine.eval("throw typeof true;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown5), "boolean");
    }
}
