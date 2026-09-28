//! The engine's `NativeRegistry`: handler table, job-enqueue side channel,
//! RegExp pattern cache, host closures, and the `call_native` dispatch
//! (compile-time builtin table first, stateful natives intercepted, host
//! functions last).

use std::cell::RefCell;
use std::rc::Rc;

use v12_heap::{Handle, Heap, JsObject, JsValue};
use v12_native::{CompiledEval, EvalCompileFacts, NativeId, Throw, parse_error_text};

use super::{builtin_dispatch, ctx::Ctx, promise, regexp, string};
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
    /// Module loader state (module map + referrer). Shared with the engine
    /// and job-side loader driver via clone; `None` disables module loading
    /// (the `ModuleImport` native then throws, as before the loader existed).
    loader: Option<crate::module_loader::Loader>,
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

    /// Installs the module loader state (shared with the engine-side driver).
    pub(crate) fn set_loader(&mut self, loader: crate::module_loader::Loader) {
        self.loader = Some(loader);
    }

    /// The shared module loader state, if installed.
    pub(crate) fn loader(&self) -> Option<crate::module_loader::Loader> {
        self.loader.clone()
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
        if id == NativeId::ModuleImport {
            if let Some(loader) = self.loader.clone() {
                let pending = Rc::clone(&self.pending);
                let global = heap.realm_globals().first().copied();
                let mut ctx = Ctx::new(heap, global, Some(pending));
                return crate::module_loader::handle_import(&mut ctx, &loader, args);
            }
            return super::module_import(heap, this, args);
        }
        if let Some(handler) = self.handlers.get(&id).copied() {
            // Phase 2 shim: every legacy handler runs through the `Ctx`
            // adapter seam (bodies still take `&mut Heap`; no migration yet).
            let mut ctx = Ctx::new(heap, None, Some(Rc::clone(&self.pending)));
            super::ctx::call_legacy(handler, &mut ctx, this, args)
        } else {
            Err(Throw::type_error(
                heap,
                format!("native function {id:?} is not registered"),
            ))
        }
    }
}

