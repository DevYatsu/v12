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
    /// The two namespace objects are phase-specific, but the execution
    /// environment and cells below are shared by both.
    pub eager_namespace: Option<JsValue>,
    pub deferred_namespace: Option<JsValue>,
    pub env: Handle<JsObject>,
    pub owner_module_id: u32,
    pub binding_plan: Rc<[ModuleBindingPlan]>,
    pub export_stars: Rc<[String]>,
    pub evaluated: bool,
    /// Reserved for the deferred read-trigger evaluator in the next task.
    #[allow(dead_code)]
    pub cached_error: Option<JsValue>,
}

impl LinkedModule {
    pub fn namespace(&self, deferred: bool) -> Option<JsValue> {
        if deferred {
            self.deferred_namespace
        } else {
            self.eager_namespace
        }
    }
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
    /// Linked/evaluated modules: canonical path → one canonical module record.
    /// The record owns one execution environment/cell set; phase-specific
    /// namespace handles live in the record itself.
    pub modules: HashMap<PathBuf, LinkedModule>,
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
    // Static eager calls use one argument, static deferred calls use a
    // numeric phase marker, and dynamic imports carry the historical trailing
    // marker as a third argument.
    let dynamic = args.len() >= 3;
    let deferred = args.get(1).is_some_and(|marker| marker.as_smi() == Some(1));
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
        if let Some(namespace) = state
            .borrow()
            .modules
            .get(&path)
            .and_then(|m| m.namespace(deferred))
        {
            return Ok(namespace);
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
        if s.modules.get(path).is_some_and(|record| record.evaluated) || s.loading.contains(path) {
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
    let mut seen_spec: HashSet<(String, bool)> = HashSet::new();
    for entry in &module.imports {
        if seen_spec.insert((entry.specifier.clone(), entry.deferred)) {
            let child = resolve_specifier(&dir, &entry.specifier);
            gather_async_targets(interp, state, &child, seen, out)?;
        }
    }
    Ok(())
}

/// Links the module at `path` without evaluating it (the `import defer`
/// contract). Only the namespace handle is phase-specific; the record's
/// environment, cells, and evaluation status are shared with eager requests.
pub(crate) fn load_deferred(
    interp: &mut Interp<'_>,
    state: &Loader,
    path: &Path,
) -> Result<JsValue, JsValue> {
    if let Some(namespace) = state
        .borrow()
        .modules
        .get(path)
        .and_then(|record| record.namespace(true))
    {
        return Ok(namespace);
    }
    let existing = state.borrow().modules.get(path).map(|record| {
        (
            record.owner_module_id,
            record.env,
            record.binding_plan.clone(),
        )
    });
    if let Some((owner_module_id, env, plan)) = existing {
        let namespace = super::module_namespace::attach_namespace(
            interp.heap_mut(),
            owner_module_id,
            env,
            &plan,
            v12_heap::ModuleNamespaceVariant::Deferred,
        );
        let namespace = JsValue::object(namespace);
        if let Some(record) = state.borrow_mut().modules.get_mut(path) {
            record.deferred_namespace = Some(namespace);
        }
        if let Err(reason) = link_record(interp, state, path) {
            if let Some(record) = state.borrow_mut().modules.get_mut(path) {
                record.deferred_namespace = None;
            }
            return Err(reason);
        }
        return Ok(namespace);
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
    let env = super::module_namespace::alloc_execution(
        interp.heap_mut(),
        owner_module_id,
        env_slots,
        &module.binding_plan,
    );
    let namespace = super::module_namespace::attach_namespace(
        interp.heap_mut(),
        owner_module_id,
        env,
        &module.binding_plan,
        v12_heap::ModuleNamespaceVariant::Deferred,
    );
    let namespace = JsValue::object(namespace);
    state.borrow_mut().modules.insert(
        path.to_path_buf(),
        LinkedModule {
            eager_namespace: None,
            deferred_namespace: Some(namespace),
            env,
            owner_module_id,
            binding_plan: Rc::from(module.binding_plan.into_boxed_slice()),
            export_stars: Rc::from(module.export_stars.into_boxed_slice()),
            evaluated: false,
            cached_error: None,
        },
    );

    // Link the complete dependency graph before exposing this deferred
    // namespace. Dependencies are linked without body evaluation here; async
    // targets are handled by the existing transitive gather below.
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    for entry in &module.imports {
        if !seen.insert((entry.specifier.clone(), entry.deferred)) {
            continue;
        }
        let child = resolve_specifier(&dir, &entry.specifier);
        if let Err(reason) = load_deferred(interp, state, &child) {
            state.borrow_mut().modules.remove(path);
            return Err(reason);
        }
    }
    if let Err(reason) = link_record(interp, state, path) {
        state.borrow_mut().modules.remove(path);
        return Err(reason);
    }

    // Gather then evaluate async transitive dependencies. Path-based `seen`
    // remains the cycle guard; only request enumeration is phase-aware.
    let mut async_seen: HashSet<PathBuf> = HashSet::new();
    let mut targets: Vec<PathBuf> = Vec::new();
    gather_async_targets(interp, state, path, &mut async_seen, &mut targets)?;
    for target in targets {
        load_and_evaluate(interp, state, &target)?;
    }
    Ok(state
        .borrow()
        .modules
        .get(path)
        .and_then(|record| record.namespace(true))
        .expect("deferred namespace"))
}

/// Links export-star aliases and named binding cells for either namespace
/// variant of a canonical module record.
fn link_record(interp: &mut Interp<'_>, state: &Loader, path: &Path) -> Result<(), JsValue> {
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    validate_export_stars(interp, state, path, &dir)?;
    link_export_stars(interp, state, path, &dir)?;
    link_binding_cells(interp, state, path, &dir)
}

/// Evaluates the module at `path` (loading its static dependency graph first,
/// post-order) and returns its eager namespace snapshot.
pub(crate) fn load_and_evaluate(
    interp: &mut Interp<'_>,
    state: &Loader,
    path: &Path,
) -> Result<JsValue, JsValue> {
    if let Some(module) = state.borrow().modules.get(path) {
        if module.evaluated {
            if let Some(namespace) = module.eager_namespace {
                return Ok(namespace);
            }
        }
    }
    if state.borrow().loading.contains(path) {
        if let Some(module) = state.borrow().modules.get(path)
            && let Some(namespace) = module.namespace(false).or_else(|| module.namespace(true))
        {
            return Ok(namespace);
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
    let (env, owner_module_id) = {
        let existing = state
            .borrow()
            .modules
            .get(path)
            .map(|record| (record.env, record.owner_module_id));
        existing.unwrap_or_else(|| {
            let mut s = state.borrow_mut();
            let id = s.next_module_id;
            s.next_module_id += 1;
            let env = super::module_namespace::alloc_execution(
                interp.heap_mut(),
                id,
                env_slots,
                &module.binding_plan,
            );
            (env, id)
        })
    };
    let eager_namespace = {
        let existing = state
            .borrow()
            .modules
            .get(path)
            .and_then(|record| record.eager_namespace);
        existing.unwrap_or_else(|| {
            JsValue::object(super::module_namespace::attach_namespace(
                interp.heap_mut(),
                owner_module_id,
                env,
                &module.binding_plan,
                v12_heap::ModuleNamespaceVariant::Eager,
            ))
        })
    };
    {
        let mut state_mut = state.borrow_mut();
        let record = state_mut
            .modules
            .entry(path.to_path_buf())
            .or_insert_with(|| LinkedModule {
                eager_namespace: None,
                deferred_namespace: None,
                env,
                owner_module_id,
                binding_plan: Rc::from(module.binding_plan.clone().into_boxed_slice()),
                export_stars: Rc::from(module.export_stars.clone().into_boxed_slice()),
                evaluated: false,
                cached_error: None,
            });
        record.eager_namespace = Some(eager_namespace);
    }
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    for entry in &module.imports {
        if !seen.insert((entry.specifier.clone(), entry.deferred)) {
            continue;
        }
        let child = resolve_specifier(&dir, &entry.specifier);
        let result = if entry.deferred {
            load_deferred(interp, state, &child)
        } else {
            load_and_evaluate(interp, state, &child)
        };
        if let Err(reason) = result {
            state.borrow_mut().loading.remove(path);
            state.borrow_mut().modules.remove(path);
            return Err(reason);
        }
    }
    if let Err(reason) = link_record(interp, state, path) {
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
    let completion = result.map_err(|exc| {
        state.borrow_mut().loading.remove(path);
        state.borrow_mut().modules.remove(path);
        exc.0
    })?;
    if main_is_async && completion.as_object().is_some() {
        settle_evaluation_promise(interp, completion)?;
    }
    if let Some(record) = state.borrow_mut().modules.get_mut(path) {
        record.evaluated = true;
    }
    state.borrow_mut().loading.remove(path);
    Ok(eager_namespace)
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
    let namespaces: Vec<_> = record
        .namespace(false)
        .into_iter()
        .chain(record.namespace(true))
        .filter_map(|namespace| namespace.as_object())
        .collect();
    for specifier in record.export_stars.iter() {
        let child_path = resolve_specifier(base, specifier);
        let child = state
            .borrow()
            .modules
            .get(&child_path)
            .cloned()
            .ok_or_else(|| {
                string_value(
                    interp.heap_mut(),
                    &format!("Unlinked module import: '{specifier}'"),
                )
            })?;
        let child_ns = child
            .namespace(false)
            .or_else(|| child.namespace(true))
            .and_then(|namespace| namespace.as_object())
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
            for namespace in &namespaces {
                super::module_namespace::set_or_add_export_cell(
                    interp.heap_mut(),
                    *namespace,
                    &name,
                    cell,
                );
            }
        }
    }
    for namespace in namespaces {
        super::module_namespace::sort_exports(interp.heap_mut(), namespace);
    }
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
        let child = state
            .borrow()
            .modules
            .get(&child_path)
            .cloned()
            .ok_or_else(|| {
                string_value(
                    interp.heap_mut(),
                    &format!("Unlinked module import: '{specifier}'"),
                )
            })?;
        let child_ns = child
            .namespace(false)
            .or_else(|| child.namespace(true))
            .and_then(|namespace| namespace.as_object())
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
        let (specifier, imported, reexport, deferred) = match &binding.source {
            ModuleBindingSource::Local | ModuleBindingSource::DefaultExpression => continue,
            ModuleBindingSource::Import {
                specifier,
                imported,
                deferred,
            } => (specifier, imported, false, *deferred),
            ModuleBindingSource::ReExport {
                specifier,
                imported,
            } => (specifier, imported, true, false),
        };
        let child_path = resolve_specifier(base, specifier);
        let child = state.borrow().modules.get(&child_path).cloned();
        let Some(child) = child else {
            return Err(string_value(
                interp.heap_mut(),
                &format!("Unlinked module import: '{specifier}'"),
            ));
        };
        let child_ns = child
            .namespace(deferred)
            .or_else(|| child.namespace(!deferred))
            .and_then(|namespace| namespace.as_object())
            .expect("linked module namespace object");
        if imported == "*" {
            super::module_namespace::set_env_value(
                interp.heap_mut(),
                record.env,
                binding.env_slot,
                child
                    .namespace(deferred)
                    .or_else(|| child.namespace(!deferred))
                    .expect("linked module namespace"),
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
            for namespace in [record.namespace(false), record.namespace(true)]
                .into_iter()
                .flatten()
                .filter_map(|namespace| namespace.as_object())
            {
                super::module_namespace::set_export_cell(
                    interp.heap_mut(),
                    namespace,
                    &binding.exports[0],
                    cell,
                );
            }
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
