//! Built-in objects and functions.
//!
//! Each built-in is a native function registered with the interpreter's
//! `NativeRegistry`. The functions operate directly on the heap, using shape
//! transitions and string primitives.

pub mod array;
pub mod boolean;
pub mod error;
pub mod iterator;
pub mod map;
pub mod math;
pub mod number;
pub mod object;
pub mod promise;
pub mod regexp;
pub mod string;

use std::cell::RefCell;
use std::rc::Rc;

use v12_heap::{Heap, JsValue};

use crate::job_queue::Job;

/// Registry of native function indices. Indices beyond the compiled program
/// length route to this table.
///
/// `pending` is the enqueue side channel for natives: `queueMicrotask`,
/// `Promise#then` on a settled promise, and reaction settling all push jobs
/// here. The engine shares this `Rc` with its job queue so jobs enqueued
/// during interpreter execution join the current or next checkpoint.
///
/// Host functions registered through the embedding API (`register_fn`) are
/// capturing Rust closures, stored separately from the fn-pointer handlers.
#[derive(Default, Clone)]
pub struct NativeRegistry {
    handlers: std::collections::HashMap<u32, NativeHandler>,
    closures: std::collections::HashMap<u32, HostClosure>,
    pending: Rc<RefCell<Vec<Job>>>,
    /// Compiled-regexp cache for RegExp natives. Per-registry (per-engine) so
    /// object-handle indexes never collide across engines.
    regex_cache: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<u32, v12_regex::CompiledRegex>>>,
}

impl std::fmt::Debug for NativeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeRegistry")
            .field("handlers", &self.handlers.len())
            .field("closures", &self.closures.len())
            .field("pending", &self.pending.borrow().len())
            .finish()
    }
}

/// A native handler.
pub type NativeHandler = fn(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue>;

/// A host function implemented as a capturing Rust closure.
///
/// The closure receives the heap (for allocating return values), the `this`
/// value, and the argument slice; an `Err` return is thrown inside JS.
#[derive(Clone)]
pub struct HostClosure(Rc<RefCell<dyn FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue>>>);

impl HostClosure {
    /// Wraps a user closure. `F` must match the host-function signature
    /// with all lifetimes elided (higher-ranked).
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue> + 'static,
    {
        Self(Rc::new(RefCell::new(f)))
    }

    /// Invokes the closure.
    pub fn call(
        &self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsValue> {
        (self.0.borrow_mut())(heap, this, args)
    }
}

impl NativeRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Shares the enqueue side channel with the engine's job queue.
    pub fn set_pending(&mut self, pending: Rc<RefCell<Vec<Job>>>) {
        self.pending = pending;
    }

    /// Adopted follow-up jobs enqueued by natives since the last checkpoint.
    pub fn take_pending(&self) -> Vec<Job> {
        self.pending.borrow_mut().drain(..).collect()
    }

    /// Registers a handler at `index`.
    pub fn register(&mut self, index: u32, handler: NativeHandler) {
        self.handlers.insert(index, handler);
    }

    /// Registers a capturing closure at `index`.
    pub fn register_closure(&mut self, index: u32, closure: HostClosure) {
        self.closures.insert(index, closure);
    }

    /// Returns an iterator over `(index, handler)` pairs.
    ///
    /// Used by [`crate::Engine::eval_indirect`] (ADR-007) to clone the
    /// engine's installed natives into a fresh realm-local registry
    /// without touching the engine's `pending` sink. The returned pairs
    /// are exact copies of the registered handlers.
    pub fn snapshot_handlers(&self) -> impl Iterator<Item = (u32, NativeHandler)> + '_ {
        self.handlers.iter().map(|(&i, &h)| (i, h))
    }

    /// Dispatches a native call.
    pub fn dispatch(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        index: u32,
    ) -> Result<JsValue, JsValue> {
        if let Some(closure) = self.closures.get(&index).cloned() {
            return closure.call(heap, this, args);
        }
        if let Some(handler) = self.handlers.get(&index).copied() {
            handler(heap, this, args)
        } else {
            let msg = format!("TypeError: native function #{index} is not registered");
            let h = heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()));
            Err(JsValue::string(h))
        }
    }
}

impl v12_interp::NativeRegistry for NativeRegistry {
    fn call_native(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        index: u32,
    ) -> Result<JsValue, JsValue> {
        // Job-enqueuing natives need the side channel, which the bare
        // `NativeHandler` signature (`fn(&mut Heap, …)`) cannot carry; they
        // are intercepted here instead of registered as plain handlers.
        // RegExp natives need the per-registry compiled-pattern cache.
        match index {
            NATIVE_PROMISE_RESOLVE => promise::promise_resolve(heap, this, args),
            NATIVE_PROMISE_REJECT => promise::promise_reject(heap, this, args),
            NATIVE_PROMISE_THEN => promise::promise_then(heap, this, args, &self.pending),
            NATIVE_QUEUE_MICROTASK => promise::queue_microtask(heap, args, &self.pending),
            NATIVE_REGEXP_EXEC => regexp::regexp_exec(heap, &self.regex_cache, this, args),
            NATIVE_REGEXP_TEST => regexp::regexp_test(heap, &self.regex_cache, this, args),
            NATIVE_REGEXP_COMPILE => regexp::regexp_compile(heap, &self.regex_cache, this, args),
            _ => self.dispatch(heap, this, args, index),
        }
    }

