//! Built-in objects and functions.
//!
//! Each built-in is a native function registered with the interpreter's
//! `NativeRegistry`. The functions operate directly on the heap, using shape
//! transitions and string primitives.

pub mod array;
pub mod boolean;
pub mod error;
pub mod helpers;
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
use v12_native::{NativeId, Throw};

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
    handlers: std::collections::HashMap<NativeId, NativeHandler>,
    pending: Rc<RefCell<Vec<Job>>>,
    /// Compiled-regexp cache for RegExp natives. Per-registry (per-engine) so
    /// object-handle indexes never collide across engines. Single-threaded
    /// engine: an `Rc<RefCell>` (shared via clone), not a lock.
    regex_cache: regexp::RegexCache,
}

impl std::fmt::Debug for NativeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeRegistry")
            .field("handlers", &self.handlers.len())
            .field("pending", &self.pending.borrow().len())
            .finish()
    }
}

/// A native handler.
pub type NativeHandler = fn(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, Throw>;

/// A host function implemented as a capturing Rust closure.
///
/// The closure receives the heap (for allocating return values), the `this`
/// value, and the argument slice; an `Err` return is thrown inside JS.
#[derive(Clone)]
pub struct HostClosure(
    Rc<RefCell<dyn FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, Throw>>>,
);

impl HostClosure {
    /// Wraps a user closure. `F` must match the host-function signature
    /// with all lifetimes elided (higher-ranked).
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, Throw> + 'static,
    {
        Self(Rc::new(RefCell::new(f)))
    }

    /// Invokes the closure.
    pub fn call(&self, heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
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

    /// Registers a handler at `id`.
    pub fn register(&mut self, id: NativeId, handler: NativeHandler) {
        self.handlers.insert(id, handler);
    }

    /// Dispatches a native call.
    pub fn dispatch(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        id: NativeId,
    ) -> Result<JsValue, Throw> {
        // Compile-time table first (stateless builtins).
        if let Some(result) = builtin_dispatch(id, heap, this, args) {
            return result;
        }
        if let Some(handler) = self.handlers.get(&id).copied() {
            handler(heap, this, args)
        } else {
            Err(Throw::type_error(
                heap,
                format!("native function {id:?} is not registered"),
            ))
        }
    }
}

impl v12_native::NativeRegistry for NativeRegistry {
    fn call_native(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        id: NativeId,
    ) -> Result<JsValue, Throw> {
        // 1. Compile-time builtin table: a jump-table match, no lookup.
        if let Some(result) = builtin_dispatch(id, heap, this, args) {
            return result;
        }
        // 2. Stateful natives: job-enqueuing natives need the side channel,
        //    which the bare `NativeHandler` signature cannot carry, and
        //    RegExp natives need the per-registry compiled-pattern cache.
        //    They are intercepted here instead of registered as handlers.
        match id {
            NativeId::PromiseResolve => promise::promise_resolve(heap, this, args),
            NativeId::PromiseReject => promise::promise_reject(heap, this, args),
            NativeId::PromiseThen => promise::promise_then(heap, this, args, &self.pending),
            NativeId::QueueMicrotask => promise::queue_microtask(heap, args, &self.pending),
            NativeId::RegExpExec => regexp::regexp_exec(heap, &self.regex_cache, this, args),
            NativeId::RegExpTest => regexp::regexp_test(heap, &self.regex_cache, this, args),
            NativeId::RegExpCompile => regexp::regexp_compile(heap, &self.regex_cache, this, args),
            NativeId::StringMatch => string::string_match(heap, &self.regex_cache, this, args),
            NativeId::StringReplace => string::string_replace(heap, &self.regex_cache, this, args),
            NativeId::StringSearch => string::string_search(heap, &self.regex_cache, this, args),
            NativeId::StringSplit => string::string_split(heap, &self.regex_cache, this, args),
            // 3. Runtime map (host functions) or "not registered".
            _ => self.dispatch(heap, this, args, id),
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
        programs: std::rc::Rc<std::cell::RefCell<Vec<v12_native::ProgramTable>>>,
    ) -> Result<JsValue, Throw> {
        let (program, strings) =
            v12_bccompiler::compile_source_with_strings(source).map_err(|err| {
                let msg = err.message;
                let h = if msg.is_ascii() {
                    heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()))
                } else {
                    heap.intern_string(v12_heap::V12Str::utf16(msg.encode_utf16().collect()))
                };
                Throw::Value(JsValue::string(h))
            })?;
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
        let mut interp =
            v12_interp::Interp::new_with_heap(heap, global, Vec::new(), program.main, strings);
        interp.set_program_id(program_id);
        interp.set_programs(programs);
        interp.set_natives(Box::new(self.clone()));
        match interp.run() {
            Ok(()) => Ok(interp.completion_value().unwrap_or_else(JsValue::undefined)),
            Err(v12_interp::JSException(thrown)) => Err(Throw::Value(thrown)),
        }
    }
}

/// Built-in indices, as aliases of the shared [`NativeId`] enum.
///
/// These used to be a hand-numbered `u32` block that had to stay in sync with
/// the interpreter's duplicate constants; both now live in the single
/// `v12_native::NativeId` type. The aliases keep existing spelling working
/// while everything dispatches through the shared enum.
pub const NATIVE_OBJECT_CREATE: NativeId = NativeId::ObjectCreate;
pub const NATIVE_OBJECT_GET_PROTOTYPE_OF: NativeId = NativeId::ObjectGetPrototypeOf;
pub const NATIVE_OBJECT_DEFINE_PROPERTY: NativeId = NativeId::ObjectDefineProperty;
pub const NATIVE_ARRAY_PUSH: NativeId = NativeId::ArrayPush;
pub const NATIVE_ARRAY_POP: NativeId = NativeId::ArrayPop;
pub const NATIVE_STRING_CHAR_AT: NativeId = NativeId::StringCharAt;
pub const NATIVE_STRING_SLICE: NativeId = NativeId::StringSlice;
/// `String(x)` — callable `String` intrinsic (ToString subset).
pub const NATIVE_STRING_CONSTRUCT: NativeId = NativeId::StringConstruct;
/// `String.prototype.match(regexp)` — regexp match over a string.
pub const NATIVE_STRING_MATCH: NativeId = NativeId::StringMatch;
/// `String.prototype.replace(regexp, replacement)` — regexp replace.
pub const NATIVE_STRING_REPLACE: NativeId = NativeId::StringReplace;
/// `String.prototype.search(regexp)` — first match index.
pub const NATIVE_STRING_SEARCH: NativeId = NativeId::StringSearch;
/// `String.prototype.split(regexp, limit)` — split on regexp separators.
pub const NATIVE_STRING_SPLIT: NativeId = NativeId::StringSplit;
pub const NATIVE_NUMBER_IS_NAN: NativeId = NativeId::NumberIsNan;
pub const NATIVE_MATH_ABS: NativeId = NativeId::MathAbs;
pub const NATIVE_BOOLEAN_CONSTRUCT: NativeId = NativeId::BooleanConstruct;
pub const NATIVE_ERROR_CREATE: NativeId = NativeId::ErrorCreate;
pub const NATIVE_QUEUE_MICROTASK: NativeId = NativeId::QueueMicrotask;
pub const NATIVE_PROMISE_RESOLVE: NativeId = NativeId::PromiseResolve;
pub const NATIVE_PROMISE_REJECT: NativeId = NativeId::PromiseReject;
pub const NATIVE_PROMISE_THEN: NativeId = NativeId::PromiseThen;
pub const NATIVE_ARRAY_JOIN: NativeId = NativeId::ArrayJoin;
pub const NATIVE_EVAL: NativeId = NativeId::Eval;
pub const NATIVE_FUNCTION: NativeId = NativeId::Function;
pub const NATIVE_CONSOLE_LOG: NativeId = NativeId::ConsoleLog;
pub const NATIVE_MAP_CONSTRUCT: NativeId = NativeId::MapConstruct;
pub const NATIVE_MAP_GET: NativeId = NativeId::MapGet;
pub const NATIVE_MAP_SET: NativeId = NativeId::MapSet;
pub const NATIVE_MAP_HAS: NativeId = NativeId::MapHas;
pub const NATIVE_MAP_DELETE: NativeId = NativeId::MapDelete;
pub const NATIVE_MAP_SIZE: NativeId = NativeId::MapSize;
pub const NATIVE_SET_CONSTRUCT: NativeId = NativeId::SetConstruct;
pub const NATIVE_SET_ADD: NativeId = NativeId::SetAdd;
pub const NATIVE_SET_HAS: NativeId = NativeId::SetHas;
pub const NATIVE_SET_DELETE: NativeId = NativeId::SetDelete;
pub const NATIVE_SET_SIZE: NativeId = NativeId::SetSize;
pub const NATIVE_REGEXP_CONSTRUCT: NativeId = NativeId::RegExpConstruct;
pub const NATIVE_REGEXP_EXEC: NativeId = NativeId::RegExpExec;
pub const NATIVE_REGEXP_TEST: NativeId = NativeId::RegExpTest;
pub const NATIVE_REGEXP_TO_STRING: NativeId = NativeId::RegExpToString;
pub const NATIVE_REGEXP_COMPILE: NativeId = NativeId::RegExpCompile;
pub const NATIVE_ITERATOR_NEXT: NativeId = NativeId::IteratorNext;
/// `Array.prototype[Symbol.iterator]` — creates an array-values iterator.
pub const NATIVE_ARRAY_ITERATOR: NativeId = NativeId::ArrayIterator;
/// `Map.prototype[Symbol.iterator]` — creates a map-entries iterator.
pub const NATIVE_MAP_ITERATOR: NativeId = NativeId::MapIterator;
/// `Set.prototype[Symbol.iterator]` — creates a set-values iterator.
pub const NATIVE_SET_ITERATOR: NativeId = NativeId::SetIterator;
/// `%IteratorPrototype%[Symbol.iterator]` — returns `this`.
pub const NATIVE_ITERATOR_SELF: NativeId = NativeId::IteratorSelf;
/// `Array.prototype.entries` — array-entries iterator.
pub const NATIVE_ARRAY_ITERATOR_ENTRIES: NativeId = NativeId::ArrayIteratorEntries;
/// `Array.prototype.keys` — array-keys iterator.
pub const NATIVE_ARRAY_ITERATOR_KEYS: NativeId = NativeId::ArrayIteratorKeys;

// The compile-time builtin table: every pure, stateless native.
//
// Declared at module scope so `builtin_dispatch` is a module-level `match`
// (a jump table the compiler emits); `call_native` consults it before the
// runtime map. No array, no `HashMap`, no runtime construction.
v12_native::native_table! {
    ObjectCreate => object::object_create,
    ObjectGetPrototypeOf => object::object_get_prototype_of,
    ObjectDefineProperty => object::object_define_property,
    ArrayPush => array::array_push,
    ArrayPop => array::array_pop,
    ArrayJoin => array_join,
    StringCharAt => string::string_char_at,
    StringSlice => string::string_slice,
    StringConstruct => string_construct,
    NumberIsNan => number::number_is_nan,
    MathAbs => math::math_abs,
    BooleanConstruct => boolean::boolean_construct,
    ErrorCreate => error::error_create,
    Eval => eval_stub,
    Function => function_stub,
    ConsoleLog => console_log,
    MapConstruct => map::map_construct,
    MapGet => map::map_get,
    MapSet => map::map_set,
    MapHas => map::map_has,
    MapDelete => map::map_delete,
    MapSize => map::map_size,
    SetConstruct => map::set_construct,
    SetAdd => map::set_add,
    SetHas => map::set_has,
    SetDelete => map::set_delete,
    SetSize => map::set_size,
    IteratorNext => iterator::iterator_next,
    ArrayIterator => iterator::array_iterator,
    MapIterator => iterator::map_iterator,
    SetIterator => iterator::set_iterator,
    IteratorSelf => iterator::iterator_self,
    ArrayIteratorEntries => iterator::array_iterator_entries,
    ArrayIteratorKeys => iterator::array_iterator_keys,
    RegExpConstruct => regexp::regexp_construct,
    RegExpToString => regexp::regexp_to_string,
}

/// Installs the core built-ins into `registry`.
///
/// Stateless builtins live in the compile-time [`native_table!`] above and
/// need no registration. Stateful natives (Promise needs the job-queue sink;
/// RegExp needs the compiled-pattern cache) are intercepted in
/// `NativeRegistry::call_native`, so this stays a no-op today — kept for the
/// API shape (engine construction calls it) and for host hooks that register
/// additional natives.
pub fn install_core(registry: &mut NativeRegistry) {
    // Stateful natives carry per-engine state and are intercepted in
    // `NativeRegistry::call_native` (the job-queue sink, the compiled-pattern
    // cache), not registered as plain handlers.
    let _ = registry;
}

/// `String(x)`: ES ToString subset for the callable `String` intrinsic.
/// The realm points the `String` placeholder's `elements[0]` at this index.
fn string_construct(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(&v) => helpers::value_text(heap, v),
        None => "undefined".to_string(),
    };
    Ok(JsValue::string(helpers::intern_text(heap, &text)))
}

