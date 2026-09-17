//! Evaluation and scheduling: script/module/indirect eval entry points, the
//! shared `run_compiled` driver, and the microtask checkpoint drain.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use v12_bytecode::FunctionBytecode;
use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue};
use v12_interp::{Interp, JSException};

use super::{Engine, MAX_SOURCE_LEN, RetainedProgram, string_value};
use crate::builtins::{NativeRegistry, promise};
use crate::error::EngineError;
#[cfg(feature = "jit")]
use crate::jit_tier;
use crate::job_queue::{Job, JobQueue};
use crate::realm::Realm;

impl Engine {
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
        self.eval_inner(source).map_err(|e| self.error_to_value(e))
    }

    /// Private inner: compiles + runs + captures completion, returning a
    /// structured [`EngineError`]. Both [`Self::eval_direct`] (legacy) and
    /// [`Self::eval_with_completion`] are thin adapters over this.
    pub(crate) fn eval_inner(&mut self, source: &str) -> Result<JsValue, EngineError> {
        if source.len() > MAX_SOURCE_LEN {
            return Err(EngineError::Host("source too large".into()));
        }
        // Top-level script: dynamic `import()` resolves against the engine's
        // module base (the runner points it at the executing file's dir).
        let base = self.module_base.clone();
        self.prime_loader(base);
        let global = self.realm.global();
        self.heap.add_root(JsValue::object(global));
        let (program, strings) =
            v12_bccompiler::compile_source_with_strings(source).map_err(EngineError::Compile)?;
        // Wrap the tables in `Rc` once; subsequent `eval`/`run_jobs` clone
        // the `Rc` instead of deep-cloning the function/string tables.
        let functions: Rc<[FunctionBytecode]> = Rc::from(program.functions.into_boxed_slice());
        let strings: Rc<[String]> = Rc::from(strings.into_boxed_slice());
        let natives = Box::new(self.registry.clone());
        self.run_compiled(global, functions, program.main, strings, natives)
    }

    /// Flattens a structured [`EngineError`] into the legacy thrown-`JsValue`
    /// representation: thrown values pass through, host/compile errors become
    /// interned string values.
    pub(crate) fn error_to_value(&mut self, err: EngineError) -> JsValue {
        match err {
            EngineError::Thrown(t) => t,
            EngineError::Host(msg) => string_value(&mut self.heap, &msg),
            EngineError::Compile(err) => string_value(&mut self.heap, &err.message),
        }
    }

    /// Constructs the checkpoint interpreter shared by script eval and module
    /// eval: installs natives (which carry the shared loader/pending state),
    /// applies JIT hooks, sets the cooperative deadline, and adopts the
    /// engine's shared cross-program table (registering this program into it
    /// so its id survives interpreter rebuilds across checkpoint drains).
    fn make_interp<'a>(
        heap: &'a mut Heap,
        global: Handle<JsObject>,
        functions: Rc<[FunctionBytecode]>,
        main: u32,
        strings: Rc<[String]>,
        natives: Box<dyn v12_interp::NativeRegistry>,
        deadline: Option<std::time::Instant>,
        programs: Rc<RefCell<Vec<v12_native::ProgramTable>>>,
    ) -> Interp<'a> {
        #[cfg(feature = "jit")]
        let jit_program = Rc::clone(&functions);
        let mut interp = Interp::new_with_heap(heap, Some(global), functions, main, strings);
        interp.set_natives(natives);
        interp.adopt_shared_programs(programs);
        #[cfg(feature = "jit")]
        jit_tier::JitTierHooks::install_if_enabled(&mut interp, &jit_program);
        interp.set_deadline(deadline);
        interp
    }

    /// Runs a compiled program in a fresh interpreter borrowing the engine
    /// heap: retains the program so queued jobs can rebuild an interpreter
    /// later, installs `natives`, runs, drains the single microtask
    /// checkpoint, and captures the completion value. Shared by script eval
    /// ([`Self::eval_inner`]) and module eval.
    pub(crate) fn run_compiled(
        &mut self,
        global: Handle<JsObject>,
        functions: Rc<[FunctionBytecode]>,
        main: u32,
        strings: Rc<[String]>,
        natives: Box<dyn v12_interp::NativeRegistry>,
    ) -> Result<JsValue, EngineError> {
        self.retained = Some(RetainedProgram {
            functions: Rc::clone(&functions),
            main,
            strings: Rc::clone(&strings),
        });
        let deadline = self.deadline;
        // Borrow the engine's heap for the interpreter's lifetime —
        // no `mem::replace` swap, no sentinel heap, `Engine::heap()` stays
        // valid the whole time the interpreter runs. Destructure `self` so
        // the heap borrow and the job-queue/registry accesses are disjoint
        // locals (the borrow checker cannot see field disjointness through
        // `&mut self`).
        let Engine {
            heap,
            jobs,
            registry,
            pending,
            completion,
            programs,
            ..
        } = self;
        let mut interp = Self::make_interp(
            heap,
            global,
            functions,
            main,
            strings,
            natives,
            deadline,
            Rc::clone(programs),
        );
        let outcome = interp.run();
        // Drain the single microtask checkpoint against the still-live
        // interpreter: host jobs and async resumes alternate until empty.
        let _ = Self::drain_checkpoint(registry, &mut interp, jobs, pending);
        // Capture the actual completion value (e.g. `1+1` -> 2).
        *completion = interp.completion_value();
        drop(interp); // releases the `&mut heap` borrow
        match outcome {
            Ok(()) => Ok(completion.unwrap_or_else(JsValue::undefined)),
            Err(JSException(thrown)) => Err(EngineError::Thrown(thrown)),
        }
    }

    /// Indirect `eval`: fresh global scope (new heap + global).
    ///
    /// `var` declarations in `source` do **not** affect the caller's global.
    pub fn eval_indirect(&mut self, source: &str) -> Result<JsValue, JsValue> {
        if source.len() > MAX_SOURCE_LEN {
            return Err(string_value(&mut self.heap, "RangeError: source too large"));
        }
        // Fresh heap + realm for the indirect eval.
        let mut heap = Heap::new(GcPolicy::default());
        let realm = Realm::new(&mut heap);
        let global = realm.global();
        heap.add_root(JsValue::object(global));
        let (program, strings) = v12_bccompiler::compile_source_with_strings(source)
            .map_err(|err| string_value(&mut heap, &err.message))?;
        // The indirect-eval gets its OWN `NativeRegistry` with its
        // OWN pending sink, so jobs enqueued in this realm never reach the
        // engine's queue and no `set_pending` save/restore is needed. The
        // engine's `self.registry` is left untouched for the whole call.
        // `NativeRegistry` is `Clone`, so the local registry starts as a full
        // copy of the engine's (builtins + host functions).
        let mut local_registry = self.registry.clone();
        local_registry.set_pending(Rc::new(RefCell::new(Vec::new())));
        let functions = Rc::from(program.functions);
        let mut interp = Interp::new_with_heap(
            &mut heap,
            Some(global),
            Rc::clone(&functions),
            program.main,
            strings,
        );
        interp.set_natives(Box::new(local_registry.clone()));
        #[cfg(feature = "jit")]
        jit_tier::JitTierHooks::install_if_enabled(&mut interp, &functions);
        #[cfg(feature = "jit")]
        jit_tier::JitTierHooks::install_if_enabled(&mut interp, &functions);
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
                    // Read the text from the fresh heap's string, intern in caller.
                    let text = interp.to_display_string(thrown);
                    let _ = h;
                    Err(string_value(&mut self.heap, &text))
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
    ///
    /// The static import graph is resolved, loaded, and evaluated *before*
    /// this module's body runs (post-order DFS; see
    /// [`crate::module_loader`]): each dependency's namespace snapshot lands
    /// in the shared module map, so the lowered static-import native calls in
    /// the body answer synchronously. Dynamic `import()` calls inside module
    /// code go through the loader's promise/job path.
    pub fn eval_module_source(&mut self, source: &str, base: &Path) -> Result<JsValue, JsValue> {
        self.eval_module_source_at(source, base, None)
    }

    /// Like [`Self::eval_module_source`], but registers the entry module's
    /// namespace under `entry_path` before evaluating its static imports.
    ///
    /// Required for a self-import (`import * as ns from './self.js'`) or a
    /// cycle back to the entry: without entry registration the importer
    /// re-reads the raw file from disk (bypassing any harness-prepended
    /// source) and gets a second, unrelated namespace.
    pub fn eval_module_source_at(
        &mut self,
        source: &str,
        base: &Path,
        entry_path: Option<&Path>,
    ) -> Result<JsValue, JsValue> {
        if source.len() > MAX_SOURCE_LEN {
            return Err(string_value(&mut self.heap, "RangeError: source too large"));
        }
        self.prime_loader(base.to_path_buf());
        // Register the entry namespace under its own path so a self-import or
        // cyclic importer resolves to it. `resolve_specifier(".", abs)` just
        // normalizes the absolute path.
        let entry_ns = entry_path.map(|ep| {
            let ns = crate::module_namespace::alloc_namespace(&mut self.heap);
            if let Some(loader) = self.registry.loader() {
                let canonical = crate::module_loader::resolve_specifier(
                    Path::new("."),
                    &ep.to_string_lossy(),
                );
                loader
                    .borrow_mut()
                    .modules
                    .insert(canonical, JsValue::object(ns));
            }
            ns
        });
        let global = self.realm.global();
        self.heap.add_root(JsValue::object(global));
        // Compile as module.
        let mut interner = v12_bccompiler::Interner::default();
        let module = v12_bccompiler::compile_source_as_module_with_interner(source, &mut interner)
            .map_err(|err| string_value(&mut self.heap, &err.message))?;
        // Seed the entry's export keys before its dependencies evaluate: a
        // self-import during the body must observe the export list.
        if let Some(ns) = entry_ns {
            let names: Vec<String> = module.exports.iter().map(|e| e.exported.clone()).collect();
            crate::module_namespace::seed_export_keys(&mut self.heap, ns, &names);
        }
        let strings: Vec<String> = v12_bccompiler::freeze_interner(interner)
            .iter()
            .map(|(_, s)| s.to_string())
            .collect();
        let program = module.program;
        let functions: Rc<[FunctionBytecode]> = Rc::from(program.functions.into_boxed_slice());
        let strings: Rc<[String]> = Rc::from(strings.into_boxed_slice());
        self.retained = Some(RetainedProgram {
            functions: Rc::clone(&functions),
            main: program.main,
            strings: Rc::clone(&strings),
        });
        let deadline = self.deadline;
        let natives = Box::new(self.registry.clone());
        let Engine {
            heap,
            jobs,
            registry,
            pending,
            completion,
            programs,
            ..
        } = self;
        let mut interp = Self::make_interp(
            heap,
            global,
            functions,
            program.main,
            strings,
            natives,
            deadline,
            Rc::clone(programs),
        );
        // Pre-evaluate the static import graph on this interpreter (the
        // dynamic-import path is runtime-driven and does not need it).
        if let Some(loader) = registry.loader() {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for entry in &module.imports {
                if seen.insert(entry.specifier.clone()) {
                    let child = crate::module_loader::resolve_specifier(base, &entry.specifier);
                    if let Err(reason) =
                        crate::module_loader::load_and_evaluate(&mut interp, &loader, &child)
                    {
                        drop(interp); // releases the `&mut heap` borrow
                        return Err(reason);
                    }
                }
            }
        }
        let outcome = interp.run();
        // Drain the single microtask checkpoint against the still-live
        // interpreter: host jobs and async resumes alternate until empty.
        let _ = Self::drain_checkpoint(registry, &mut interp, jobs, pending);
        *completion = interp.completion_value();
        drop(interp); // releases the `&mut heap` borrow
        // Fill the entry namespace from the module epilogue's exports object
        // (the completion value) so self-importers read real exports.
        if let Some(ns) = entry_ns
            && let Some(exports) = (*completion).and_then(|v| v.as_object())
        {
            crate::module_namespace::populate_namespace(heap, ns, exports);
        }
        match outcome {
            Ok(()) => Ok(completion.unwrap_or_else(JsValue::undefined)),
            Err(JSException(thrown)) => Err(thrown),
        }
    }

    /// Evaluates a module file at `path`, resolving imports relative to its directory.
    pub fn eval_module_file(&mut self, path: &Path) -> Result<JsValue, JsValue> {
        let source = std::fs::read_to_string(path).map_err(|e| {
            let msg = format!("Error reading {}: {e}", path.display());
            string_value(&mut self.heap, &msg)
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
    pub(crate) fn drain_checkpoint(
        registry: &mut NativeRegistry,
        interp: &mut Interp<'_>,
        jobs: &mut JobQueue,
        pending: &Rc<RefCell<Vec<Job>>>,
    ) -> usize {
        let mut count = 0usize;
        loop {
            // Settle async-function completions queued by the interpreter:
            // each one settles its completion promise through the full
            // capability/reaction path (spec 27.7.5.1 / AsyncFunctionAwait).
            // Reaction jobs join the shared sink and are adopted below.
            let settlements = interp.take_pending_settlements();
            count += settlements.len();
            for (promise, value, rejecting) in settlements {
                promise::settle_async_completion(
                    interp.heap_mut(),
                    pending,
                    promise,
                    value,
                    rejecting,
                );
            }
            // Adopt follow-ups enqueued by natives/promises during the last
            // iteration, then run host jobs until the queue is empty.
            let follow_ups = registry.take_pending();
            let adopted = follow_ups.len();
            for job in follow_ups {
                jobs.enqueue(job);
            }
            let drained = jobs.drain(interp, Rc::clone(pending));
            count += drained;

            // One pass of async resumes: each queued await is attempted once;
            // awaits parked on still-pending promises re-queue and retry after
            // more host jobs run. Each resume may enqueue more host jobs
            // (promise settlements), which the loop picks up next.
            let mut resumed = 0usize;
            let attempts = interp.pending_jobs();
            for _ in 0..attempts {
                if interp.resume_next_await() {
                    resumed += 1;
                }
            }
            count += resumed;

            // Loop ends when neither host jobs nor awaits nor native
            // follow-ups nor async settlements remain, when the deadline
            // fired mid-drain (remaining microtask bodies can never complete
            // within the budget; their `execute` will re-trip the deadline),
            // or when the pass was quiescent: adopted nothing, drained
            // nothing, resumed nothing, settled nothing — the only remaining
            // awaits are parked on promises that (without timers or external
            // resolution) can never settle.
            if interp.is_deadline_exceeded()
                || (jobs.is_empty()
                    && !interp.has_pending_awaits()
                    && !interp.has_pending_settlements())
                || (adopted == 0
                    && drained == 0
                    && resumed == 0
                    && !interp.has_pending_settlements())
            {
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
            Some(r) => (Rc::clone(&r.functions), r.main, Rc::clone(&r.strings)),
            None => (
                Rc::<[FunctionBytecode]>::from(Vec::new()),
                0,
                Rc::<[String]>::from(Vec::new()),
            ),
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
            programs,
            ..
        } = self;
        #[cfg(feature = "jit")]
        let jit_program = Rc::clone(&functions);
        let mut interp = Interp::new_with_heap(heap, Some(global), functions, main, strings);
        interp.set_natives(Box::new(registry.clone()));
        // Adopt the engine's shared cross-program table and register the
        // retained program under a fresh id: async resumes and reaction jobs
        // may close over programs registered during earlier evals (imports,
        // eval'd sources), which a fresh per-interp table would lose — the
        // silent fallback then runs foreign bytecode in a wrong register
        // window (register-window OOB panic class).
        interp.adopt_shared_programs(Rc::clone(programs));
        #[cfg(feature = "jit")]
        jit_tier::JitTierHooks::install_if_enabled(&mut interp, &jit_program);
        interp.set_deadline(deadline);
        let count = Self::drain_checkpoint(registry, &mut interp, jobs, pending);
        drop(interp); // releases the `&mut heap` borrow
        count
    }

    /// Moves native-enqueued follow-up jobs into the queue.
    pub(crate) fn adopt_pending(&mut self) {
        for job in self.registry.take_pending() {
            if !self.jobs.enqueue(job) {
                break;
            }
        }
    }
}