    /// Direct `eval`: compile and run `source` against the shared heap and
    /// global, returning the script's completion value. The eval program is
    /// registered into the caller's cross-program registry so eval-created
    /// closures resolve from the caller's interpreter.
    fn eval(
        &mut self,
        heap: &mut Heap,
        source: &str,
        _this: JsValue,
        global: Option<v12_heap::Handle<v12_heap::JsObject>>,
        programs: std::rc::Rc<
            std::cell::RefCell<
                std::vec::Vec<(
                    std::rc::Rc<[v12_bytecode::FunctionBytecode]>,
                    std::rc::Rc<[String]>,
                )>,
            >,
        >,
    ) -> Result<JsValue, JsValue> {
        let (program, strings) = v12_bccompiler::compile_source_with_strings(source).map_err(
            |err| {
                let msg = err.message;
                let h = if msg.is_ascii() {
                    heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()))
                } else {
                    heap.intern_string(v12_heap::V12Str::utf16(msg.encode_utf16().collect()))
                };
                JsValue::string(h)
            },
        )?;
        // Register the eval program so its closures can be invoked from the
        // caller's program afterwards.
        let program_id = {
            let mut table = programs.borrow_mut();
            let id = table.len() as u32;
            table.push((
                std::rc::Rc::from(program.functions.into_boxed_slice()),
                std::rc::Rc::from(strings.clone().into_boxed_slice()),
            ));
            id
        };
        let mut interp = v12_interp::Interp::new_with_heap(
            heap,
            global,
            Vec::new(),
            program.main,
            strings,
        );
        interp.set_program_id(program_id);
        interp.set_programs(programs);
        interp.set_natives(Box::new(self.clone()));
        match interp.run() {
            Ok(()) => Ok(interp.completion_value().unwrap_or_else(JsValue::undefined)),
            Err(v12_interp::JSException(thrown)) => Err(thrown),
        }
    }
}

/// Indices for built-ins. Chosen beyond any plausible program function count.
pub const NATIVE_OBJECT_CREATE: u32 = 1000;
pub const NATIVE_OBJECT_GET_PROTOTYPE_OF: u32 = 1001;
pub const NATIVE_OBJECT_DEFINE_PROPERTY: u32 = 1002;
pub const NATIVE_ARRAY_PUSH: u32 = 1100;
pub const NATIVE_ARRAY_POP: u32 = 1101;
pub const NATIVE_STRING_CHAR_AT: u32 = 1200;
pub const NATIVE_STRING_SLICE: u32 = 1201;
/// `String(x)` — callable `String` intrinsic (ToString subset).
pub const NATIVE_STRING_CONSTRUCT: u32 = 1202;
pub const NATIVE_NUMBER_IS_NAN: u32 = 1300;
pub const NATIVE_MATH_ABS: u32 = 1400;
pub const NATIVE_BOOLEAN_CONSTRUCT: u32 = 1500;
pub const NATIVE_ERROR_CREATE: u32 = 1600;
pub const NATIVE_QUEUE_MICROTASK: u32 = 1700;
pub const NATIVE_PROMISE_RESOLVE: u32 = 1710;
pub const NATIVE_PROMISE_REJECT: u32 = 1711;
pub const NATIVE_PROMISE_THEN: u32 = 1712;
pub const NATIVE_ARRAY_JOIN: u32 = 1102;
pub const NATIVE_EVAL: u32 = 1800;
pub const NATIVE_FUNCTION: u32 = 1801;
pub const NATIVE_CONSOLE_LOG: u32 = 1900;
pub const NATIVE_MAP_CONSTRUCT: u32 = 2000;
pub const NATIVE_MAP_GET: u32 = 2001;
pub const NATIVE_MAP_SET: u32 = 2002;
pub const NATIVE_MAP_HAS: u32 = 2003;
pub const NATIVE_MAP_DELETE: u32 = 2004;
pub const NATIVE_MAP_SIZE: u32 = 2005;
pub const NATIVE_SET_CONSTRUCT: u32 = 2100;
pub const NATIVE_SET_ADD: u32 = 2101;
pub const NATIVE_SET_HAS: u32 = 2102;
pub const NATIVE_SET_DELETE: u32 = 2103;
pub const NATIVE_SET_SIZE: u32 = 2104;
pub const NATIVE_REGEXP_CONSTRUCT: u32 = 2300;
pub const NATIVE_REGEXP_EXEC: u32 = 2301;
pub const NATIVE_REGEXP_TEST: u32 = 2302;
pub const NATIVE_REGEXP_TO_STRING: u32 = 2303;
pub const NATIVE_REGEXP_COMPILE: u32 = 2304;
pub const NATIVE_ITERATOR_NEXT: u32 = 2200;
/// `Array.prototype[Symbol.iterator]` — creates an array-values iterator.
pub const NATIVE_ARRAY_ITERATOR: u32 = 2201;
/// `Map.prototype[Symbol.iterator]` — creates a map-entries iterator.
pub const NATIVE_MAP_ITERATOR: u32 = 2202;
/// `Set.prototype[Symbol.iterator]` — creates a set-values iterator.
pub const NATIVE_SET_ITERATOR: u32 = 2203;
/// `%IteratorPrototype%[Symbol.iterator]` — returns `this`.
pub const NATIVE_ITERATOR_SELF: u32 = 2204;
/// `Array.prototype.entries` — array-entries iterator.
pub const NATIVE_ARRAY_ITERATOR_ENTRIES: u32 = 2205;
/// `Array.prototype.keys` — array-keys iterator.
pub const NATIVE_ARRAY_ITERATOR_KEYS: u32 = 2206;

