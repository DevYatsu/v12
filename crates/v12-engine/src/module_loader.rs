//! ES module loader: specifier resolution, module-graph loading/linking, and
//! evaluation.
//!
//! ## Shape
//!
//! [`LoaderState`] is the module map: canonical path → linked module record.
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
//! other programs). The compiled main runs with a loader-seeded environment;
//! export values stay in GC-visible binding cells, and the namespace properties
//! dereference those cells. When the importing module's body later runs
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
//! linked namespace (or the load/compile/evaluation error as the rejection
//! reason). Returning the promise synchronously matches ImportCall
//! evaluation; resolution happens at the next checkpoint, so `.then`/`await`
//! observe it through the normal microtask machinery.
//!
//! ## Known v1 gaps
//!
//! - Deferred reads do not trigger evaluation yet; the interpreter-owned
//!   evaluator capability is Task 6B.
//! - Specifier coercion uses basic ToString (no user `toString` hooks).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use v12_bccompiler::{ModuleBindingPlan, ModuleBindingSource};
use v12_heap::{Handle, JsObject, JsValue};
use v12_interp::Interp;
use v12_native::Throw;

use super::builtins::ctx::Ctx;
use super::builtins::promise;
use crate::engine::string_value;
use crate::job_queue::{Job, JobCtx};