impl NativeRegistry {
    /// `Ctx` for the stateful Promise natives: the shared pending-job sink
    /// plus the primary realm's global, so `Ctx::type_error` builds a real
    /// class instance. v1 is single-realm, so the first registered realm
    /// global is the right one.
    fn promise_ctx<'h>(&self, heap: &'h mut Heap) -> Ctx<'h> {
        let global = heap.realm_globals().first().copied();
        Ctx::new(heap, global, Some(Rc::clone(&self.pending)))
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
            // The Promise natives get the primary realm's global (the same
            // convention the ModuleImport arm below uses) so a `TypeError`
            // they throw is a real class instance: `Ctx::type_error` links the
            // error's [[Prototype]] and `constructor` only when a global is
            // attached, and `assert.throws(TypeError, ...)` needs the link.
            NativeId::PromiseResolve => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_resolve(&mut ctx, this, args)
            }
            NativeId::PromiseReject => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_reject(&mut ctx, this, args)
            }
            NativeId::PromiseThen => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_then(&mut ctx, this, args)
            }
            NativeId::PromiseCatch => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_catch(&mut ctx, this, args)
            }
            NativeId::PromiseAll => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_all(&mut ctx, this, args)
            }
            NativeId::PromiseRace => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_race(&mut ctx, this, args)
            }
            NativeId::PromiseFinally => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_finally(&mut ctx, this, args)
            }
            // `get Promise [ @@species ]`: installed by the realm as the get
            // accessor of a symbol-keyed property, so the function object
            // carries no `prototype` and reaches here by id.
            NativeId::PromiseSymbolSpeciesGet => {
                let mut ctx = self.promise_ctx(heap);
                promise::promise_species_get(&mut ctx, this, args)
            }
            NativeId::PromiseConstruct => Err(Throw::type_error(
                heap,
                "Promise must be constructed with new".to_string(),
            )),
            NativeId::QueueMicrotask => {
                let mut ctx = self.promise_ctx(heap);
                promise::queue_microtask(&mut ctx, this, args)
            }
            NativeId::RegExpExec => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_exec(&mut ctx, this, args)
            }
            NativeId::RegExpTest => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_test(&mut ctx, this, args)
            }
            NativeId::RegExpCompile => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_compile(&mut ctx, this, args)
            }
            NativeId::StringMatch => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                string::string_match(&mut ctx, this, args)
            }
            NativeId::StringReplace => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                string::string_replace(&mut ctx, this, args)
            }
            NativeId::StringSearch => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                string::string_search(&mut ctx, this, args)
            }
            NativeId::StringSplit => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                string::string_split(&mut ctx, this, args)
            }
            NativeId::StringMatchAll => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                string::string_match_all(&mut ctx, this, args)
            }
            // The `%RegExp.prototype%` accessor surface (ES 22.2.6). These
            // need no compiled pattern, but they share this arm rather than
            // the compile-time table so the whole RegExp prototype has one
            // dispatch home — and so a future `define_builtins!` entry for
            // one of them cannot silently bypass the cache the protocol
            // methods below require.
            NativeId::RegExpProtoGetSource => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_source(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetFlags => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_flags(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetGlobal => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_global(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetIgnoreCase => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_ignore_case(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetMultiline => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_multiline(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetDotAll => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_dot_all(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetUnicode => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_unicode(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetUnicodeSets => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_unicode_sets(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetSticky => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_sticky(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetHasIndices => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_has_indices(&mut ctx, this, args)
            }
            NativeId::RegExpProtoGetLastIndex => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_get_last_index(&mut ctx, this, args)
            }
            NativeId::RegExpProtoSetLastIndex => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_set_last_index(&mut ctx, this, args)
            }
            NativeId::RegExpSymbolSpeciesGet => {
                let mut ctx = self.promise_ctx(heap);
                regexp::regexp_species_get(&mut ctx, this, args)
            }
            // The 22.2.7 symbol-keyed protocol methods. `@@match`, `@@matchAll`,
            // `@@search` and `@@split` only ever call the *internal*
            // `[[RegExpMatcher]]`, never user code, so they are pure heap work
            // once the `&mut Heap` signature is all they get. `@@replace` is
            // intercepted at the interpreter seam whenever the replacement is
            // callable (`Interp::run_callback_builtin`); the arm here is the
            // string-replacement half, which is the fallback for an embedder
            // that drives the registry directly.
            NativeId::RegExpSymbolMatch => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_symbol_match(&mut ctx, this, args)
            }
            NativeId::RegExpSymbolMatchAll => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_symbol_match_all(&mut ctx, this, args)
            }
            NativeId::RegExpSymbolReplace => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_symbol_replace(&mut ctx, this, args)
            }
            NativeId::RegExpSymbolSearch => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_symbol_search(&mut ctx, this, args)
            }
            NativeId::RegExpSymbolSplit => {
                let mut ctx = Ctx::new(heap, None, None).with_regex_cache(self.regex_cache.clone());
                regexp::regexp_symbol_split(&mut ctx, this, args)
            }
            // Module import seam: loader-aware when installed (static imports
            // answer from the module map; dynamic `import()` returns a
            // promise backed by a load job), otherwise the spec-shaped
            // rejection fallback.
            NativeId::ModuleImport => {
                if let Some(loader) = self.loader.clone() {
                    let pending = Rc::clone(&self.pending);
                    let global = heap.realm_globals().first().copied();
                    let mut ctx = Ctx::new(heap, global, Some(pending));
                    return crate::module_loader::handle_import(&mut ctx, &loader, args);
                }
                super::module_import(heap, this, args)
            }
            // 3. Runtime map (host functions) or "not registered".
            _ => self.dispatch(heap, this, args, id),
        }
    }

    /// `NewPromiseCapability(C)` for `new Promise(executor)` — the promise and
    /// its `[[Resolve]]`/`[[Reject]]` pair as one three-element array.
    /// The executor call that follows is the interpreter's.
    fn promise_capability(&mut self, heap: &mut Heap, ctor: JsValue) -> Result<JsValue, Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::promise_capability(&mut ctx, ctor)
    }

    /// Settles that capability when the executor threw (27.5.1.1 step 6).
    fn capability_settle(
        &mut self,
        heap: &mut Heap,
        promise: JsValue,
        resolve: JsValue,
        reject: JsValue,
        value: JsValue,
        rejecting: bool,
    ) -> Result<JsValue, Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::capability_settle_values(&mut ctx, promise, resolve, reject, value, rejecting)
    }

    /// `NewPromiseCapability(C)`'s `GetCapabilitiesExecutor` builtin (spec
    /// 27.5.1.5 steps 3-4). `Construct` is the interpreter's, so this is only
    /// the object: a host closure over the record's two slots.
    fn capability_executor(&mut self, heap: &mut Heap, state: JsValue) -> Result<JsValue, Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::capability_executor(&mut ctx, state)
    }

    /// The recorded `(resolve, reject)` pair (spec 27.5.1.5 step 7).
    fn capability_pair(
        &mut self,
        heap: &mut Heap,
        state: JsValue,
    ) -> Result<(JsValue, JsValue), Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::capability_pair(&mut ctx, state)
    }

    /// `PerformPromiseThen` behind a species-selected derived promise.
    ///
    /// `resolve`/`reject` are `resultCapability`'s pair: the reaction job
    /// settles `derived` by calling them (27.5.2.1 step 4), which is where a
    /// returned thenable is adopted and where a species constructor's own
    /// resolving functions get their say.
    fn promise_perform_then(
        &mut self,
        heap: &mut Heap,
        promise: JsValue,
        derived: JsValue,
        resolve: JsValue,
        reject: JsValue,
        on_fulfilled: JsValue,
        on_rejected: JsValue,
    ) -> Result<(), Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::promise_perform_then(
            &mut ctx,
            promise,
            derived,
            resolve,
            reject,
            on_fulfilled,
            on_rejected,
        )
    }

    /// The `finally` watcher behind a species-selected derived promise.
    ///
    /// `resolve`/`reject` are `resultCapability`'s pair, for the same reason as
    /// in [`Self::promise_perform_then`]: 27.5.5.8's `thenFinally` settles the
    /// derived promise through them once the callback's own result settles.
    fn promise_watch_finally(
        &mut self,
        heap: &mut Heap,
        promise: JsValue,
        derived: JsValue,
        resolve: JsValue,
        reject: JsValue,
        on_finally: JsValue,
    ) -> Result<(), Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::promise_watch_finally(&mut ctx, promise, derived, resolve, reject, on_finally)
    }

    /// The streaming `Promise.all` / `Promise.race` seam. The interpreter
    /// drives it one element at a time (it owns the iterator, the
    /// `NewPromiseCapability(C)` constructor call, and every `Call` into user
    /// code); the engine owns the results array, the unsettled count and the
    /// reaction records.
    fn combinator_begin(
        &mut self,
        heap: &mut Heap,
        kind: i32,
        promise: JsValue,
        reject: JsValue,
    ) -> Result<JsValue, Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::combinator::begin(&mut ctx, kind, promise, reject)
    }

    fn combinator_step(
        &mut self,
        heap: &mut Heap,
        state: JsValue,
        index: u32,
        value: JsValue,
        then_v: JsValue,
        resolve: JsValue,
    ) -> Result<JsValue, Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::combinator::step(&mut ctx, state, index, value, then_v, resolve)
    }

    fn combinator_element_settled(
        &mut self,
        heap: &mut Heap,
        state: JsValue,
        index: u32,
        fulfilled: bool,
        value: JsValue,
    ) -> Result<Option<JsValue>, Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::combinator::element_settled(&mut ctx, state, index, fulfilled, value)
    }

    fn combinator_settle(
        &mut self,
        heap: &mut Heap,
        state: JsValue,
        exhausted: bool,
    ) -> Result<Option<JsValue>, Throw> {
        let mut ctx = self.promise_ctx(heap);
        promise::combinator::settle(&mut ctx, state, exhausted)
    }

    fn compile_eval(
        &mut self,
        heap: &mut Heap,
        source: &str,
        global: Option<Handle<JsObject>>,
        facts: &EvalCompileFacts,
    ) -> Result<CompiledEval, Throw> {
        let mut interner = v12_bccompiler::Interner::default();
        let program =
            v12_bccompiler::compile_eval_source_with_interner_facts(source, &mut interner, facts)
                .map_err(|err| Throw::Value(syntax_error_value(heap, global, &err.message)))?;
        let strings: Vec<String> = v12_bccompiler::freeze_interner(interner)
            .iter()
            .map(|(_, s)| s.to_string())
            .collect();
        Ok(CompiledEval {
            main: program.main,
            functions: Rc::from(program.functions.into_boxed_slice()),
            strings: Rc::from(strings.into_boxed_slice()),
        })
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
        let (program, strings) = v12_bccompiler::compile_eval_source_with_strings(source)
            .map_err(|err| Throw::Value(syntax_error_value(heap, global, &err.message)))?;
        // Register the eval program so its closures can be invoked from the
        // caller's program afterwards. The nested interpreter also installs
        // the eval function table locally: direct `self.functions` indexing
        // elsewhere would otherwise panic on the empty table (len 0).
        let funcs = std::rc::Rc::from(program.functions.into_boxed_slice());
        let program_id = {
            let mut table = programs.borrow_mut();
            let id = table.len() as u32;
            table.push((
                std::rc::Rc::clone(&funcs),
                std::rc::Rc::from(strings.clone().into_boxed_slice()),
            ));
            id
        };
        let mut interp =
            v12_interp::Interp::new_with_heap(heap, global, funcs, program.main, strings);
        interp.set_program_id(program_id);
        interp.set_programs(programs);
        interp.set_natives(Box::new(self.clone()));
        if let Some(loader) = self.loader.clone() {
            interp.set_module_evaluator(std::rc::Rc::new(
                crate::module_loader::ModuleEvaluatorAdapter::new(loader),
            ));
        }
        match interp.run() {
            Ok(()) => Ok(interp.completion_value().unwrap_or_else(JsValue::undefined)),
            Err(v12_interp::JSException(thrown)) => Err(Throw::Value(thrown)),
        }
    }

    /// `Function(params…, body)`: compile the body into a program, register
    /// it in the caller's cross-program table, and return a real closure
    /// stamped with the program id — so the result is callable and
    /// constructible from the caller's interpreter (unlike the compile-time
    /// `function_stub`, whose placeholder has no registered program).
    fn function_construct(
        &mut self,
        heap: &mut Heap,
        args: &[JsValue],
        global: Option<Handle<JsObject>>,
        programs: Rc<RefCell<Vec<v12_native::ProgramTable>>>,
    ) -> Result<JsValue, Throw> {
        let mut param_parts = Vec::new();
        for &arg in args[..args.len().saturating_sub(1)].iter() {
            if let Some(h) = arg.as_string() {
                param_parts.push(super::helpers::string_text(heap, h));
            }
        }
        let body = args
            .last()
            .and_then(|v| v.as_string())
            .map(|h| super::helpers::string_text(heap, h))
            .unwrap_or_default();
        let src = format!("function __f({}){{{}}}", param_parts.join(","), body);
        let (program, strings) =
            v12_bccompiler::compile_source_with_strings(&src).map_err(|err| {
                let (kind, message) = parse_error_text(&err.message, "SyntaxError");
                // Realm-linked (not `None`): `assert.throws(SyntaxError, …)`
                // needs `thrown.constructor === SyntaxError`, which only the
                // global-wired error object carries.
                Throw::Value(error_object(heap, global, kind, message))
            })?;
        let fn_idx = program
            .functions
            .iter()
            .position(|f| f.name_hint.as_deref() == Some("__f"))
            .or_else(|| {
                // The compiler prefixes declared names with "<fn>:"; accept
                // that spelling before falling back.
                program
                    .functions
                    .iter()
                    .position(|f| f.name_hint.as_deref() == Some("<fn>:__f"))
            })
            .unwrap_or(1) as u32;
        let program_id = {
            let mut table = programs.borrow_mut();
            let id = table.len() as u32;
            table.push((
                Rc::from(program.functions.into_boxed_slice()),
                Rc::from(strings.into_boxed_slice()),
            ));
            id
        };
        let mut func =
            v12_heap::JsObject::function(v12_heap::FunctionTarget::Bytecode(fn_idx), None);
        func.program_id = program_id;
        let handle = heap.alloc(func);
        heap.add_root(JsValue::object(handle));
        Ok(JsValue::object(handle))
    }
}

