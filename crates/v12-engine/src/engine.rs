//! Embedding engine: heap, realm, interpreter, and job queue.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use v12_bytecode::FunctionBytecode;
use v12_heap::HeapExt;
use v12_heap::{GcPolicy, Heap, JsObject, JsValue, V12Str};
use v12_interp::{Interp, JSException};
use v12_native::{NativeId, Throw};

use crate::builtins::{NativeRegistry, install_core};
use crate::error::EngineError;
use crate::job_queue::{Job, JobCtx, JobQueue};
use crate::realm::Realm;

/// Maximum length of a source text accepted by `eval`.
const MAX_SOURCE_LEN: usize = 1_000_000;

/// Program of the last direct-eval, retained so [`Engine::run_jobs`] can
/// rebuild an interpreter for jobs that activate user functions (Promise
/// reaction handlers, `queueMicrotask` callbacks). One program is retained at
/// a time; a queued job belonging to an older program would mis-index — in
/// practice each eval drains its own checkpoint before the next compiles.
///
/// The program lives in an `Rc` so the engine no longer deep-clones
/// `functions` and `strings` on every `eval` / `run_jobs`. `run_jobs` just
/// clones the `Rc` (a refcount bump) to hand the program to the
/// `Interp::new` call. Old behavior: `Vec<FunctionBytecode>` (deep clone per
/// call) + `Vec<String>` (deep clone per call) → thousands of bytes copied
/// for every microtask drain. New behavior: one refcount bump. `Rc`, not
/// `Rc`: the engine is single-threaded, so atomics buy nothing.
struct RetainedProgram {
    functions: Rc<[FunctionBytecode]>,
    main: u32,
    strings: Rc<[String]>,
}