#[derive(Clone)]
pub(crate) struct LinkedModule {
    pub namespace: JsValue,
    pub env: Handle<JsObject>,
    pub binding_plan: Rc<[ModuleBindingPlan]>,
    pub export_stars: Rc<[String]>,
    pub evaluated: bool,
}

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
    /// Linked/evaluated modules: canonical path → stable module record.
    pub modules: HashMap<PathBuf, LinkedModule>,
    /// Deferred modules: canonical path → *linked-only* deferred namespace.
    ///
    /// `import defer * as ns from` links the dependency (compile + export
    /// keys) but must not evaluate it. The namespace is registered here so
    /// the importer's lowered static-import call resolves to a real object;
    /// [`Self::modules`] stays reserved for evaluated modules so an eager
    /// import of the same path still evaluates rather than observing the
    /// placeholder.
    pub deferred: HashMap<PathBuf, LinkedModule>,
    /// Canonical paths currently loading (cycle detection).
    pub(crate) loading: HashSet<PathBuf>,
    pub(crate) next_module_id: u32,
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
        if let Some(module) = state.borrow().modules.get(&path) {
            return Ok(module.namespace);
        }
        // A `defer` dependency is linked into `deferred`, not `modules`.
        if let Some(module) = state.borrow().deferred.get(&path) {
            return Ok(module.namespace);
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

/// Reads and compiles the module at `path`, or returns the throw value for a
/// missing file / syntax error. Shared by eager evaluation and link-only
/// deferred loading.
fn read_and_compile(
    interp: &mut Interp<'_>,
    path: &Path,
) -> Result<(v12_bccompiler::Module, Vec<String>), JsValue> {
    let source = std::fs::read_to_string(path).map_err(|e| {
        string_value(
            interp.heap_mut(),
            &format!("Cannot find module '{}': {e}", path.display()),
        )
    })?;
    // JSON-module parsing/attributes are intentionally outside 6A. Preserve
    // the existing idempotency behavior with a synthetic undefined default
    // rather than rejecting the graph as an unlinked module.
    let source = if path.extension().is_some_and(|ext| ext == "json") {
        "export default undefined;".to_string()
    } else {
        source
    };
    v12_bccompiler::compile_source_as_module_with_strings(&source).map_err(|e| {
        string_value(
            interp.heap_mut(),
            &format!("SyntaxError: {}: {}", path.display(), e.message),
        )
    })
}

/// Recursively collects the modules a `defer` request must still evaluate
/// eagerly: the spec's `GatherAsynchronousTransitiveDependencies`.
///
/// A deferred module is evaluated eagerly exactly when it (or a transitively
/// reached dependency) has top-level await. The deferred module itself is
/// only included when its own main is async; a sync deferred module with an
/// async descendant contributes that descendant, not itself, so reading an
/// export still triggers the deferred body later.
fn gather_async_targets(
    interp: &mut Interp<'_>,
    state: &Loader,
    path: &Path,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) -> Result<(), JsValue> {
    if !seen.insert(path.to_path_buf()) {
        return Ok(());
    }
    {
        let s = state.borrow();
        // Already evaluated / currently evaluating: nothing more to gather.
        if s.modules.contains_key(path) || s.loading.contains(path) {
            return Ok(());
        }
    }
    let (module, _strings) = read_and_compile(interp, path)?;
    let main = module.program.main;
    let main_is_async = module
        .program
        .functions
        .get(main as usize)
        .is_some_and(|f| f.is_async);
    if main_is_async {
        out.push(path.to_path_buf());
        return Ok(());
    }
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut seen_spec: HashSet<String> = HashSet::new();
    for entry in &module.imports {
        if seen_spec.insert(entry.specifier.clone()) {
            let child = resolve_specifier(&dir, &entry.specifier);
            gather_async_targets(interp, state, &child, seen, out)?;
        }
    }
    Ok(())
}

/// Links the module at `path` without evaluating it (the `import defer`
/// contract): compiles it, allocates a namespace and binding cells, and
/// registers it in [`LoaderState::deferred`].
///
/// The deferred namespace deliberately lives in a separate map from
/// [`LoaderState::modules`]: an eager import of the same path must still
/// evaluate it rather than observe this unevaluated placeholder.
///
/// Exception (ES `InnerModuleEvaluation` / `GatherAsynchronousTransitiveDependencies`):
/// asynchronous transitive dependencies are evaluated eagerly — a module with
/// top-level await cannot be deferred, so each gathered target runs through
/// [`load_and_evaluate`] here.
pub(crate) fn load_deferred(
    interp: &mut Interp<'_>,
    state: &Loader,
    path: &Path,
) -> Result<JsValue, JsValue> {
    if let Some(module) = state.borrow().modules.get(path) {
        return Ok(module.namespace);
    }
    if let Some(module) = state.borrow().deferred.get(path) {
        return Ok(module.namespace);
    }
    let (module, _strings) = read_and_compile(interp, path)?;
    let env_slots = module
        .program
        .functions
        .get(module.program.main as usize)
        .map_or(0, |f| f.env_slots);
    let owner_module_id = {
        let mut s = state.borrow_mut();
        let id = s.next_module_id;
        s.next_module_id += 1;
        id
    };
    let (namespace, env) = super::module_namespace::alloc_linked_module(
        interp.heap_mut(),
        owner_module_id,
        env_slots,
        &module.binding_plan,
    );
    let namespace = JsValue::object(namespace);
    let record = LinkedModule {
        namespace,
        env,
        binding_plan: Rc::from(module.binding_plan.into_boxed_slice()),
        export_stars: Rc::from(module.export_stars.into_boxed_slice()),
        evaluated: false,
    };
    state
        .borrow_mut()
        .deferred
        .insert(path.to_path_buf(), record);
    // Gather then evaluate the async transitive dependencies. Gathering must
    // complete before any of them evaluates: evaluating one may add its own
    // namespaces to `modules`, which would prune the walk.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut targets: Vec<PathBuf> = Vec::new();
    gather_async_targets(interp, state, path, &mut seen, &mut targets)?;
    for target in targets {
        load_and_evaluate(interp, state, &target)?;
    }
    // An async deferred module was evaluated above; hand back its real
    // namespace so the importer observes populated exports immediately.
    if let Some(module) = state.borrow().modules.get(path) {
        return Ok(module.namespace);
    }
    Ok(namespace)
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
    if let Some(module) = state.borrow().modules.get(path) {
        if module.evaluated {
            return Ok(module.namespace);
        }
    }
    if state.borrow().loading.contains(path) {
        // Cycle: the namespace and cell records were allocated and registered
        // before this module's dependencies were evaluated, so a cyclic or
        // self importer resolves to that same object. Defensive fallback for
        // an unregistered re-entry.
        if let Some(module) = state.borrow().modules.get(path) {
            return Ok(module.namespace);
        }
        let placeholder = interp.heap_mut().alloc(v12_heap::JsObject::ordinary(
            Default::default(),
            Default::default(),
        ));
        return Ok(JsValue::object(placeholder));
    }
    let (module, strings) = read_and_compile(interp, path)?;
    state.borrow_mut().loading.insert(path.to_path_buf());
    let env_slots = module
        .program
        .functions
        .get(module.program.main as usize)
        .map_or(0, |f| f.env_slots);
    let owner_module_id = {
        let mut s = state.borrow_mut();
        let id = s.next_module_id;
        s.next_module_id += 1;
        id
    };
    let (namespace, env) = super::module_namespace::alloc_linked_module(
        interp.heap_mut(),
        owner_module_id,
        env_slots,
        &module.binding_plan,
    );
    let namespace = JsValue::object(namespace);
    let record = LinkedModule {
        namespace,
        env,
        binding_plan: Rc::from(module.binding_plan.clone().into_boxed_slice()),
        export_stars: Rc::from(module.export_stars.clone().into_boxed_slice()),
        evaluated: false,
    };
    state
        .borrow_mut()
        .modules
        .insert(path.to_path_buf(), record);
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    // Evaluate static dependencies first (dedup, source order). A `defer`
    // dependency is linked (namespace/cells allocated) but
    // not evaluated here: its body runs only when an export is read through
    // the deferred namespace, per the defer-import-eval proposal.
    let mut seen: HashSet<String> = HashSet::new();
    for entry in &module.imports {
        if seen.insert(entry.specifier.clone()) {
            let child = resolve_specifier(&dir, &entry.specifier);
            let result = if entry.deferred {
                load_deferred(interp, state, &child)
            } else {
                load_and_evaluate(interp, state, &child)
            };
            if let Err(reason) = result {
                // Do not cache a partially-linked namespace: a later dynamic
                // import of this path must retry rather than observe it.
                state.borrow_mut().loading.remove(path);
                state.borrow_mut().modules.remove(path);
                return Err(reason);
            }
        }
    }
    if let Err(reason) = validate_export_stars(interp, state, path, &dir) {
        state.borrow_mut().loading.remove(path);
        state.borrow_mut().modules.remove(path);
        return Err(reason);
    }
    if let Err(reason) = link_export_stars(interp, state, path, &dir) {
        state.borrow_mut().loading.remove(path);
        state.borrow_mut().modules.remove(path);
        return Err(reason);
    }
    if let Err(reason) = link_binding_cells(interp, state, path, &dir) {
        state.borrow_mut().loading.remove(path);
        state.borrow_mut().modules.remove(path);
        return Err(reason);
    }
    let env = state
        .borrow()
        .modules
        .get(path)
        .expect("linked module record")
        .env;
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
    let result = interp.call_program_main_with_env(functions, strings, main, Some(env));
    state.borrow_mut().referrer = prev_referrer;
    let _completion = result.map_err(|exc| {
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
    if main_is_async && _completion.as_object().is_some() {
        settle_evaluation_promise(interp, _completion)?;
    }
    if let Some(record) = state.borrow_mut().modules.get_mut(path) {
        record.evaluated = true;
    }
    state.borrow_mut().loading.remove(path);
    Ok(namespace)
}

/// Compatibility alias installation for `export *`. This intentionally does
/// not attempt full star-export resolution; it copies already validated child
/// cells so the previously passing arbitrary-name star probe remains usable.
pub(crate) fn link_export_stars(
    interp: &mut Interp<'_>,
    state: &Loader,
    module_path: &Path,
    base: &Path,
) -> Result<(), JsValue> {
    let record = state
        .borrow()
        .modules
        .get(module_path)
        .cloned()
        .expect("linked module record");
    let namespace = record
        .namespace
        .as_object()
        .expect("linked module namespace object");
    for specifier in record.export_stars.iter() {
        let child_path = resolve_specifier(base, specifier);
        let child = {
            let state = state.borrow();
            state
                .modules
                .get(&child_path)
                .or_else(|| state.deferred.get(&child_path))
                .cloned()
        };
        let Some(child) = child else {
            return Err(string_value(
                interp.heap_mut(),
                &format!("Unlinked module import: '{specifier}'"),
            ));
        };
        let child_ns = child
            .namespace
            .as_object()
            .expect("linked module namespace object");
        for name in super::module_namespace::export_names(interp.heap_mut(), child_ns) {
            if name == "default" {
                continue;
            }
            let Some(cell) =
                super::module_namespace::export_cell(interp.heap_mut(), child_ns, &name)
            else {
                continue;
            };
            super::module_namespace::set_or_add_export_cell(
                interp.heap_mut(),
                namespace,
                &name,
                cell,
            );
        }
    }
    super::module_namespace::sort_exports(interp.heap_mut(), namespace);
    Ok(())
}

/// Compatibility validation for indirect `export *` entries. Full star
/// export materialization remains deferred, but duplicate names from distinct
/// source modules must reject dynamic imports as `SyntaxError` rather than
/// silently producing an incomplete namespace.
pub(crate) fn validate_export_stars(
    interp: &mut Interp<'_>,
    state: &Loader,
    module_path: &Path,
    base: &Path,
) -> Result<(), JsValue> {
    let record = state
        .borrow()
        .modules
        .get(module_path)
        .cloned()
        .expect("linked module record");
    let mut owners: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for specifier in record.export_stars.iter() {
        let child_path = resolve_specifier(base, specifier);
        let child = {
            let state = state.borrow();
            state
                .modules
                .get(&child_path)
                .or_else(|| state.deferred.get(&child_path))
                .cloned()
        };
        let Some(child) = child else {
            return Err(string_value(
                interp.heap_mut(),
                &format!("Unlinked module import: '{specifier}'"),
            ));
        };
        let child_ns = child
            .namespace
            .as_object()
            .expect("linked module namespace object");
        for name in super::module_namespace::export_names(interp.heap_mut(), child_ns) {
            if name == "default" {
                continue;
            }
            if let Some(previous) = owners.get(&name)
                && previous != &child_path
            {
                return Err(
                    interp.error_value(&format!("SyntaxError: ambiguous export name '{name}'"))
                );
            }
            owners.insert(name, child_path.clone());
        }
    }
    Ok(())
}

/// Installs direct source-cell aliases for named imports and explicit
/// re-exports. Namespace imports use a cell whose value is the stable namespace
/// object.
pub(crate) fn link_binding_cells(
    interp: &mut Interp<'_>,
    state: &Loader,
    module_path: &Path,
    base: &Path,
) -> Result<(), JsValue> {
    let record = state
        .borrow()
        .modules
        .get(module_path)
        .cloned()
        .expect("linked module record");
    for binding in record.binding_plan.iter() {
        let (specifier, imported, reexport) = match &binding.source {
            ModuleBindingSource::Local | ModuleBindingSource::DefaultExpression => continue,
            ModuleBindingSource::Import {
                specifier,
                imported,
            } => (specifier, imported, false),
            ModuleBindingSource::ReExport {
                specifier,
                imported,
            } => (specifier, imported, true),
        };
        let child_path = resolve_specifier(base, specifier);
        let child = {
            let s = state.borrow();
            s.modules
                .get(&child_path)
                .or_else(|| s.deferred.get(&child_path))
                .cloned()
        };
        let Some(child) = child else {
            return Err(string_value(
                interp.heap_mut(),
                &format!("Unlinked module import: '{specifier}'"),
            ));
        };
        let child_ns = child
            .namespace
            .as_object()
            .expect("linked module namespace object");
        if imported == "*" {
            super::module_namespace::set_env_value(
                interp.heap_mut(),
                record.env,
                binding.env_slot,
                child.namespace,
            );
            continue;
        }
        let cell = super::module_namespace::export_cell(interp.heap_mut(), child_ns, imported)
            .ok_or_else(|| {
                string_value(
                    interp.heap_mut(),
                    &format!("The requested module does not provide an export named '{imported}'"),
                )
            })?;
        super::module_namespace::set_env_cell(
            interp.heap_mut(),
            record.env,
            binding.env_slot,
            cell,
        );
        if reexport {
            let namespace = record
                .namespace
                .as_object()
                .expect("module namespace object");
            super::module_namespace::set_export_cell(
                interp.heap_mut(),
                namespace,
                &binding.exports[0],
                cell,
            );
        }
    }
    Ok(())
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