/// Builds a real error object of class `kind` carrying `message`.
///
/// Generalization of the former `syntax_error_value` (plan §4.2): the error
/// carries own `name` + `message` props plus an own `constructor` wired to
/// the caller's realm intrinsic for `kind` (located by its
/// `GLOBAL_INTRINSICS` slot). Kinds without a realm slot (`InternalError`)
/// or calls without a global skip the `constructor` link.
/// `assert.throws(SyntaxError, ...)` requires `typeof thrown === "object"`
/// with `thrown.constructor === SyntaxError`; a plain string never
/// satisfies it.
pub(crate) fn error_object(
    heap: &mut Heap,
    global: Option<Handle<JsObject>>,
    kind: &str,
    message: &str,
) -> JsValue {
    // Shape-aligned construction: `JsObject::error` pre-fills two
    // descriptor-less slots, so a later shape-bound install would land at
    // storage index 2 while its descriptor claims slot 0. Installing
    // `name`/`message`/`constructor` in order on an empty `Kind::Error`
    // keeps descriptors and storage aligned (and keeps `properties[0..2]`
    // as the name/message strings the display paths read directly).
    let name_h = heap.intern_text(kind);
    let msg_h = heap.intern_text(message);
    let obj = heap.alloc(JsObject {
        kind: v12_heap::Kind::Error,
        ..Default::default()
    });
    heap.add_root(JsValue::object(obj));
    super::builtin_install_prop(heap, obj, "name", JsValue::string(name_h));
    super::builtin_install_prop(heap, obj, "message", JsValue::string(msg_h));
    let ctor = global.and_then(|g| {
        // O(1) jump table over the fixed realm names (see `intrinsic_slot`
        // below) — replaces the old `.position()` linear scan.
        let idx = intrinsic_slot(kind)?;
        heap.get(g).properties.get(idx).copied()
    });
    if let Some(ctor) = ctor
        && ctor.as_object().is_some()
    {
        super::builtin_install_prop(heap, obj, "constructor", ctor);
        // Link the instance's [[Prototype]] to the class prototype object
        // (installed by the realm via `install_ctor`), so `instanceof`
        // walks to the right class and prototype `name` reads resolve.
        let ctor_obj = ctor.as_object().unwrap();
        if let Some(proto) = heap.get(ctor_obj).prototype {
            heap.get_mut(obj).prototype = Some(proto);
        }
    }
    JsValue::object(obj)
}

