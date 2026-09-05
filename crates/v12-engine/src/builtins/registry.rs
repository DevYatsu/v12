//! The engine's `NativeRegistry`: handler table, job-enqueue side channel,
//! RegExp pattern cache, host closures, and the `call_native` dispatch
//! (compile-time builtin table first, stateful natives intercepted, host
//! functions last).

use std::cell::RefCell;
use std::rc::Rc;

use v12_heap::{Heap, JsValue};
use v12_native::{NativeId, Throw};

use super::{builtin_dispatch, promise, regexp, string};
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
    handlers: rustc_hash::FxHashMap<NativeId, NativeHandler>,
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
pub struct HostClosure(Rc<RefCell<HostFn>>);

/// The capturing host-function signature (see [`HostClosure`]).
pub type HostFn = dyn FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, Throw>;

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
            NativeId::PromiseCatch => promise::promise_catch(heap, this, args, &self.pending),
            NativeId::PromiseConstruct => promise::promise_construct(heap, &self.pending, this, args),
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