/// Installs the core built-ins into `registry`.
pub fn install_core(registry: &mut NativeRegistry) {
    registry.register(NATIVE_OBJECT_CREATE, object::object_create);
    registry.register(
        NATIVE_OBJECT_GET_PROTOTYPE_OF,
        object::object_get_prototype_of,
    );
    registry.register(
        NATIVE_OBJECT_DEFINE_PROPERTY,
        object::object_define_property,
    );
    registry.register(NATIVE_ARRAY_PUSH, array::array_push);
    registry.register(NATIVE_ARRAY_POP, array::array_pop);
    registry.register(NATIVE_STRING_CHAR_AT, string::string_char_at);
    registry.register(NATIVE_STRING_SLICE, string::string_slice);
    registry.register(NATIVE_STRING_CONSTRUCT, string_construct);
    registry.register(NATIVE_NUMBER_IS_NAN, number::number_is_nan);
    registry.register(NATIVE_MATH_ABS, math::math_abs);
    registry.register(NATIVE_BOOLEAN_CONSTRUCT, boolean::boolean_construct);
    registry.register(NATIVE_ERROR_CREATE, error::error_create);
    registry.register(NATIVE_ARRAY_JOIN, array_join);
    registry.register(NATIVE_EVAL, eval_stub);
    registry.register(NATIVE_FUNCTION, function_stub);
    registry.register(NATIVE_CONSOLE_LOG, console_log);
    registry.register(NATIVE_MAP_CONSTRUCT, map::map_construct);
    registry.register(NATIVE_MAP_GET, map::map_get);
    registry.register(NATIVE_MAP_SET, map::map_set);
    registry.register(NATIVE_MAP_HAS, map::map_has);
    registry.register(NATIVE_MAP_DELETE, map::map_delete);
    registry.register(NATIVE_MAP_SIZE, map::map_size);
    registry.register(NATIVE_SET_CONSTRUCT, map::set_construct);
    registry.register(NATIVE_SET_ADD, map::set_add);
    registry.register(NATIVE_SET_HAS, map::set_has);
    registry.register(NATIVE_SET_DELETE, map::set_delete);
    registry.register(NATIVE_SET_SIZE, map::set_size);
    iterator::install(registry);
    regexp::install(registry);
}

/// Renders a value the way `console.log` observes it (Tier-0 display subset).
/// Shared by `console_log`, `array_join`, and `string_construct`.
fn value_display_text(heap: &mut Heap, v: JsValue) -> String {
    if let Some(handle) = v.as_string() {
        heap.flatten(handle);
        return match &heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
            _ => String::new(),
        };
    }
    if let Some(number) = v.as_smi().map(f64::from).or(v.as_f64()) {
        if number.is_nan() {
            return "NaN".to_string();
        }
        if number == f64::INFINITY {
            return "Infinity".to_string();
        }
        if number == f64::NEG_INFINITY {
            return "-Infinity".to_string();
        }
        return format!("{number}");
    }
    if v.is_true() {
        return "true".to_string();
    }
    if v.is_false() {
        return "false".to_string();
    }
    if v.is_undefined() {
        return "undefined".to_string();
    }
    if v.is_null() {
        return "null".to_string();
    }
    if v.is_object() {
        return "[object Object]".to_string();
    }
    "<unprintable>".to_string()
}

