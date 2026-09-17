//! ES module loader: specifier resolution, module-graph loading/linking, and
//! evaluation.
//!
//! ## Shape
//!
//! [`LoaderState`] is the module map: canonical path → namespace snapshot.
//! It lives in an `Rc<RefCell<…>>` owned by the engine's
//! [`NativeRegistry`](crate::builtins::NativeRegistry), so the `ModuleImport`
//! native — dispatched from arbitrary interpreter contexts — and the
//! engine/job-side [`load_and_evaluate`] driver see the same state.
//!
//! ## Static imports (module code)
//!
//! `eval_module_source` (and dynamic-import jobs) pre-evaluate the whole
//! static import graph *before* the importing module's body runs:
//! post-order DFS, each dependency's main executes on the live interpreter
//! via [`v12_interp::Interp::call_program_main`], which registers the module
//! program in the cross-program table (exported functions stay callable from
//! other programs). The compiled main returns its exports object (the
//! compiler emits an epilogue building one), which becomes the namespace
//! snapshot stored in the map. When the importing module's body later runs
//! its lowered `import(specifier)` native call, the loader resolves the
//! specifier against the *current* referrer directory and answers from the
//! map — synchronously, as static imports require.
//!
//! ## Dynamic `import()` (scripts and module code)
//!
//! The compiler marks dynamic calls with argc=2 (static calls use argc=1; the
//! marker argument's value is unused). The native creates a real pending
//! promise (prototype = `%Promise%.prototype` from the realm intrinsics) and
//! enqueues a load job on the pending-job sink; the job evaluates the target
//! module graph on the draining interpreter and settles the promise with the
//! namespace snapshot (or the load/compile/evaluation error as the rejection
//! reason). Returning the promise synchronously matches ImportCall
//! evaluation; resolution happens at the next checkpoint, so `.then`/`await`
//! observe it through the normal microtask machinery.
//!
//! ## Known v1 gaps
//!
//! - Live bindings: namespace snapshots are taken after evaluation; a module
//!   that mutates its exports later is not observed.
//! - Cycles: a module re-entered while its graph is still loading yields an
//!   empty placeholder namespace (no partially-initialized live bindings).
//! - Re-exports (`export … from`) are skipped by the compiler epilogue.
//! - Specifier coercion uses basic ToString (no user `toString` hooks).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use v12_heap::JsValue;
use v12_interp::Interp;
use v12_native::Throw;

use super::builtins::ctx::Ctx;
use super::builtins::promise;
use crate::engine::string_value;
use crate::job_queue::{Job, JobCtx};

/// Shared module map + referrer tracking (see crate docs).
#[derive(Default)]
pub(crate) struct LoaderState {
    /// Referrer base for the *currently executing* top-level program (set
    /// per eval; the runner points it at the test file's directory).
    pub base: PathBuf,
    /// Directory of the module body currently executing. The `ModuleImport`
    /// native resolves static specifiers against this, so nested modules
    /// resolve their own imports relative to themselves.
    pub referrer: PathBuf,
    /// Evaluated modules: canonical path → namespace snapshot.
    pub modules: HashMap<PathBuf, JsValue>,
    /// Canonical paths currently loading (cycle detection).
    loading: HashSet<PathBuf>,
}