/// O(1) slot index for a `GLOBAL_INTRINSICS` name (compiler jump table).
/// Indices mirror `v12_bytecode::GLOBAL_INTRINSICS` order; the
/// `debug_assert!` pins each arm against the table so drift fails fast in
/// test builds.
fn intrinsic_slot(name: &str) -> Option<usize> {
    let idx = match name {
        "Object" => 0,
        "Array" => 1,
        "String" => 2,
        "Number" => 3,
        "Boolean" => 4,
        "Math" => 5,
        "JSON" => 6,
        "Error" => 7,
        "TypeError" => 8,
        "RangeError" => 9,
        "ReferenceError" => 10,
        "SyntaxError" => 11,
        "Promise" => 12,
        "Symbol" => 13,
        "Map" => 14,
        "Set" => 15,
        "RegExp" => 16,
        "eval" => 17,
        "console" => 18,
        "globalThis" => 19,
        "Proxy" => 20,
        "EvalError" => 21,
        "URIError" => 22,
        "AggregateError" => 23,
        _ => return None,
    };
    debug_assert!(v12_bytecode::GLOBAL_INTRINSICS.get(idx) == Some(&name));
    Some(idx)
}

/// Builds a real `SyntaxError` object for an `eval` compile failure.
fn syntax_error_value(
    heap: &mut Heap,
    global: Option<v12_heap::Handle<v12_heap::JsObject>>,
    message: &str,
) -> JsValue {
    let (kind, msg) = parse_error_text(message, "SyntaxError");
    error_object(heap, global, kind, msg)
}