/// `String(x)`: ES ToString subset for the callable `String` intrinsic.
/// The realm points the `String` placeholder's `elements[0]` at this index.
fn string_construct(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let text = args
        .first()
        .map_or_else(|| "undefined".to_string(), |&v| value_display_text(heap, v));
    Ok(JsValue::string(intern_text(heap, &text)))
}

/// `Array.prototype.join(separator?)`: element display strings joined by
/// `separator` (default `","`). `undefined`/`null` elements render empty,
/// matching ES `Array.prototype.join`.
fn array_join(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let Some(arr) = this.as_object() else {
        return Err(intern_type_error(heap, "TypeError: Array.prototype.join requires an array"));
    };
    let sep = match args.first() {
        Some(&v) if !v.is_undefined() => value_display_text(heap, v),
        _ => ",".to_string(),
    };
    // Snapshot before formatting: the display helpers may allocate (and thus
    // collect), invalidating a live borrow of the element store.
    let elements: Vec<JsValue> = if heap.get(arr).kind == v12_heap::KIND_ARRAY {
        heap.get(arr).elements_array.iter().collect()
    } else {
        heap.get(arr).elements.clone()
    };
    let mut parts = Vec::with_capacity(elements.len());
    for &v in &elements {
        if v.is_undefined() || v.is_null() {
            parts.push(String::new());
        } else {
            parts.push(value_display_text(heap, v));
        }
    }
    let text = parts.join(&sep);
    Ok(JsValue::string(intern_text(heap, &text)))
}

/// Interns `text` as a heap string, choosing the storage by ASCII-ness.
fn intern_text(heap: &mut Heap, text: &str) -> v12_heap::Handle<v12_heap::V12Str> {
    if text.is_ascii() {
        heap.intern_string(v12_heap::V12Str::latin1(text.as_bytes().to_vec()))
    } else {
        heap.intern_string(v12_heap::V12Str::utf16(text.encode_utf16().collect()))
    }
}

fn eval_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    // v1 stub: non-string args return as-is; string args are syntax-checked
    // via the compiler and return `undefined` on success. The full
    // heap-sharing `eval` path is exercised via `Engine::eval_direct`.
    if let Some(first) = args.first() {
        if let Some(h) = first.as_string() {
            heap.flatten(h);
            let text = match &heap.get(h).storage {
                v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).into_owned(),
                v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
                _ => String::new(),
            };
            if let Err(err) = v12_bccompiler::compile_source_with_strings(&text) {
                let msg = err.message;
                let handle = if msg.is_ascii() {
                    heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()))
                } else {
                    heap.intern_string(v12_heap::V12Str::utf16(msg.encode_utf16().collect()))
                };
                return Err(JsValue::string(handle));
            }
            Ok(JsValue::undefined())
        } else {
            Ok(*first)
        }
    } else {
        Ok(JsValue::undefined())
    }
}

fn function_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    // v1 stub for `new Function`: validate syntax and return a placeholder
    // function object. Full compilation is via `Engine::create_function`.
    if args.is_empty() {
        let func = heap.alloc(v12_heap::JsObject::function(
            v12_heap::FunctionTarget::Bytecode(0),
            None,
        ));
        heap.add_root(JsValue::object(func));
        return Ok(JsValue::object(func));
    }
    let mut param_parts = Vec::new();
    for &arg in &args[..args.len() - 1] {
        if let Some(h) = arg.as_string() {
            heap.flatten(h);
            let txt = match &heap.get(h).storage {
                v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).into_owned(),
                v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
                _ => String::new(),
            };
            param_parts.push(txt);
        }
    }
    let param_str = param_parts.join(",");
    let body = if let Some(last) = args.last().and_then(|v| v.as_string()) {
        heap.flatten(last);
        match &heap.get(last).storage {
            v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).into_owned(),
            v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let src = format!("function __f({param_str}){{{body}}}");
    if let Err(err) = v12_bccompiler::compile_source_with_strings(&src) {
        let msg = err.message;
        let handle = if msg.is_ascii() {
            heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()))
        } else {
            heap.intern_string(v12_heap::V12Str::utf16(msg.encode_utf16().collect()))
        };
        return Err(JsValue::string(handle));
    }
    let func = heap.alloc(v12_heap::JsObject::function(
        v12_heap::FunctionTarget::Bytecode(1),
        None,
    ));
    heap.add_root(JsValue::object(func));
    Ok(JsValue::object(func))
}

fn console_log(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let mut parts = Vec::with_capacity(args.len());
    for &v in args {
        parts.push(value_display_text(heap, v));
    }
    println!("{}", parts.join(" "));
    Ok(JsValue::undefined())
}

fn intern_type_error(heap: &mut Heap, msg: &str) -> JsValue {
    let h = heap.intern_string(v12_heap::V12Str::latin1(msg.as_bytes().to_vec()));
    JsValue::string(h)
}