/// Lexically resolves `spec` against `base_dir`: join, then normalize `.`/`..`
/// components without touching the filesystem.
pub(crate) fn resolve_specifier(base_dir: &Path, spec: &str) -> PathBuf {
    normalize(&base_dir.join(spec))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop one component; keep a leading `..` for relative paths.
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(crate) type Loader = Rc<RefCell<LoaderState>>;

/// The `ModuleImport` native with a loader installed.
///
/// - Static call (argc=1, the compiler's lowered `import "spec"` prologue):
///   the resolved module must already be in the map (the graph driver
///   pre-evaluates dependencies); answer with its namespace snapshot.
/// - Dynamic call (argc=2, the compiler's `import()` lowering): create a
///   pending `%Promise%`-prototype promise and enqueue a load job that
///   evaluates the target graph and settles the promise.
pub(crate) fn handle_import(
    ctx: &mut Ctx,
    state: &Loader,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let dynamic = args.len() >= 2;
    let raw = args.first().copied().unwrap_or_else(JsValue::undefined);
    let spec_text = if let Some(h) = raw.as_string() {
        ctx.string_text(h)
    } else if raw.as_f64().is_some() {
        ctx.to_string(raw)
    } else {
        return Err(ctx.type_error("import specifier must be a string"));
    };
    // The currently executing program's directory is the resolution referrer.
    let dir = {
        let s = state.borrow();
        if s.referrer.as_os_str().is_empty() {
            s.base.clone()
        } else {
            s.referrer.clone()
        }
    };
    let path = resolve_specifier(&dir, &spec_text);
    if !dynamic {
        if let Some(ns) = state.borrow().modules.get(&path) {
            return Ok(*ns);
        }
        return Err(ctx.type_error(format!("Unlinked module import: '{spec_text}'")));
    }
    let proto = ctx
        .intrinsic("Promise")
        .and_then(|v| v.as_object())
        .and_then(|o| ctx.heap.get(o).prototype);
    let promise = promise::make_pending_promise(ctx.heap, proto);
    let job = make_load_job(Rc::clone(state), path, promise);
    ctx.enqueue_job(job);
    Ok(JsValue::object(promise))
}

/// Evaluates the module at `path` (loading its static dependency graph first,
/// post-order) and returns its namespace snapshot.
///
/// Runs on `interp` so each module body executes through
/// `call_program_main`; exported function objects carry the module's
/// registered program id and remain callable from any program sharing the
/// interpreter's program table.
pub(crate) fn load_and_evaluate(
    interp: &mut Interp<'_>,
    state: &Loader,
    path: &Path,
) -> Result<JsValue, JsValue> {
    if let Some(ns) = state.borrow().modules.get(path) {
        return Ok(*ns);
    }
    if state.borrow().loading.contains(path) {
        // Cycle: the namespace was allocated and registered *before* this
        // module's dependencies were evaluated (see below), so a cyclic or
        // self importer resolves to that same object. A registered-but-empty
        // namespace is the v1 snapshot contract: exports fill in once the
        // module body completes. Defensive fallback for an unregistered
        // re-entry (no live bindings either way).
        if let Some(ns) = state.borrow().modules.get(path) {
            return Ok(*ns);
        }
        let placeholder = interp.heap_mut().alloc(v12_heap::JsObject::ordinary(
            Default::default(),
            Default::default(),
        ));
        return Ok(JsValue::object(placeholder));
    }
    let source = std::fs::read_to_string(path).map_err(|e| {
        string_value(
            interp.heap_mut(),
            &format!("Cannot find module '{}': {e}", path.display()),
        )
    })?;
    let (module, strings) = v12_bccompiler::compile_source_as_module_with_strings(&source)
        .map_err(|e| {
            string_value(
                interp.heap_mut(),
                &format!("SyntaxError: {}: {}", path.display(), e.message),
            )
        })?;
    state.borrow_mut().loading.insert(path.to_path_buf());
    // Allocate and register the namespace *before* evaluating static
    // dependencies: a self-import (`import * as ns from './self.js'`) or a
    // cyclic importer hits `handle_import`, which answers from
    // `state.modules`; without early registration it throws
    // `Unlinked module import`. Exports populate after the body runs.
    let namespace = super::module_namespace::alloc_namespace(interp.heap_mut());
    state
        .borrow_mut()
        .modules
        .insert(path.to_path_buf(), JsValue::object(namespace));
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    // Install the statically-known export keys (values snapshot later) so
    // `in`/`hasOwnProperty`/enumeration observe the exports even for a
    // cyclic/self importer that reads the namespace while this body (or a
    // dependency's body) is still executing.
    let export_names: Vec<String> = module.exports.iter().map(|e| e.exported.clone()).collect();
    super::module_namespace::seed_export_keys(interp.heap_mut(), namespace, &export_names);
    // Evaluate static dependencies first (dedup, source order).
    let mut seen: HashSet<String> = HashSet::new();
    for entry in &module.imports {
        if seen.insert(entry.specifier.clone()) {
            let child = resolve_specifier(&dir, &entry.specifier);
            if let Err(reason) = load_and_evaluate(interp, state, &child) {
                // Do not cache a partially-linked namespace: a later dynamic
                // import of this path must retry rather than observe it.
                state.borrow_mut().loading.remove(path);
                state.borrow_mut().modules.remove(path);
                return Err(reason);
            }
        }
    }
    // Evaluate this module's body with the referrer pointing at its own
    // directory so its static import calls resolve correctly.
    let prev_referrer = state.borrow().referrer.clone();
    state.borrow_mut().referrer = dir;
    let main = module.program.main;
    let main_is_async = module
        .program
        .functions
        .get(main as usize)
        .is_some_and(|f| f.is_async);
    let functions: Rc<[v12_bytecode::FunctionBytecode]> = module.program.functions.into();
    let result = interp.call_program_main(functions, strings, main);
    state.borrow_mut().referrer = prev_referrer;
    // The compiler epilogue makes the module main's completion the exports
    // object; that snapshot is the namespace.
    let exports = result.map_err(|exc| {
        state.borrow_mut().loading.remove(path);
        state.borrow_mut().modules.remove(path);
        exc.0
    })?;
    // Top-level await: an async main returns its evaluation promise, not
    // the namespace. Drain interpreter awaits inline until that promise's
    // completion settles, then take the settlement value as the exports
    // object (rejections become evaluation errors). Settlements for any other
    // promise are handed back for the engine's checkpoint drain. Host jobs
    // (promise reactions, dynamic-import loads) are engine-side and cannot
    // run here: an evaluation promise parked on one stays pending and the
    // loop exits quiescent, falling back to the promise object.
    let exports = if main_is_async && exports.as_object().is_some() {
        settle_evaluation_promise(interp, exports)?
    } else {
        exports
    };
    if let Some(exports) = exports.as_object() {
        super::module_namespace::populate_namespace(interp.heap_mut(), namespace, exports);
    }
    state.borrow_mut().loading.remove(path);
    Ok(JsValue::object(namespace))
}

/// Drains interpreter awaits until async module `main_promise` settles.
///
/// Returns the fulfillment value, or the rejection reason as `Err`. Exits
/// quiescent (returning the still-pending promise) when no pass resumes or
/// settles anything: the remaining awaits are parked on promises only host
/// jobs can settle, which run at the engine's checkpoint drain.
fn settle_evaluation_promise(
    interp: &mut Interp<'_>,
    main_promise: JsValue,
) -> Result<JsValue, JsValue> {
    let target = main_promise.as_object();
    for _ in 0..10_000 {
        let resumed = interp.run_jobs();
        let settlements = interp.take_pending_settlements();
        let mut outcome: Option<Result<JsValue, JsValue>> = None;
        for (promise, value, rejecting) in settlements {
            if Some(promise) == target {
                if outcome.is_none() {
                    outcome = Some(if rejecting { Err(value) } else { Ok(value) });
                }
            } else {
                interp.push_pending_settlement(promise, value, rejecting);
            }
        }
        if let Some(result) = outcome {
            return result;
        }
        if resumed == 0 && !interp.has_pending_awaits() && !interp.has_pending_settlements() {
            break;
        }
        if resumed == 0 && !interp.has_pending_awaits() {
            break;
        }
    }
    Ok(main_promise)
}

/// Dynamic-import load job: evaluate the graph, then settle `promise`.
pub(crate) fn make_load_job(
    state: Loader,
    path: PathBuf,
    promise: v12_heap::Handle<v12_heap::JsObject>,
) -> Job {
    Box::new(move |ctx: &mut JobCtx<'_, '_>| {
        let result = load_and_evaluate(ctx.interp_mut(), &state, &path);
        match result {
            Ok(ns) => crate::builtins::promise::settle(
                ctx,
                promise,
                crate::builtins::promise::STATE_FULFILLED,
                ns,
            ),
            Err(reason) => crate::builtins::promise::settle(
                ctx,
                promise,
                crate::builtins::promise::STATE_REJECTED,
                reason,
            ),
        };
    })
}