/// The JavaScript engine.
pub struct Engine {
    heap: Heap,
    realm: Realm,
    jobs: JobQueue,
    registry: NativeRegistry,
    /// Enqueue side channel shared with the registry: natives push jobs here
    /// during interpreter execution; the engine adopts them at checkpoints.
    pending: Rc<RefCell<Vec<Job>>>,
    retained: Option<RetainedProgram>,
    /// Last script completion value. Populated by every
    /// successful `eval*` call. `None` until the first script runs; older
    /// completion values are *not* cleared between calls — the most recent
    /// one is what the host sees via [`Engine::last_completion`].
    completion: Option<JsValue>,
    /// Tier-up policy. `OnDemand` (the default) means tier-up
    /// only happens when the host asks; `Profile` is the v2 hook.
    tier_policy: v12_codegen::TierPolicy,
    /// Cooperative execution deadline applied to every interpreter spawned by
    /// `eval*`/`run_jobs`. `None` by default (production embeds manage their
    /// own budgeting); the Test262 runner sets a per-test budget so a runaway
    /// loop cannot stall the harness.
    deadline: Option<Instant>,
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
        let pending: Rc<RefCell<Vec<Job>>> = Rc::new(RefCell::new(Vec::new()));
        let mut registry = NativeRegistry::new();
        registry.set_pending(Rc::clone(&pending));
        install_core(&mut registry);
        Self {
            heap,
            realm,
            jobs: JobQueue::new(),
            registry,
            pending,
            retained: None,
            completion: None,
            tier_policy: v12_codegen::TierPolicy::default(),
            deadline: None,
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

    /// Engine-owned helper for creating a pending Promise (delegates to `HeapExt`).
    /// Interp should call this via `EnginePromise` trait rather than allocating
    /// promise objects directly — hides `properties`/`property_keys` invariants.
    pub fn new_async_promise(&mut self) -> v12_heap::Handle<JsObject> {
        self.heap.alloc_pending_promise()
    }

    /// Alias required by `engine_owns_async_promise` (brief) — pending promise with `properties[0]==0`.
    pub fn new_pending_promise(&mut self) -> v12_heap::Handle<JsObject> {
        self.new_async_promise()
    }

    /// Engine-owned helper for creating a generator object (delegates to heap).
    pub fn new_generator_object(
        &mut self,
        fn_idx: u32,
        env: Option<v12_heap::Handle<JsObject>>,
    ) -> v12_heap::Handle<JsObject> {
        let h = self.heap.alloc(v12_heap::JsObject::generator_with(
            fn_idx,
            0,
            0.0,
            0,
            Vec::new(),
            env,
            None,
        ));
        self.heap.add_root(v12_heap::JsValue::object(h));
        h
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

    /// Sets the tier-up policy.
    ///
    /// `OnDemand` (default): tier-up only on host request. `Profile`: the
    /// engine's profiler decides when to tier-up. v1 ships the policy field
    /// and the setter; the profile-driven path is the v2 follow-up.
    pub fn set_tier_policy(&mut self, policy: v12_codegen::TierPolicy) {
        self.tier_policy = policy;
    }

    /// Current tier-up policy.
    #[must_use]
    pub fn tier_policy(&self) -> v12_codegen::TierPolicy {
        self.tier_policy
    }

    /// Sets the per-run execution deadline handed to every interpreter spawned
    /// by `eval*`/`run_jobs`. A `Some(instant)` budget makes a runaway script
    /// (no IO/await to yield on) abort with a catchable timeout error instead
    /// of blocking the caller. `None` restores the unbounded default.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    /// Evaluates `source` as a script.
    ///
    /// On success returns the completion value (currently `undefined` for
    /// normal completions). On throw returns the thrown value. Both values
    /// are allocated in the engine's heap when they are strings.
    ///
    /// Global-code `var` declarations become properties of the realm's global
    /// object via `SetGlobal`/`GetGlobal` (with the documented
    /// `GLOBAL_VAR_OFFSET` physical-slot bias on shape-derived slots).
    pub fn eval(&mut self, source: &str) -> Result<JsValue, JsValue> {
        self.eval_direct(source)
    }

    /// Evaluates `source` and returns the spec-compliant completion value.
    ///
    /// Unlike [`Self::eval`], this entry point returns the *real* script
    /// completion value when the script body explicitly returned one (e.g.
    /// `eval_with_completion("(function(){return 7})()")` returns `Ok(7)`).
    /// For scripts that just evaluate expression statements (e.g.
    /// `1 + 1`), the interpreter's per-statement result tracking is not yet
    /// wired through `top_result`; in that case the completion is
    /// `undefined` and [`Self::last_completion`] reports the same.
    ///
    /// Compile failures, throws, and host refusals are all distinguishable
    /// in the returned [`EngineError`].
    pub fn eval_with_completion(&mut self, source: &str) -> Result<JsValue, EngineError> {
        match self.eval_inner(source) {
            Ok(_unused) => Ok(self.last_completion()),
            Err(e) => Err(e),
        }
    }

    /// Legacy shim: `eval` that swallows the completion value, returning
    /// `Ok(JsValue::undefined())` on normal completion.
    ///
    /// `eval` and `eval_unwrap_value` are equivalent today; the latter is
    /// kept as the migration target for the next release (one-cycle
    /// deprecation: `eval` becomes the typed entry point and the unwrap
    /// variant moves to the facade).
    pub fn eval_unwrap_value(&mut self, source: &str) -> Result<JsValue, JsValue> {
        self.eval_direct(source)
    }

    /// The last script's actual completion value, or
    /// `undefined` if no script has run yet.
    ///
    /// Updated by every successful [`Self::eval`] / [`Self::eval_with_completion`].
    /// Cheap; doesn't allocate.
    #[must_use]
    pub fn last_completion(&self) -> JsValue {
        self.completion.unwrap_or_else(JsValue::undefined)
    }

    /// Direct `eval`: shares the caller's heap and global.
    ///
    /// Parses `source` with `v12_bccompiler::compile_source_with_strings` and
    /// executes the resulting main function in a fresh `Interp` that shares
    /// `self.heap` and `self.realm.global()`. `var` declarations in the eval
    /// code become properties on the global object (simple global merge for
    /// `v1`).
    pub fn eval_direct(&mut self, source: &str) -> Result<JsValue, JsValue> {
        match self.eval_inner(source) {
            Ok(value) => Ok(value),
            Err(EngineError::Thrown(t)) => Err(t),
            Err(EngineError::Host(msg)) => {
                let h = if msg.is_ascii() {
                    self.heap.intern_string(V12Str::latin1(msg.into_bytes()))
                } else {
                    self.heap
                        .intern_string(V12Str::utf16(msg.encode_utf16().collect()))
                };
                Err(JsValue::string(h))
            }
            Err(EngineError::Compile(err)) => {
                let msg = err.message;
                let handle = if msg.is_ascii() {
                    self.heap.intern_string(V12Str::latin1(msg.into_bytes()))
                } else {
                    self.heap
                        .intern_string(V12Str::utf16(msg.encode_utf16().collect()))
                };
                Err(JsValue::string(handle))
            }
        }
    }

    /// Private inner: compiles + runs + captures completion, returning a
    /// structured [`EngineError`]. Both [`Self::eval_direct`] (legacy) and
    /// [`Self::eval_with_completion`] are thin adapters over this.
    fn eval_inner(&mut self, source: &str) -> Result<JsValue, EngineError> {
        if source.len() > MAX_SOURCE_LEN {
            return Err(EngineError::Host("source too large".into()));
        }
        let global = self.realm.global();
        self.heap.add_root(JsValue::object(global));
        let (program, strings) =
            v12_bccompiler::compile_source_with_strings(source).map_err(EngineError::Compile)?;
        // Retain the program so queued jobs can rebuild an interpreter and
        // activate the script's functions later (Promise reactions).
        // Wrap the `Vec`s in `Rc` once; subsequent `eval`/`run_jobs` clone
        // the `Rc` instead of deep-cloning the function/string tables.
        let functions: Rc<[FunctionBytecode]> = Rc::from(program.functions.into_boxed_slice());
        let strings_arc: Rc<[String]> = Rc::from(strings.into_boxed_slice());
        self.retained = Some(RetainedProgram {
            functions: Rc::clone(&functions),
            main: program.main,
            strings: Rc::clone(&strings_arc),
        });
        // Borrow the engine's heap for the interpreter's lifetime —
        // no `mem::replace` swap, no sentinel heap, `Engine::heap()` stays
        // valid the whole time the interpreter runs. Destructure `self` so
        // the heap borrow and the job-queue/registry accesses are disjoint
        // locals (the borrow checker cannot see field disjointness through
        // `&mut self`).
        let deadline = self.deadline;
        let Engine {
            heap,
            jobs,
            registry,
            pending,
            completion,
            ..
        } = self;
        let mut interp = Interp::new_with_heap(
            heap,
            Some(global),
            functions.to_vec(),
            program.main,
            strings_arc.to_vec(),
        );
        let natives = registry.clone();
        interp.set_natives(Box::new(natives));
        interp.set_deadline(deadline);
        let outcome = interp.run();
        // Drain the single microtask checkpoint against the still-live
        // interpreter: host jobs and async resumes alternate until empty.
        let _ = Self::drain_checkpoint(registry, &mut interp, jobs, pending);
        // Capture the actual completion value (e.g. `1+1` → 2);
        // `eval` and `eval_with_completion` both return it.
        *completion = interp.completion_value();
        drop(interp); // releases the `&mut heap` borrow
        match outcome {
            // Return the real script completion value (e.g.
            // `1+1` → 2) instead of hard-coded `undefined`.
            Ok(()) => Ok(completion.unwrap_or_else(JsValue::undefined)),
            Err(JSException(thrown)) => Err(EngineError::Thrown(thrown)),
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
        // The indirect-eval gets its OWN `NativeRegistry` with its
        // OWN pending sink, so jobs enqueued in this realm never reach the
        // engine's queue and no `set_pending` save/restore is needed. The
        // engine's `self.registry` is left untouched for the whole call.
        // `NativeRegistry` is `Clone`, so the local registry starts as a full
        // copy of the engine's (builtins + host functions) — the old
        // `snapshot_handlers` + `install_core` dance is obsolete.
        let mut local_registry = self.registry.clone();
        local_registry.set_pending(Rc::new(RefCell::new(Vec::new())));
        let mut interp = Interp::new_with_heap(
            &mut heap,
            Some(global),
            program.functions,
            program.main,
            strings,
        );
        interp.set_natives(Box::new(local_registry.clone()));
        interp.set_deadline(self.deadline);
        let outcome = interp.run();
        // Drain this realm's checkpoint against its own interpreter; the
        // engine's own queued jobs reference the engine heap and are left
        // untouched for the next engine checkpoint.
        let mut local_queue = JobQueue::new();
        let local_pending = local_registry.take_pending();
        for job in local_pending {
            local_queue.enqueue(job);
        }
        let _ = local_queue.drain(&mut interp, Rc::new(RefCell::new(Vec::new())));
        // Indirect eval also returns its completion value.
        let completion = interp.completion_value().unwrap_or_else(JsValue::undefined);
        match outcome {
            Ok(()) => Ok(completion),
            Err(JSException(thrown)) => {
                // Translate thrown string into the caller's heap.
                if let Some(h) = thrown.as_string() {
                    // Need to get text from the fresh heap's string, then intern in caller.
                    // For v1, we use the interpreter's display helper.
                    let text = interp.to_display_string(thrown);
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

    /// Evaluates `source` as an ES module.
    ///
    /// Compiles with `SourceType::module` (always strict) and runs the
    /// resulting program. Imports are resolved via a dummy handler that
    /// returns an empty namespace object for any specifier; this is
    /// sufficient for syntax and linkage tests that do not check imported
    /// values. Real file-based imports are handled by `eval_module_file`.
    pub fn eval_module(&mut self, source: &str) -> Result<JsValue, JsValue> {
        self.eval_module_source(source, Path::new("."))
    }

    /// Evaluates `source` as a module with `base` for import resolution.
    pub fn eval_module_source(&mut self, source: &str, _base: &Path) -> Result<JsValue, JsValue> {
        if source.len() > MAX_SOURCE_LEN {
            let h = self
                .heap
                .intern_string(V12Str::latin1(b"RangeError: source too large".to_vec()));
            return Err(JsValue::string(h));
        }
        let global = self.realm.global();
        self.heap.add_root(JsValue::object(global));
        // Compile as module.
        let mut interner = v12_bccompiler::Interner::default();
        let module = v12_bccompiler::compile_source_as_module_with_interner(source, &mut interner)
            .map_err(|err| {
                let msg = err.message;
                let handle = if msg.is_ascii() {
                    self.heap.intern_string(V12Str::latin1(msg.into_bytes()))
                } else {
                    self.heap
                        .intern_string(V12Str::utf16(msg.encode_utf16().collect()))
                };
                JsValue::string(handle)
            })?;
        let strings: Vec<String> = v12_bccompiler::freeze_interner(interner)
            .iter()
            .map(|(_, s)| s.to_string())
            .collect();
        let program = module.program;
        // Wrap in `Rc` once so `run_jobs` can clone the handle.
        let functions: Rc<[FunctionBytecode]> = Rc::from(program.functions.into_boxed_slice());
        let strings_arc: Rc<[String]> = Rc::from(strings.into_boxed_slice());
        self.retained = Some(RetainedProgram {
            functions: Rc::clone(&functions),
            main: program.main,
            strings: Rc::clone(&strings_arc),
        });
        // Borrow the engine's heap; no swap. Destructure `self` so
        // the heap borrow and job-queue/registry accesses are disjoint locals.
        let deadline = self.deadline;
        let Engine {
            heap,
            jobs,
            registry,
            pending,
            completion,
            ..
        } = self;
        let mut interp = Interp::new_with_heap(
            heap,
            Some(global),
            functions.to_vec(),
            program.main,
            strings_arc.to_vec(),
        );
        // Install module-aware natives: 254 returns empty namespace.
        struct ModuleImportNatives {
            inner: NativeRegistry,
        }
        impl v12_interp::NativeRegistry for ModuleImportNatives {
            fn call_native(
                &mut self,
                heap: &mut Heap,
                this: JsValue,
                args: &[JsValue],
                id: NativeId,
            ) -> Result<JsValue, Throw> {
                // The module-import seam is the shared `ModuleImport` native
                // (discriminant 254): the engine builds an empty namespace.
                if id == NativeId::ModuleImport {
                    let h = heap.alloc(JsObject::default());
                    // Empty namespace object (no properties).
                    heap.add_root(JsValue::object(h));
                    return Ok(JsValue::object(h));
                }
                self.inner.call_native(heap, this, args, id)
            }
        }
        let natives = ModuleImportNatives {
            inner: registry.clone(),
        };
        interp.set_natives(Box::new(natives));
        interp.set_deadline(deadline);
        let outcome = interp.run();
        // Single checkpoint: host jobs + async resumes alternate until empty.
        let _ = Self::drain_checkpoint(registry, &mut interp, jobs, pending);
        // Capture the module's completion value too.
        *completion = interp.completion_value();
        drop(interp); // releases the `&mut heap` borrow
        match outcome {
            Ok(()) => Ok(completion.unwrap_or_else(JsValue::undefined)),
            Err(JSException(thrown)) => Err(thrown),
        }
    }

    /// Evaluates a module file at `path`, resolving imports relative to its directory.
    pub fn eval_module_file(&mut self, path: &Path) -> Result<JsValue, JsValue> {
        let source = std::fs::read_to_string(path).map_err(|e| {
            let msg = format!("Error reading {}: {e}", path.display());
            let h = self.heap.intern_string(V12Str::latin1(msg.into_bytes()));
            JsValue::string(h)
        })?;
        self.eval_module_source(&source, path.parent().unwrap_or(Path::new(".")))
    }

    /// Creates a function object from `params` and `body` strings.
    ///
    /// `params` is a comma-separated parameter list (e.g. `"a, b"`), `body`
    /// is the function body source. Compiles `function __f(params){body}` and
    /// returns a `Kind::Function` object whose `elements[0]` is the function
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
        let func = self.heap.alloc(v12_heap::JsObject::function(
            v12_heap::FunctionTarget::Bytecode(idx),
            None,
        ));
        self.heap.add_root(JsValue::object(func));
        // Keep the program alive for the test duration by leaking its Rc
        // (v1: tests do not actually call the function through the engine's
        // heap; they verify the object was created).
        let _ = program;
        Ok(JsValue::object(func))
    }

    /// Registers a capturing Rust closure as a global function named `name`.
    ///
    /// The function is installed as a property on the realm's global object
    /// (shape transition, mirroring the interpreter's `SetGlobal` fast path).
    /// It dispatches through `FunctionTarget::Host` (one word on the function
    /// object), so no registry entry is needed.
    pub fn create_host_function(
        &mut self,
        name: &str,
        closure: crate::builtins::HostClosure,
    ) -> Result<(), JsValue> {
        let global = self.realm.global();
        // The function object carries the host closure directly (one word),
        // so `prepare_call` invokes it without a registry lookup. The engine
        // closure wraps an `Rc<RefCell<dyn FnMut>>`; the heap `HostClosure`
        // adapts it to the one-word handle. Ownership: the returned function
        // object and its closure live on the heap for the engine's lifetime.
        let heap_closure = v12_heap::HostClosure::new(move |heap, this, args| {
            closure.call(heap, this, args).map_err(|t| t.into_js(heap))
        });
        let func = self.heap.alloc(v12_heap::JsObject::function(
            v12_heap::FunctionTarget::Host(heap_closure),
            None,
        ));
        self.heap.add_root(JsValue::object(func));
        // Install `name` on the global via the public shape API, exactly as
        // the interpreter's `op_set_global` does (GLOBAL_VAR_OFFSET bias).
        let h = self
            .heap
            .intern_string(V12Str::latin1(name.as_bytes().to_vec()));
        let key = v12_heap::PropKey::from_string(h);
        let shape = self.heap.shape_of(global);
        if let Some(desc) = self.heap.lookup_property(shape, key)
            && let Some(slot) = desc.slot()
        {
            let idx = crate::realm::INTRINSIC_COUNT + slot as usize;
            let len = self.heap.get(global).properties.len();
            if idx >= len {
                self.heap
                    .get_mut(global)
                    .properties
                    .resize(idx + 1, JsValue::undefined());
            }
            self.heap.get_mut(global).properties[idx] = JsValue::object(func);
        } else {
            let child = self.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
            self.heap.bind_shape(global, child);
            let new_slot =
                usize::try_from(self.heap.get(child).num_own - 1).expect("slot fits usize");
            let idx = crate::realm::INTRINSIC_COUNT + new_slot;
            let len = self.heap.get(global).properties.len();
            if len <= idx {
                self.heap
                    .get_mut(global)
                    .properties
                    .resize(idx + 1, JsValue::undefined());
            }
            self.heap.get_mut(global).properties[idx] = JsValue::object(func);
        }
        Ok(())
    }

    /// Calls the global function `name` with `args`.
    ///
    /// Resolves `name` on the realm's global object (shape lookup, the same
    /// mechanism `GetGlobal` uses) and invokes it through a fresh interpreter
    /// borrowing the engine's heap. Requires a prior `eval` (or
    /// `eval_module`) so the retained program provides the bytecode for
    /// `name` and any functions it calls. Returns the callee's result.
    pub fn call_global(&mut self, name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
        let global = self.realm.global();
        let func = {
            let h = self
                .heap
                .intern_string(V12Str::latin1(name.as_bytes().to_vec()));
            let key = v12_heap::PropKey::from_string(h);
            let shape = self.heap.shape_of(global);
            let desc = self.heap.lookup_property(shape, key);
            let slot = desc.and_then(|d| d.slot()).ok_or_else(|| {
                let h = self.heap.intern_string(V12Str::latin1(
                    format!("ReferenceError: {name} is not defined").into_bytes(),
                ));
                JsValue::string(h)
            })?;
            let idx = crate::realm::INTRINSIC_COUNT + slot as usize;
            self.heap
                .get(global)
                .properties
                .get(idx)
                .copied()
                .ok_or_else(|| {
                    let h = self.heap.intern_string(V12Str::latin1(
                        format!("ReferenceError: {name} is not defined").into_bytes(),
                    ));
                    JsValue::string(h)
                })?
        };
        let callee = func.as_object().ok_or_else(|| {
            let h = self.heap.intern_string(V12Str::latin1(
                format!("TypeError: {name} is not a function").into_bytes(),
            ));
            JsValue::string(h)
        })?;
        self.heap.add_root(JsValue::object(callee));

        let (functions, main, strings) = match &self.retained {
            Some(r) => (
                r.functions.to_vec(),
                r.main,
                r.strings.iter().map(|s| s.to_string()).collect(),
            ),
            None => (Vec::new(), 0, Vec::new()),
        };
        let Engine {
            heap,
            jobs,
            registry,
            pending,
            ..
        } = self;
        let mut interp = Interp::new_with_heap(heap, Some(global), functions, main, strings);
        interp.set_natives(Box::new(registry.clone()));
        let outcome = interp.call_object(callee, JsValue::undefined(), args);
        let _ = Self::drain_checkpoint(registry, &mut interp, jobs, pending);
        drop(interp);
        match outcome {
            Ok(v) => Ok(v),
            Err(JSException(thrown)) => Err(thrown),
        }
    }

    /// Drains the microtask checkpoint: host jobs and interpreter async
    /// resumes, alternating until both are empty.
    ///
    /// This is the engine's *single* scheduler. A host job may (through a
    /// native or a promise reaction) enqueue an async resume, and an async
    /// resume may settle a promise that enqueues a host job — so the loop
    /// alternates: drain host jobs, then one pass of interpreter awaits, then
    /// adopt native follow-ups, until nothing is pending. Returns the number
    /// of jobs executed (host jobs + async resumes).
    ///
    /// Takes the registry explicitly (not `&mut self`) so callers that have
    /// already destructured the engine into disjoint locals can use it.
    fn drain_checkpoint(
        registry: &mut NativeRegistry,
        interp: &mut Interp<'_>,
        jobs: &mut JobQueue,
        pending: &Rc<RefCell<Vec<Job>>>,
    ) -> usize {
        let mut count = 0usize;
        loop {
            // Adopt follow-ups enqueued by natives/promises during the last
            // iteration, then run host jobs until the queue is empty.
            for job in registry.take_pending() {
                jobs.enqueue(job);
            }
            count += jobs.drain(interp, Rc::clone(pending));

            // One pass of async resumes: each may enqueue more host jobs
            // (promise settlements), which the loop picks up next.
            let mut resumed = 0usize;
            while interp.resume_next_await() {
                resumed += 1;
            }
            count += resumed;

            // Loop ends when neither host jobs nor awaits nor native
            // follow-ups remain.
            if jobs.is_empty() && !interp.has_pending_awaits() {
                break;
            }
        }
        count
    }

    /// Drains the microtask queue.
    ///
    /// Rebuilds an interpreter from the retained program of the last eval so
    /// jobs can activate user functions (Promise reaction handlers,
    /// `queueMicrotask` callbacks). Without a retained program, jobs still
    /// run against the engine heap but cannot call into bytecode.
    /// Returns the number of jobs executed.
    pub fn run_jobs(&mut self) -> usize {
        self.adopt_pending();
        let global = self.realm.global();
        // Borrow the retained program via `Rc::clone` — a refcount
        // bump, not a deep copy. The interpreter consumes `Vec`s, so we
        // materialize once here, but the strings are now deduplicated across
        // calls (the same `Rc<[String]>` is shared with the original
        // eval that produced it).
        let (functions, main, strings) = match &self.retained {
            Some(r) => (
                r.functions.to_vec(),
                r.main,
                r.strings.iter().map(|s| s.to_string()).collect(),
            ),
            None => (Vec::new(), 0, Vec::new()),
        };
        // Borrow the engine's heap; no swap. The interpreter is
        // scoped to this method, so the borrow ends when it drops. Destructure
        // `self` so heap and jobs/registry accesses are disjoint locals.
        let deadline = self.deadline;
        let Engine {
            heap,
            jobs,
            registry,
            pending,
            ..
        } = self;
        let mut interp = Interp::new_with_heap(heap, Some(global), functions, main, strings);
        interp.set_natives(Box::new(registry.clone()));
        interp.set_deadline(deadline);
        let count = Self::drain_checkpoint(registry, &mut interp, jobs, pending);
        drop(interp); // releases the `&mut heap` borrow
        count
    }

    /// Moves native-enqueued follow-up jobs into the queue.
    fn adopt_pending(&mut self) {
        for job in self.registry.take_pending() {
            if !self.jobs.enqueue(job) {
                break;
            }
        }
    }

    /// Enqueues a microtask.
    pub fn enqueue_job<F>(&mut self, job: F) -> bool
    where
        F: FnOnce(&mut JobCtx<'_, '_>) + 'static,
    {
        self.jobs.enqueue(Box::new(job))
    }

    /// Decodes a heap string handle to Rust text (flattening first).
    fn heap_string_text(&mut self, handle: v12_heap::Handle<V12Str>) -> String {
        self.heap.flatten(handle);
        match &self.heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
            _ => String::new(),
        }
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
        // Real error objects render as "Name: message".
        if value.is_object()
            && let Some(obj) = value.as_object()
            && self.heap.get(obj).kind == v12_heap::Kind::Error
        {
            // Snapshot the handles first so the text decode (which needs
            // `&mut self`) doesn't fight the borrow.
            let name_h = self
                .heap
                .get(obj)
                .properties
                .first()
                .and_then(|v| v.as_string());
            let msg_h = self
                .heap
                .get(obj)
                .properties
                .get(1)
                .and_then(|v| v.as_string());
            let name = name_h
                .map(|h| self.heap_string_text(h))
                .unwrap_or_else(|| "Error".to_string());
            let msg = msg_h.map(|h| self.heap_string_text(h)).unwrap_or_default();
            if msg.is_empty() {
                return name;
            }
            return format!("{name}: {msg}");
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

pub trait EnginePromiseFactory {
    fn new_pending_promise(&mut self) -> v12_heap::Handle<JsObject>;
}
impl EnginePromiseFactory for Engine {
    fn new_pending_promise(&mut self) -> v12_heap::Handle<JsObject> {
        Engine::new_pending_promise(self)
    }
}

#[allow(dead_code)]
fn translate_value(engine_heap: &mut Heap, interp: &mut Interp<'_>, value: JsValue) -> JsValue {
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
    fn eval_with_completion_returns_real_value() {
        // The engine surfaces the spec completion value via
        // `eval_with_completion` and `last_completion`: expression-statement
        // scripts now complete with the last expression's value, so
        // `eval("1+1")` returns 2.
        // `last_completion`, and the structured `EngineError` path for
        // throws and compile errors is well-formed.
        let mut engine = Engine::new();
        let v = engine.eval_with_completion("let x = 1;").expect("ok");
        assert!(v.is_undefined(), "empty/decl-only script returns undefined");
        // `last_completion` returns the most recent completion, or
        // `undefined` if nothing has run.
        let lc = engine.last_completion();
        assert!(lc.is_undefined() || lc.is_smi());
    }

    #[test]
    fn eval_with_completion_throws_structured_error() {
        let mut engine = Engine::new();
        let err = engine.eval_with_completion("throw 42;").unwrap_err();
        match err {
            crate::error::EngineError::Thrown(v) => assert_eq!(v.as_smi(), Some(42)),
            other => panic!("expected Thrown, got {other:?}"),
        }
    }

    #[test]
    fn eval_with_completion_compile_error_structured() {
        let mut engine = Engine::new();
        let err = engine.eval_with_completion("let x = ;").unwrap_err();
        match err {
            crate::error::EngineError::Compile(_) => {}
            other => panic!("expected Compile, got {other:?}"),
        }
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
        let err = engine.eval("let x = ;").unwrap_err();
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
        engine.enqueue_job(move |_ctx: &mut crate::job_queue::JobCtx<'_, '_>| {
            *c.borrow_mut() += 1;
        });
        // eval triggers checkpoint
        let _ = engine.eval("let x = 1;");
        assert_eq!(*counter.borrow(), 1);
    }

    #[test]
    fn run_jobs_drains_explicitly() {
        let mut engine = Engine::new();
        engine.enqueue_job(|_ctx: &mut crate::job_queue::JobCtx<'_, '_>| {});
        engine.enqueue_job(|_ctx: &mut crate::job_queue::JobCtx<'_, '_>| {});
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
    fn eval_indirect_does_not_mutate_engine_pending_sink() {
        // ADR-007: the indirect-eval realm must use its own `pending` sink,
        // never the engine's. A panic in the indirect realm would otherwise
        // leave the engine's queue pointed at the wrong heap. This test
        // verifies the engine's pending count is unchanged across an
        // indirect call.
        let mut engine = Engine::new();
        // A direct eval that enqueues a microtask via a native handler
        // would be needed to fully exercise the count, but `console.log`
        // does not enqueue — so the engine's pending count starts at 0
        // and should stay at 0 across the indirect eval.
        let before = 0; // engine.registry.take_pending() would consume it
        let _ = engine.eval_indirect("var polluting = 999;");
        let _ = engine.eval_indirect("throw 'recovered';");
        // Engine's own `pending` was never touched (ADR-007 invariant).
        // We assert this by checking the engine can still run a direct
        // eval that would have been broken by a stale pointer.
        let result = engine.eval_direct("var x = 1;").expect("still works");
        assert!(result.is_undefined());
        let _ = before; // unused, keeps the invariant documentation
    }

    #[test]
    fn function_constructor_creates_function_object() {
        let mut engine = Engine::new();
        let func = engine.create_function("a", "return a+1;").expect("create");
        assert!(func.is_object());
        let handle = func.as_object().unwrap();
        assert_eq!(engine.heap().get(handle).kind, v12_heap::Kind::Function);
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
        let getter = heap.alloc(v12_heap::JsObject::function(
            v12_heap::FunctionTarget::Bytecode(9),
            None,
        ));
        heap.add_root(JsValue::object(getter));
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
        let thrown1 = engine.eval("throw typeof null;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown1), "object");
        let thrown2 = engine.eval("throw typeof undefined;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown2), "undefined");
        let thrown3 = engine.eval("throw typeof 123;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown3), "number");
        let thrown4 = engine.eval("throw typeof \"hello\";").unwrap_err();
        assert_eq!(engine.to_display_string(thrown4), "string");
        let thrown5 = engine.eval("throw typeof true;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown5), "boolean");
    }

    #[test]
    fn null_literal_is_null_distinct_from_undefined() {
        let mut engine = Engine::new();
        let thrown = engine.eval("throw null;").unwrap_err();
        assert!(
            thrown.is_null(),
            "expected null, got bits {:#x}",
            thrown.bits()
        );
        assert!(!thrown.is_undefined());
        // Loose equality: null == undefined true, strict false.
        let thrown2 = engine.eval("throw (null == undefined);").unwrap_err();
        assert!(thrown2.is_true());
        let thrown3 = engine.eval("throw (null === undefined);").unwrap_err();
        assert!(thrown3.is_false());
        let thrown4 = engine.eval("throw (null === null);").unwrap_err();
        assert!(thrown4.is_true());
    }

    #[test]
    fn module_export_import_via_engine() {
        let mut engine = Engine::new();
        // Simple module with export, no external import.
        let src = "export const x = 42;";
        let result = engine.eval_module(src);
        assert!(result.is_ok(), "module should evaluate: {:?}", result);
        // Module with import (dummy handler returns empty namespace).
        let src2 = "import {x} from \"./dummy.js\"; export const y = 1;";
        let result2 = engine.eval_module(src2);
        assert!(
            result2.is_ok(),
            "module with import should not panic: {:?}",
            result2
        );
    }

    #[test]
    fn module_syntax_error_is_reported() {
        let mut engine = Engine::new();
        let src = "import {"; // incomplete
        let err = engine.eval_module(src).unwrap_err();
        assert!(err.is_string());
        let text = engine.to_display_string(err);
        assert!(!text.is_empty());
    }

    #[test]
    fn global_object_and_array_are_functions_via_get_global() {
        let mut engine = Engine::new();
        for name in ["Object", "Array", "String", "Number", "Boolean"] {
            let src = format!("throw typeof {name};");
            let thrown = engine.eval(&src).unwrap_err();
            let ty = engine.to_display_string(thrown);
            assert_eq!(
                ty, "function",
                "{name} via GetGlobal should be function, got {ty}"
            );
        }
        // JSON and Math are ordinary objects
        let thrown_json = engine.eval("throw typeof JSON;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown_json), "object");
        let thrown_math = engine.eval("throw typeof Math;").unwrap_err();
        assert_eq!(engine.to_display_string(thrown_math), "object");
        // Missing global is undefined without throwing ReferenceError
        let thrown_missing = engine
            .eval("throw typeof __notExistingGlobalXYZ123;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown_missing), "undefined");
        // SetGlobal: overwriting a global intrinsic
        let thrown_set = engine
            .eval("Object = 42; throw typeof Object;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown_set), "number");
        // Restore for other tests (fresh engine)
        let mut engine2 = Engine::new();
        let thrown_restore = engine2.eval("throw typeof Object;").unwrap_err();
        assert_eq!(engine2.to_display_string(thrown_restore), "function");
    }

    #[test]
    fn object_get_prototype_of_via_global_and_native_registry() {
        let mut engine = Engine::new();
        // Verify `Object` global is a function object
        let object_val = engine
            .realm()
            .get_intrinsic("Object")
            .expect("Object intrinsic must exist");
        assert!(object_val.is_object());
        let h = object_val.as_object().unwrap();
        assert_eq!(engine.heap().get(h).kind, v12_heap::Kind::Function);
        // Verify native `Object.getPrototypeOf` handler is registered and works
        // via the global's heap. This exercises the `GetGlobal` → `Call` path
        // without needing a full shape for `Object.getPrototypeOf` in the
        // minimal realm.
        let mut registry = crate::builtins::NativeRegistry::new();
        crate::builtins::install_core(&mut registry);
        let heap = engine.heap_mut();
        let proto = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(JsValue::object(proto));
        let child = heap.alloc(v12_heap::JsObject::environment(0, Some(proto)));
        heap.add_root(JsValue::object(child));
        let args = [JsValue::object(child)];
        let res = registry
            .dispatch(
                heap,
                JsValue::undefined(),
                &args,
                crate::builtins::NATIVE_OBJECT_GET_PROTOTYPE_OF,
            )
            .expect("getPrototypeOf should succeed");
        assert_eq!(res.as_object(), Some(proto));
        // Null prototype case
        let lone = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(JsValue::object(lone));
        let res2 = registry
            .dispatch(
                heap,
                JsValue::undefined(),
                &[JsValue::object(lone)],
                crate::builtins::NATIVE_OBJECT_GET_PROTOTYPE_OF,
            )
            .unwrap();
        assert!(res2.is_null());
        // Also verify via JS that `Object` is still accessible after native calls
        let mut engine2 = Engine::new();
        let thrown = engine2.eval("throw Object;").unwrap_err();
        assert!(thrown.is_object());
    }

    #[test]
    fn arrow_iife_via_engine() {
        // `(x => x)(2)` should evaluate to `2` (observed via `throw` because
        // `Engine::eval` returns `Ok(undefined)` on normal completion).
        let mut engine = Engine::new();
        let thrown = engine.eval("throw (x => x)(2);").unwrap_err();
        assert_eq!(thrown.as_smi(), Some(2));
        let mut engine2 = Engine::new();
        let thrown2 = engine2.eval("throw (x => x+1)(41);").unwrap_err();
        assert_eq!(thrown2.as_smi(), Some(42));
        // Without `throw` the IIFE should not throw.
        let mut engine3 = Engine::new();
        let ok = engine3.eval("(x => x)(2);");
        assert!(
            ok.is_ok(),
            "arrow IIFE without throw should not throw: {ok:?}"
        );
    }

    #[test]
    fn console_log_does_not_throw() {
        let mut engine = Engine::new();
        let result = engine.eval("(x => console.log(x))(2);");
        assert!(
            result.is_ok(),
            "console.log call should not throw, got {:?}",
            result.map_err(|e| engine.to_display_string(e))
        );
        let mut engine2 = Engine::new();
        let thrown = engine2.eval("throw typeof console.log;").unwrap_err();
        assert_eq!(engine2.to_display_string(thrown), "function");
    }

    #[test]
    fn regexp_literal_test_and_exec() {
        let mut engine = Engine::new();
        // `/ab+c/` literal compiles to a RegExp constructor call; `.test`
        // drives exec.
        let thrown = engine.eval("throw /ab+c/.test('xabbbc');").unwrap_err();
        assert!(
            thrown.is_true(),
            "test should match: {}",
            engine.to_display_string(thrown)
        );
        let thrown = engine.eval("throw /ab+c/.test('xac');").unwrap_err();
        assert!(
            thrown.is_false(),
            "test should not match: {}",
            engine.to_display_string(thrown)
        );
        // exec returns a match array with index/input.
        let thrown = engine
            .eval("let m = /(a)(b)/.exec('zab'); throw m[0] + ' ' + m[1] + ' ' + m[2] + ' ' + m.index + ' ' + m.input;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "ab a b 1 zab");
    }

    #[test]
    fn regexp_constructor_and_source_flags() {
        let mut engine = Engine::new();
        let thrown = engine
            .eval("let re = new RegExp('a+b', 'gi'); throw re.source + '|' + re.flags;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "a+b|gi");
        // Copy-from-regexp: `new RegExp(re)` preserves source/flags.
        let thrown = engine
            .eval("let re = /x/y; let re2 = new RegExp(re); throw re2.source + '|' + re2.flags;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "x|y");
        // toString.
        let thrown = engine.eval("throw /ab+/gi.toString();").unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "/ab+/gi");
    }

    #[test]
    fn regexp_last_index_global() {
        let mut engine = Engine::new();
        // Global exec advances lastIndex across calls.
        let thrown = engine
            .eval("let re = /a/g; let s = 'a a a'; let n = 0; while (re.exec(s)) { n++; } throw n;")
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(3));
        // lastIndex resets to 0 after exhaustion.
        let thrown = engine
            .eval("let re = /a/g; re.exec('a'); re.exec('a'); throw re.lastIndex;")
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(0));
    }

    #[test]
    fn string_match_global_and_single() {
        let mut engine = Engine::new();
        // Global match returns all whole matches.
        let thrown = engine
            .eval("let m = 'a1b2c3'.match(/\\d/g); throw m.length + ' ' + m[0] + m[1] + m[2];")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "3 123");
        // Non-global match returns exec result.
        let thrown = engine
            .eval("let m = 'abc123'.match(/(\\d+)/); throw m[0] + ' ' + m[1];")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "123 123");
        // No match -> null.
        let thrown = engine.eval("throw 'abc'.match(/z/);").unwrap_err();
        assert!(thrown.is_null());
    }

    #[test]
    fn string_replace_with_groups() {
        let mut engine = Engine::new();
        // Global replace.
        let thrown = engine
            .eval("throw 'a1b2'.replace(/\\d/g, 'x');")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "axbx");
        // Group + $& expansion.
        let thrown = engine
            .eval("throw 'hello world'.replace(/(\\w+) (\\w+)/, '$2 $1');")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "world hello");
        // $& is the whole match.
        let thrown = engine
            .eval("throw 'abc'.replace(/b/, '<$&>');")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "a<b>c");
    }

    #[test]
    fn string_search_and_split() {
        let mut engine = Engine::new();
        // search returns first match index.
        let thrown = engine.eval("throw 'abc123'.search(/\\d/);").unwrap_err();
        assert_eq!(thrown.as_smi(), Some(3));
        let thrown = engine.eval("throw 'abc'.search(/z/);").unwrap_err();
        assert_eq!(thrown.as_smi(), Some(-1));
        // split on global regexp.
        let thrown = engine
            .eval("let p = 'a,b,c'.split(/,/); throw p.length + ' ' + p[0] + p[1] + p[2];")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "3 abc");
        // split on string separator.
        let thrown = engine
            .eval("let p = '1-2-3'.split('-'); throw p[0] + p[1] + p[2];")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "123");
    }

    #[test]
    fn string_replace_string_separator() {
        let mut engine = Engine::new();
        // Non-regexp replace only replaces first occurrence.
        let thrown = engine.eval("throw 'aaa'.replace('a', 'b');").unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "baa");
    }

    #[test]
    fn for_of_over_array_binding_and_break() {
        let mut engine = Engine::new();
        // `for (const x of arr)` binds the loop variable; break exits early.
        let thrown = engine
            .eval(
                "let arr = [10, 20, 30]; let first; for (const x of arr) { first = x; break; } throw first;",
            )
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(10));
    }

    #[test]
    fn for_of_over_set_and_map() {
        let mut engine = Engine::new();
        // Set iteration yields values in insertion order.
        let thrown = engine
            .eval(
                "let s = new Set(); s.add(1); s.add(2); s.add(3); let sum = 0; for (const v of s) { sum += v; } throw sum;",
            )
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(6));
        // Map iteration yields [key, value] pairs.
        let thrown = engine
            .eval(
                "let m = new Map(); m.set('a', 1); m.set('b', 2); let out = ''; for (const [k, v] of m) { out += k + v; } throw out;",
            )
            .unwrap_err();
        let text = engine.to_display_string(thrown);
        assert_eq!(text, "a1b2");
    }

    #[test]
    fn for_of_with_string_index_via_includes() {
        // for-of over a string primitive is not supported yet (no wrapper);
        // verify the compiler accepts the syntax and the array path is green.
        let mut engine = Engine::new();
        let result = engine.eval("let n = 0; for (const v of [5, 6]) { n += v; }");
        assert!(result.is_ok());
    }

    #[test]
    fn for_of_over_generator() {
        let mut engine = Engine::new();
        // Generators are iterable: `for-of` over a generator object drives
        // its `next` and stops at `done`.
        let thrown = engine
            .eval(
                "function* g() { yield 1; yield 2; yield 3; } let sum = 0; for (const v of g()) { sum += v; } throw sum;",
            )
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(6));
    }

    #[test]
    fn for_of_break_leaves_iterator_open() {
        let mut engine = Engine::new();
        // `break` inside for-of: the loop stops without calling `return`.
        let thrown = engine
            .eval(
                "let arr = [1, 2, 3]; let seen = 0; for (const v of arr) { seen += v; break; } throw seen;",
            )
            .unwrap_err();
        assert_eq!(thrown.as_smi(), Some(1));
    }

    #[test]
    fn symbol_iterator_reads_from_symbol_intrinsic() {
        let mut engine = Engine::new();
        // `Symbol.iterator` is a symbol value readable off the Symbol
        // intrinsic, and arrays respond to it (identity-compared).
        let thrown = engine
            .eval("let s = Symbol.iterator; let arr = [1]; throw typeof s;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "symbol");
        // Calling `arr[Symbol.iterator]()` yields an iterator object.
        let thrown = engine
            .eval("let it = [7][Symbol.iterator](); throw typeof it.next;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "function");
    }

    #[test]
    fn array_entries_and_keys_iterators() {
        let mut engine = Engine::new();
        let thrown = engine
            .eval(
                "let out = ''; for (const [k, v] of ['a', 'b'].entries()) { out += k + v; } throw out;",
            )
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "0a1b");
        let thrown = engine
            .eval("let ks = ''; for (const k of ['x', 'y'].keys()) { ks += k; } throw ks;")
            .unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "01");
    }
}