/// `Array.prototype.join(separator?)`: element display strings joined by
/// `separator` (default `","`). `undefined`/`null` elements render empty,
/// matching ES `Array.prototype.join`.
fn array_join(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let Some(arr) = this.as_object() else {
        return Err(
            (intern_type_error(heap, "TypeError: Array.prototype.join requires an array")).into(),
        );
    };
    let sep = match args.first() {
        Some(&v) if !v.is_undefined() => helpers::value_text(heap, v),
        _ => ",".to_string(),
    };
    // Snapshot before formatting: the display helpers may allocate (and thus
    // collect), invalidating a live borrow of the element store.
    let elements: Vec<JsValue> = if heap.get(arr).kind == v12_heap::Kind::Array {
        heap.get(arr).elements_array.iter().collect()
    } else {
        heap.get(arr).elements.clone()
    };
    let mut parts = Vec::with_capacity(elements.len());
    for &v in &elements {
        if v.is_undefined() || v.is_null() {
            parts.push(String::new());
        } else {
            parts.push(helpers::value_text(heap, v));
        }
    }
    let text = parts.join(&sep);
    Ok(JsValue::string(heap.intern_text(&text)))
}

fn eval_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    // v1 stub: non-string args return as-is; string args are syntax-checked
    // via the compiler and return `undefined` on success. The full
    // heap-sharing `eval` path is exercised via `Engine::eval_direct`.
    if let Some(first) = args.first() {
        if let Some(h) = first.as_string() {
            let text = helpers::string_text(heap, h);
            if let Err(err) = v12_bccompiler::compile_source_with_strings(&text) {
                let msg = err.message;
                let handle = helpers::intern_text(heap, &msg);
                return Err((JsValue::string(handle)).into());
            }
            Ok(JsValue::undefined())
        } else {
            Ok(*first)
        }
    } else {
        Ok(JsValue::undefined())
    }
}

fn function_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    // v1 stub for `new Function`: validate syntax and return a placeholder
    // function object. Full compilation is via `Engine::create_function`.
    if args.is_empty() {
        let func = helpers::alloc_obj(
            heap,
            v12_heap::JsObject::function(v12_heap::FunctionTarget::Bytecode(0), None),
        );
        return Ok(JsValue::object(func));
    }
    let mut param_parts = Vec::new();
    for &arg in &args[..args.len() - 1] {
        if let Some(h) = arg.as_string() {
            param_parts.push(helpers::string_text(heap, h));
        }
    }
    let param_str = param_parts.join(",");
    let body = args
        .last()
        .and_then(|v| v.as_string())
        .map(|h| helpers::string_text(heap, h))
        .unwrap_or_default();
    let src = format!("function __f({param_str}){{{body}}}");
    if let Err(err) = v12_bccompiler::compile_source_with_strings(&src) {
        let msg = err.message;
        let handle = helpers::intern_text(heap, &msg);
        return Err((JsValue::string(handle)).into());
    }
    let func = helpers::alloc_obj(
        heap,
        v12_heap::JsObject::function(v12_heap::FunctionTarget::Bytecode(1), None),
    );
    Ok(JsValue::object(func))
}

fn console_log(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut parts = Vec::with_capacity(args.len());
    for &v in args {
        parts.push(helpers::value_text(heap, v));
    }
    println!("{}", parts.join(" "));
    Ok(JsValue::undefined())
}

fn intern_type_error(heap: &mut Heap, msg: &str) -> JsValue {
    let h = heap.intern_string(v12_heap::V12Str::latin1(msg.as_bytes().to_vec()));
    JsValue::string(h)
}
