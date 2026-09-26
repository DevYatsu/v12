//! Promise built-ins — minimal `Promise.resolve`, `Promise.reject`, and
//! `Promise.prototype.then`, plus the real `queueMicrotask`.
//!
//! Promise state lives on the promise object (an ordinary object linked to
//! `Promise.prototype`, which the realm wires as the Promise constructor's
//! `prototype` field) as unshaped property slots. The shape system is
//! deliberately avoided: natives and jobs access the slots directly, and the
//! interpreter's `then` fast path recognizes promises by prototype identity.
//!
//! - `properties[0]`: `[[State]]` — Smi 0 pending / 1 fulfilled / 2 rejected
//! - `properties[1]`: `[[Result]]` — the settled payload
//! - `properties[2]`: `[[Reactions]]` — array of reaction records
//!
//! A reaction record is an ordinary object with
//! `properties[0..3] == [fulfill handler, reject handler, derived promise]`;
//! a handler may be `undefined` (pass-through to the derived promise).
//!
//! Thenables (any object with a `then` property) are adopted by
//! [`capability_settle`]: the resolution decision point moves into a
//! [`Job`] that the engine's single scheduler already runs, so
//! `then(resolve, reject)` is never called synchronously and a thenable
//! that never calls back leaves the checkpoint quiescent. The `then` read
//! that decides the branch happens inside that job, through the heap
//! ([`read_then`]) — the engine has no interpreter to read through, and
//! `Ctx` cannot activate a user getter. Accessor `then` is honoured by
//! running the stored getter from the job. Two receiver kinds are out of
//! scope: a proxy thenable (its `get` trap needs the interpreter) and a
//! realm global (its descriptor-less intrinsic prefix makes a shape slot
//! not a `properties` index) both read as "no `then`", so they resolve
//! with the value itself.
//!
//! Promises, reaction records, reaction arrays, and queued callbacks are
//! rooted at creation: jobs run after the program's operand stack is gone, so
//! everything a job reaches must survive collection. This trades reclamation
//! for soundness (the objects are small and few in harness workloads).
//!
//! Still out of scope: `Symbol.species` constructor selection (every promise
//! here uses `%Promise%` or the receiver's own `prototype`), and adoption
//! when a *reaction handler's* return value is a thenable (`settle` writes
//! the handler's value straight into the derived promise).

use std::cell::RefCell;
use std::rc::Rc;

use v12_heap::{Heap, JsObject, JsValue, Kind};
use v12_native::Throw;

use super::ctx::Ctx;
use crate::job_queue::{Job, JobCtx};
use v12_interp::JSException;

/// `[[State]]`: pending.
pub(crate) const STATE_PENDING: i32 = 0;
/// `[[State]]`: fulfilled.
pub(crate) const STATE_FULFILLED: i32 = 1;
/// `[[State]]`: rejected.
pub(crate) const STATE_REJECTED: i32 = 2;

/// Number of internal slots a promise object carries.
const PROMISE_SLOTS: usize = 3;

fn smi(v: i32) -> JsValue {
    JsValue::from_i32_smi(v).expect("state fits Smi")
}

/// Structural promise check: a `Kind::Promise` object carrying the three
/// internal slots with a plausible `[[State]]`. Promise objects are
/// engine-created only, so no user object collides in practice. The slot count
/// is exact on purpose: a user-added own property makes the object stop
/// answering as a promise here, which several `ctor-null` / `uniq-ctor` tests
/// pin. The interpreter's `Promise.resolve` seam mirrors this predicate, so the
/// two agree on which values are promises.
fn is_promise(heap: &Heap, v: JsValue) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    let o = heap.get(obj);
    o.kind == v12_heap::Kind::Promise
        && o.properties.len() == PROMISE_SLOTS
        && o.properties[0]
            .as_smi()
            .is_some_and(|s| (STATE_PENDING..=STATE_REJECTED).contains(&s))
}

/// Allocates a promise object with the given state and payload, rooted.
fn create_promise(
    heap: &mut Heap,
    prototype: Option<v12_heap::Handle<JsObject>>,
    state: i32,
    payload: JsValue,
) -> v12_heap::Handle<JsObject> {
    let reactions = heap.alloc(JsObject::array(Vec::new()));
    heap.add_root(JsValue::object(reactions));
    // `alloc_promise` binds the reserved promise shape, so a user-defined own
    // property on this object is numbered past `[[State]]`/`[[Result]]`/
    // `[[Reactions]]` instead of aliasing them.
    let promise = heap.alloc_promise(JsObject {
        kind: v12_heap::Kind::Promise,
        properties: smallvec::smallvec![smi(state), payload, JsValue::object(reactions)],
        prototype,
        ..JsObject::default()
    });
    heap.add_root(JsValue::object(promise));
    promise
}

/// `Promise.resolve(x)`: identity for promises; otherwise a fulfilled promise
/// carrying `x` (`undefined` when the argument is missing).
///
/// Thenable adoption is *not* here: it needs the `then` read, which runs in
/// the interpreter (`Interp::run_promise_resolve`) because a getter or a proxy
/// `get` trap re-enters the machine. The interpreter queues the adoption and
/// this handler stays the non-thenable half — which is also what a standalone
/// embedder with no interpreter gets.
pub fn promise_resolve(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    if is_promise(ctx.heap, value) {
        return Ok(value);
    }
    // Called as a method, `this` is the Promise constructor whose `prototype`
    // link hosts `Promise.prototype`; the interpreter's `then` fast path
    // recognizes instances by that identity. Unbound calls (e.g. a destructured
    // `const r = Promise.resolve`) degrade gracefully: the promise works but
    // its `then` is unreachable from script.
    let prototype = this
        .as_object()
        .and_then(|ctor| ctx.heap.get(ctor).prototype);
    Ok(JsValue::object(create_promise(
        ctx.heap,
        prototype,
        STATE_FULFILLED,
        value,
    )))
}

/// `Promise.reject(x)`: a rejected promise carrying `x`.
pub fn promise_reject(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    let prototype = this
        .as_object()
        .and_then(|ctor| ctx.heap.get(ctor).prototype);
    Ok(JsValue::object(create_promise(
        ctx.heap,
        prototype,
        STATE_REJECTED,
        value,
    )))
}

/// A pre-rejected promise value for internal seams (`import()` before a
/// module loader exists). Prototype-less: the interpreter's promise surface
/// recognizes promises structurally, so `.then`/`.catch` still work.
pub(crate) fn make_rejected_promise(heap: &mut Heap, reason: JsValue) -> JsValue {
    JsValue::object(create_promise(heap, None, STATE_REJECTED, reason))
}

/// A rooted pending promise carrying `prototype` as its `[[Prototype]]` —
/// used by the dynamic-import path so `Object.getPrototypeOf(p)` observes
/// `Promise.prototype` (ImportCall step 4: `NewPromiseCapability(%Promise%)`).
pub(crate) fn make_pending_promise(
    heap: &mut Heap,
    prototype: Option<v12_heap::Handle<JsObject>>,
) -> v12_heap::Handle<JsObject> {
    create_promise(heap, prototype, STATE_PENDING, JsValue::undefined())
}

/// `Promise.prototype.then(on_fulfilled, on_rejected)`.
///
/// On a pending promise: appends a reaction record. On a settled promise:
/// enqueues the reaction job immediately (the job runs at the next
/// checkpoint and settles the derived promise).
///
/// The pending-job sink comes from `Ctx::pending` (Phase 2 carrier); the
/// registry intercept always supplies it, so no separate stateful signature
/// is needed. A detached `Ctx` without a sink falls back to an ephemeral
/// queue (jobs are dropped) rather than failing the `then`.
pub fn promise_then(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    if !is_promise(ctx.heap, this) {
        return Err(ctx.type_error("Promise.prototype.then requires a promise"));
    }
    let promise = this.as_object().expect("checked above");
    let handler = args.first().copied().unwrap_or_else(JsValue::undefined);
    let on_rejected = args.get(1).copied().unwrap_or_else(JsValue::undefined);
    let sink = ctx.pending.clone().unwrap_or_default();
    let prototype = ctx.heap.get(promise).prototype;
    let derived = create_promise(ctx.heap, prototype, STATE_PENDING, JsValue::undefined());

    let (state, payload) = {
        let p = ctx.heap.get(promise);
        (
            p.properties[0].as_smi().unwrap_or(STATE_PENDING),
            p.properties[1],
        )
    };
    match state {
        STATE_PENDING => {
            let reactions = ctx.heap.get(promise).properties[2]
                .as_object()
                .expect("promise carries a reactions array");
            let record = ctx.heap.alloc(JsObject::ordinary(
                smallvec::smallvec![handler, on_rejected, JsValue::object(derived)],
                smallvec::smallvec![None; 3],
            ));
            ctx.heap.add_root(JsValue::object(record));
            ctx.heap
                .get_mut(reactions)
                .elements
                .push(JsValue::object(record));
        }
        STATE_FULFILLED => enqueue_reaction(
            &mut |job| sink.borrow_mut().push(job),
            handler,
            payload,
            derived,
            false,
        ),
        _ => enqueue_reaction(
            &mut |job| sink.borrow_mut().push(job),
            on_rejected,
            payload,
            derived,
            true,
        ),
    }
    Ok(JsValue::object(derived))
}

/// `queueMicrotask(cb)`: enqueues a job that calls `cb` with no arguments.
/// Throw completions from the callback are swallowed (Tier-0 reporting
/// substrate does not exist yet).
pub fn queue_microtask(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let cb = args.first().copied().unwrap_or_else(JsValue::undefined);
    if cb.as_object().is_none() {
        return Err(ctx.type_error("queueMicrotask requires a function"));
    }
    // Root the callback: the job outlives the program stack that referenced it.
    ctx.heap.add_root(cb);
    let cb_obj = cb.as_object().expect("checked above");
    let sink = ctx.pending.clone().unwrap_or_default();
    sink.borrow_mut().push(Box::new(move |ctx| {
        let _ = ctx.call_object(cb_obj, JsValue::undefined(), &[]);
    }));
    Ok(JsValue::undefined())
}

/// Builds one reaction-settling job. The job calls the handler with the
/// payload (or passes the payload straight through when the handler is
/// absent), then settles the derived promise — which in turn schedules that
/// promise's own queued reactions.
fn reaction_job(
    handler: JsValue,
    payload: JsValue,
    derived: v12_heap::Handle<JsObject>,
    rejecting: bool,
) -> Job {
    Box::new(move |ctx: &mut JobCtx<'_, '_>| {
        let callable = handler
            .as_object()
            .is_some_and(|h| ctx.heap_mut().get(h).kind == Kind::Function);
        let outcome = if callable {
            let h = handler.as_object().expect("checked above");
            ctx.call_object(h, JsValue::undefined(), &[payload])
        } else if rejecting {
            // Absent reject handler: pass the rejection through.
            Err(JSException(payload))
        } else {
            // Absent fulfill handler: pass the payload through.
            Ok(payload)
        };
        match outcome {
            Ok(v) => settle(ctx, derived, STATE_FULFILLED, v),
            Err(JSException(e)) => settle(ctx, derived, STATE_REJECTED, e),
        }
    })
}

/// Builds one reaction-settling job and hands it to `push`.
///
/// The job calls the handler with the payload (or passes the payload straight
/// through when the handler is absent), then settles the derived promise —
/// which in turn schedules that promise's own queued reactions.
fn enqueue_reaction(
    push: &mut dyn FnMut(Job),
    handler: JsValue,
    payload: JsValue,
    derived: v12_heap::Handle<JsObject>,
    rejecting: bool,
) {
    push(reaction_job(handler, payload, derived, rejecting));
}

/// Takes `promise`'s queued reaction records and builds one settling job per
/// record for the given settlement. The records are consumed (the reactions
/// array is emptied); the returned jobs still need an enqueue sink.
fn drain_reaction_jobs(
    heap: &mut Heap,
    promise: v12_heap::Handle<JsObject>,
    state: i32,
    value: JsValue,
) -> Vec<Job> {
    let Some(reactions) = heap.get(promise).properties[2].as_object() else {
        return Vec::new();
    };
    let records = std::mem::take(&mut heap.get_mut(reactions).elements);
    let mut jobs = Vec::with_capacity(records.len());
    for record_v in records {
        let Some(record) = record_v.as_object() else {
            continue;
        };
        let (fulfill, reject, derived_v) = {
            let r = heap.get(record);
            (r.properties[0], r.properties[1], r.properties[2])
        };
        let Some(derived) = derived_v.as_object() else {
            continue;
        };
        let (handler, rejecting) = if state == STATE_FULFILLED {
            (fulfill, false)
        } else {
            (reject, true)
        };
        jobs.push(reaction_job(handler, value, derived, rejecting));
    }
    jobs
}

/// Settles `promise` with `state`/`value` and schedules one job per queued
/// reaction record (microtask checkpoint semantics: the jobs join the
/// current drain via `ctx.enqueue`). Also used by the module loader's
/// dynamic-import load jobs.
pub(crate) fn settle(
    ctx: &mut JobCtx<'_, '_>,
    promise: v12_heap::Handle<JsObject>,
    state: i32,
    value: JsValue,
) {
    {
        let heap = ctx.heap_mut();
        heap.get_mut(promise).properties[0] = smi(state);
        heap.get_mut(promise).properties[1] = value;
    }
    for job in drain_reaction_jobs(ctx.heap_mut(), promise, state, value) {
        ctx.enqueue(job);
    }
}

/// Resolves or rejects `promise` through a constructor capability. No-op once
/// the promise has settled (first resolution wins). A promise value is
/// adopted: an adoption record `[resolve, reject, derived]` joins its
/// reactions, so settling the adopted promise routes the outcome back here.
/// A thenable value is adopted by a [`thenable_job`] queued on the same sink.
/// Everything else fulfills/rejects `promise` directly, and its queued
/// reaction jobs join the shared pending sink for the next checkpoint.
#[allow(clippy::too_many_arguments)]
fn capability_settle(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
    resolve_v: JsValue,
    reject_v: JsValue,
    value: JsValue,
    rejecting: bool,
) -> Result<JsValue, JsValue> {
    let still_pending = heap.get(promise).properties[0].as_smi() == Some(STATE_PENDING);
    if !still_pending {
        return Ok(JsValue::undefined());
    }
    if !rejecting && is_promise(heap, value) {
        let value_obj = value.as_object().expect("checked above");
        let (v_state, v_payload) = {
            let o = heap.get(value_obj);
            (
                o.properties[0].as_smi().unwrap_or(STATE_PENDING),
                o.properties[1],
            )
        };
        if v_state != STATE_PENDING {
            // Already-settled adopted promise: a late-attached record would
            // never drain (settle runs once), so route the outcome directly.
            let state = if v_state == STATE_FULFILLED {
                STATE_FULFILLED
            } else {
                STATE_REJECTED
            };
            heap.get_mut(promise).properties[0] = smi(state);
            heap.get_mut(promise).properties[1] = v_payload;
            let jobs = drain_reaction_jobs(heap, promise, state, v_payload);
            sink.borrow_mut().extend(jobs);
            return Ok(JsValue::undefined());
        }
        let proto = heap.get(promise).prototype;
        let derived = create_promise(heap, proto, STATE_PENDING, JsValue::undefined());
        let record = heap.alloc(JsObject::ordinary(
            smallvec::smallvec![resolve_v, reject_v, JsValue::object(derived)],
            smallvec::smallvec![None; 3],
        ));
        heap.add_root(JsValue::object(record));
        if let Some(reactions) = heap.get(value_obj).properties[2].as_object() {
            heap.get_mut(reactions)
                .elements
                .push(JsValue::object(record));
        }
        return Ok(JsValue::undefined());
    }
    if !rejecting && start_thenable_adoption(heap, sink, promise, value) {
        // The thenable job owns the settlement from here: `promise` stays
        // pending until the job calls the capability back (or never).
        return Ok(JsValue::undefined());
    }
    let state = if rejecting {
        STATE_REJECTED
    } else {
        STATE_FULFILLED
    };
    heap.get_mut(promise).properties[0] = smi(state);
    heap.get_mut(promise).properties[1] = value;
    let jobs = drain_reaction_jobs(heap, promise, state, value);
    sink.borrow_mut().extend(jobs);
    Ok(JsValue::undefined())
}

/// What `Get(value, "then")` names, read straight off the heap.
///
/// The engine has no interpreter to read through and `Ctx` cannot activate a
/// user getter, so the resolution decision is deferred to the job
/// [`start_thenable_adoption`] enqueues; this only reports *that* a `then`
/// property exists and how to obtain its value.
enum ThenRead {
    /// A data property: its stored value (`undefined` for a missing slot,
    /// which is not callable either way).
    Data(JsValue),
    /// An accessor property: the stored getter, which the job calls to learn
    /// the real `then` (spec 27.2.1.3.1 step 3-a).
    Getter(v12_heap::Handle<JsObject>),
}

/// `Get(value, "then")` for adoption purposes: the first `then` descriptor on
/// `obj` or its prototype chain. `None` means "no `then` property", so the
/// value is not a thenable and the promise fulfills with it unchanged.
///
/// The walk is the ordinary one (own descriptor, then `[[Prototype]]`, …) so
/// an inherited `then` counts. See the module docs for the two receiver
/// kinds this skips.
fn read_then(heap: &mut Heap, obj: v12_heap::Handle<JsObject>) -> Option<ThenRead> {
    if heap.is_realm_global(obj)
        || matches!(heap.get(obj).kind, Kind::Proxy | Kind::ModuleNamespace)
    {
        return None;
    }
    let key = v12_heap::PropKey::from_string(heap.intern_text("then"));
    let mut cur = Some(obj);
    while let Some(h) = cur {
        let shape = heap.shape_of(h);
        match heap.lookup_property(shape, key).copied() {
            Some(v12_heap::Descriptor::Data { slot, .. }) => {
                let value = heap
                    .get(h)
                    .properties
                    .get(slot as usize)
                    .copied()
                    .unwrap_or_else(JsValue::undefined);
                return Some(ThenRead::Data(value));
            }
            Some(v12_heap::Descriptor::Accessor { getter, .. }) => {
                return getter.map(ThenRead::Getter);
            }
            None => {}
        }
        cur = heap.get(h).prototype;
    }
    None
}

/// Queues the thenable job for `value` when it is a thenable, leaving
/// `promise` pending for that job to settle. Returns `false` when `value` has
/// no `then` property, so the caller settles the promise itself.
fn start_thenable_adoption(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
    value: JsValue,
) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    if is_promise(heap, value) {
        return false;
    }
    let Some(read) = read_then(heap, obj) else {
        return false;
    };
    match read {
        ThenRead::Data(then_v) => {
            enqueue_thenable_adoption(heap, sink, promise, value, then_v, false);
            true
        }
        // A getter cannot run without an interpreter, so the read stays
        // deferred: the job calls the stored getter and uses its result.
        ThenRead::Getter(getter) => {
            heap.add_root(value);
            heap.add_root(JsValue::object(promise));
            let (resolve_v, reject_v) = make_capability(heap, sink, promise);
            sink.borrow_mut().push(thenable_job(
                value,
                ThenRead::Getter(getter),
                false,
                resolve_v,
                reject_v,
                promise,
                Rc::clone(sink),
            ));
            true
        }
    }
}

/// Builds and queues the thenable job for an adoption the interpreter
/// requested. The `then` read already happened there through the ordinary
/// property path, so `abrupt`/`then_v` carry that read's outcome: an abrupt
/// read rejects the promise with `then_v`, otherwise `then_v` is `then`
/// itself (spec 27.2.1.3.1 step 3-a).
pub(crate) fn enqueue_thenable_adoption(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
    thenable: JsValue,
    then_v: JsValue,
    abrupt: bool,
) {
    // Rooted per the module contract: the job outlives the program stack that
    // built the thenable, and the sink is invisible to the collector.
    heap.add_root(thenable);
    heap.add_root(JsValue::object(promise));
    let (resolve_v, reject_v) = make_capability(heap, sink, promise);
    sink.borrow_mut().push(thenable_job(
        thenable,
        ThenRead::Data(then_v),
        abrupt,
        resolve_v,
        reject_v,
        promise,
        Rc::clone(sink),
    ));
}

/// Spec 27.2.1.3.1 `PromiseResolveFunctions` steps 3-a to 3-d, deferred to a
/// job: reject on an abrupt `Get`, fulfill with the thenable when `then` is
/// not callable, otherwise call it with the capability pair and let the
/// callbacks settle the promise.
fn thenable_job(
    thenable: JsValue,
    read: ThenRead,
    abrupt: bool,
    resolve_v: JsValue,
    reject_v: JsValue,
    promise: v12_heap::Handle<JsObject>,
    sink: Rc<RefCell<Vec<Job>>>,
) -> Job {
    Box::new(move |ctx: &mut JobCtx<'_, '_>| {
        if abrupt {
            // The ordinary-path `Get` threw: the abrupt rides the promise
            // (spec 27.2.1.3.1 step 3-a; pinned by
            // `Promise/resolve/arg-poisoned-then.js`). The thrown value
            // travels as the would-be `then`.
            let ThenRead::Data(err) = read else {
                unreachable!("an abrupt read carries a value, not a getter")
            };
            let _ = capability_settle(
                ctx.heap_mut(),
                &sink,
                promise,
                resolve_v,
                reject_v,
                err,
                true,
            );
            return;
        }
        let then_v = match read {
            ThenRead::Data(value) => Ok(value),
            ThenRead::Getter(getter) => ctx.call_object(getter, thenable, &[]),
        };
        let then_v = match then_v {
            Ok(value) => value,
            Err(JSException(thrown)) => {
                let _ = capability_settle(
                    ctx.heap_mut(),
                    &sink,
                    promise,
                    resolve_v,
                    reject_v,
                    thrown,
                    true,
                );
                return;
            }
        };
        let callable = then_v
            .as_object()
            .is_some_and(|f| matches!(ctx.heap_mut().get(f).kind, Kind::Function));
        if !callable {
            // A present but non-callable `then` is not an adoption: the
            // promise fulfills with the thenable itself (spec 27.2.1.3.1
            // step 3-b-i; pinned by `Promise/resolve/arg-non-thenable.js`).
            let _ = capability_settle(
                ctx.heap_mut(),
                &sink,
                promise,
                resolve_v,
                reject_v,
                thenable,
                false,
            );
            return;
        }
        let then = then_v.as_object().expect("callability checked above");
        if let Err(JSException(thrown)) = ctx.call_object(then, thenable, &[resolve_v, reject_v]) {
            // A throw out of `then` rejects the promise (spec
            // PromiseResolveThenableJob step 5).
            let _ = capability_settle(
                ctx.heap_mut(),
                &sink,
                promise,
                resolve_v,
                reject_v,
                thrown,
                true,
            );
        }
    })
}

/// `new Promise(executor)`.
///
/// The executor runs as a microtask job rather than synchronously (natives
/// cannot re-enter the interpreter): side effects land one checkpoint later.
/// Spec-conforming scripts that only observe ordering *within* the
/// asynchronous surface are unaffected. Calling `Promise` without `new`
/// throws per spec (prepare_construct passes the constructor as `this`; a
/// plain call's receiver never carries the construct target).
pub fn promise_construct(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let executor = args.first().copied().unwrap_or_else(JsValue::undefined);
    if !executor
        .as_object()
        .is_some_and(|o| ctx.heap.get(o).kind == Kind::Function)
    {
        return Err(ctx.type_error("Promise executor must be a function"));
    }
    let is_ctor = |heap: &Heap, v: JsValue| {
        v.as_object().is_some_and(|o| {
            matches!(
                &heap.get(o).callable,
                v12_heap::FunctionTarget::Bytecode(idx)
                    if *idx == u32::from(v12_native::NativeId::PromiseConstruct)
            )
        })
    };
    if !is_ctor(ctx.heap, this) {
        return Err(ctx.type_error("Promise constructor requires 'new'"));
    }
    let ctor = this.as_object().expect("checked above");
    let prototype = ctx.heap.get(ctor).prototype;
    let promise = create_promise(ctx.heap, prototype, STATE_PENDING, JsValue::undefined());
    let promise_v = JsValue::object(promise);

    let pending: Rc<RefCell<Vec<Job>>> = ctx.pending.clone().unwrap_or_default();

    // Capability: the resolve/reject function objects handed to the executor.
    // They are ordinary function objects whose targets are host closures
    // capturing the shared pending-jobs sink — host closures cannot touch the
    // interpreter, so reaction settling they trigger joins the same sink
    // `Promise#then` on a settled promise uses.
    let (resolve_v, reject_v) = make_capability(ctx.heap, &pending, promise);

    // The executor joins the pending sink: it runs at the next checkpoint
    // (see the divergence note above). A throw from the executor rejects the
    // promise.
    ctx.heap.add_root(executor);
    let executor_obj = executor.as_object().expect("checked above");
    let sink = Rc::clone(&pending);
    let p = promise;
    let rv = resolve_v;
    let jv = reject_v;
    pending
        .borrow_mut()
        .push(Box::new(move |ctx: &mut JobCtx<'_, '_>| {
            match ctx.call_object(executor_obj, JsValue::undefined(), &[resolve_v, reject_v]) {
                Ok(_) => {}
                Err(JSException(e)) => {
                    let _ = capability_settle(ctx.heap_mut(), &sink, p, rv, jv, e, true);
                }
            }
        }));
    Ok(promise_v)
}

/// Builds the `[[Resolve]]`/`[[Reject]]` capability pair for `promise`:
/// two rooted function objects whose host closures settle `promise` through
/// [`capability_settle`], scheduling derived reactions on `sink`.
pub(crate) fn make_capability(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
) -> (JsValue, JsValue) {
    // Placeholder target replaced with the host closure after both
    // capability objects exist (each closure needs both handles).
    let alloc_capability = |heap: &mut Heap| -> v12_heap::Handle<JsObject> {
        let func = heap.alloc(JsObject::function(
            v12_heap::FunctionTarget::Bytecode(u32::MAX),
            None,
        ));
        heap.add_root(JsValue::object(func));
        func
    };
    let resolve_obj = alloc_capability(heap);
    let reject_obj = alloc_capability(heap);
    let resolve_v = JsValue::object(resolve_obj);
    let reject_v = JsValue::object(reject_obj);

    let sink1 = Rc::clone(sink);
    let p = promise;
    let rv = resolve_v;
    let jv = reject_v;
    let resolve_closure = v12_heap::HostClosure::new(move |heap, _this, args| {
        let value = args.first().copied().unwrap_or_else(JsValue::undefined);
        capability_settle(heap, &sink1, p, rv, jv, value, false)
    });
    heap.get_mut(resolve_obj).callable = v12_heap::FunctionTarget::Host(resolve_closure);

    let sink2 = Rc::clone(sink);
    let p = promise;
    let rv = resolve_v;
    let jv = reject_v;
    let reject_closure = v12_heap::HostClosure::new(move |heap, _this, args| {
        let reason = args.first().copied().unwrap_or_else(JsValue::undefined);
        capability_settle(heap, &sink2, p, rv, jv, reason, true)
    });
    heap.get_mut(reject_obj).callable = v12_heap::FunctionTarget::Host(reject_closure);

    // Spec shape of both halves (27.5.1.3): they are `CreateBuiltinFunction`
    // results, so they must carry `length`/`name` and inherit
    // `%Function.prototype%` like any other builtin function.
    let mut ctx = Ctx::new(heap, None, None);
    shape_resolving_function(&mut ctx, resolve_obj);
    shape_resolving_function(&mut ctx, reject_obj);
    (resolve_v, reject_v)
}

/// Stamps the spec shape of a 27.5.1.3 resolving function onto `func`:
/// `length: 1`, then `name: ""`, and `[[Prototype]] = %Function.prototype%`.
///
/// The order of the two installs is observable — test262
/// `built-ins/Promise/prototype/then/reject-function-property-order.js` pins
/// `length` before `name` — so `length` goes first. No own `prototype` property
/// is created: that absence is what makes the function non-constructible
/// (27.5.1.3 step 1-a), and the engine already leaves fresh host functions
/// without one.
///
/// Shape only: no settlement, ordering, or combinator behaviour changes here.
pub(crate) fn shape_resolving_function(ctx: &mut Ctx, func: v12_heap::Handle<JsObject>) {
    if let Some(proto) = ctx.function_proto() {
        ctx.heap.get_mut(func).prototype = Some(proto);
    }
    ctx.define_data_prop_with_attrs(
        func,
        "length",
        JsValue::from_i32_smi(1).expect("resolving-function arity 1 fits Smi"),
        v12_heap::Attrs::new(false, false, true),
    );
    let empty = ctx.heap.intern_text("");
    ctx.define_data_prop_with_attrs(
        func,
        "name",
        JsValue::string(empty),
        v12_heap::Attrs::new(false, false, true),
    );
}

/// Settles an async function's completion promise and runs its reactions as
/// jobs on `sink`.
///
/// The interpreter cannot build job closures (they live in this crate), so
/// async-body completion pushes a settlement onto the interpreter's
/// pending-settlements queue and the engine's checkpoint drain routes it
/// here. First settlement wins (no-op once settled); a promise value is
/// adopted through the normal capability path.
pub(crate) fn settle_async_completion(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
    value: JsValue,
    rejecting: bool,
) {
    let still_pending = heap.get(promise).properties[0].as_smi() == Some(STATE_PENDING);
    if !still_pending {
        return;
    }
    let (resolve_v, reject_v) = make_capability(heap, sink, promise);
    let _ = capability_settle(heap, sink, promise, resolve_v, reject_v, value, rejecting);
}

/// `Promise.prototype.catch(on_rejected)`: `then(undefined, on_rejected)`.
pub fn promise_catch(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let on_rejected = args.first().copied().unwrap_or_else(JsValue::undefined);
    promise_then(ctx, this, &[JsValue::undefined(), on_rejected])
}

/// Settles `promise` synchronously from a builtin (`Ctx`) context: writes
/// the state/payload slots and moves the queued reaction jobs onto the
/// pending sink for the next checkpoint. First settlement wins (no-op once
/// settled). Mirrors [`settle`] (the `JobCtx` variant) without needing a
/// job context.
fn settle_sync(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
    state: i32,
    value: JsValue,
) {
    if heap.get(promise).properties[0].as_smi() != Some(STATE_PENDING) {
        return;
    }
    heap.get_mut(promise).properties[0] = smi(state);
    heap.get_mut(promise).properties[1] = value;
    let jobs = drain_reaction_jobs(heap, promise, state, value);
    sink.borrow_mut().extend(jobs);
}

/// Reads a promise value's settlement: `(handle, state, payload)`.
/// Returns `None` for non-promise values.
fn promise_settlement(
    heap: &Heap,
    v: JsValue,
) -> Option<(v12_heap::Handle<JsObject>, i32, JsValue)> {
    if !is_promise(heap, v) {
        return None;
    }
    let obj = v.as_object().expect("checked above");
    let o = heap.get(obj);
    Some((
        obj,
        o.properties[0].as_smi().unwrap_or(STATE_PENDING),
        o.properties[1],
    ))
}

/// Allocates a rooted `Kind::Function` object backed by a host closure.
/// The per-index watchers below run through `reaction_job`, which only
/// invokes `Kind::Function` callees via `call_object` — host targets need
/// no interpreter re-entry, exactly like the constructor capabilities.
fn alloc_host(
    heap: &mut Heap,
    f: impl FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue> + 'static,
) -> JsValue {
    let func = heap.alloc(JsObject::function(
        v12_heap::FunctionTarget::Host(v12_heap::HostClosure::new(f)),
        None,
    ));
    heap.add_root(JsValue::object(func));
    JsValue::object(func)
}

/// Appends a `[fulfill, reject, derived]` reaction record to a pending
/// promise's reactions array (the record shape `promise_then` uses). The
/// derived slot holds an unobservable dummy promise: `reaction_job`
/// settles it with each handler's return and nothing ever reads it.
fn push_record(
    heap: &mut Heap,
    input: v12_heap::Handle<JsObject>,
    fulfill: JsValue,
    reject: JsValue,
) {
    let dummy = create_promise(heap, None, STATE_PENDING, JsValue::undefined());
    let record = heap.alloc(JsObject::ordinary(
        smallvec::smallvec![fulfill, reject, JsValue::object(dummy)],
        smallvec::smallvec![None; 3],
    ));
    heap.add_root(JsValue::object(record));
    if let Some(reactions) = heap.get(input).properties[2].as_object() {
        heap.get_mut(reactions)
            .elements
            .push(JsValue::object(record));
    }
}

/// `Promise.all(iterable)`: fulfills with an array of results once every
/// input fulfills, or rejects with the first rejection.
///
/// Input contract: a `Kind::Array` of already-collected values. In the
/// engine the interpreter's `run_promise_combinator` seam resolves the real
/// `@@iterator`, drives `next`/`return` on it (the half that re-enters the
/// machine), and hands the drained values over as a canonical array — so the
/// aggregation below never sees a general iterable. Anything else is a
/// `TypeError`, which keeps the handler usable as a standalone fallback for
/// The streaming `Promise.all` / `Promise.race` seam (spec 27.2.4.1 /
/// 27.2.4.2, `PerformPromiseAll` / `PerformPromiseRace`).
///
/// The combinator runs from both sides at once, so its state lives on the heap
/// (slot names in [`v12_native::promise_combinator`]): the interpreter pulls
/// iterator steps and invokes `C.resolve` and `then` — each needs the machine —
/// while the aggregate promise, the results array and the reaction watchers
/// live here. One element at a time, which is what lets an abrupt element
/// settle the combinator instead of stalling on an unbounded iterator.
pub mod combinator {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use v12_heap::{Handle, Heap, JsObject, JsValue, Kind};
    use v12_native::Throw;
    use v12_native::promise_combinator::{
        KIND_ALL, KIND_RACE, SLOT_AGGREGATE, SLOT_KIND, SLOT_REJECT, SLOT_REMAINING, SLOT_RESULTS,
        STATE_SLOTS,
    };

    use super::{
        STATE_FULFILLED, STATE_PENDING, STATE_REJECTED, alloc_host, capability_settle,
        create_promise, make_capability, settle_sync, shape_resolving_function, smi,
    };
    use crate::builtins::ctx::Ctx;
    use crate::job_queue::Job;

    /// Reads slot `index` of a combinator state object.
    fn slot(heap: &Heap, state: Handle<JsObject>, index: i32) -> JsValue {
        heap.get(state)
            .properties
            .get(index as usize)
            .copied()
            .unwrap_or_else(JsValue::undefined)
    }

    /// `(state, aggregate promise, kind)` of a state object, or a `TypeError`
    /// when `state_v` is not one.
    fn parts(
        heap: &Heap,
        state_v: JsValue,
    ) -> Result<(Handle<JsObject>, Handle<JsObject>, i32), Throw> {
        let Some(state) = state_v.as_object() else {
            return Err(Throw::Message(
                "TypeError: Promise combinator state is not an object".into(),
            ));
        };
        let kind = v12_native::promise_combinator::combinator_kind(slot(heap, state, SLOT_KIND))
            .ok_or_else(|| {
                Throw::Message("TypeError: Promise combinator state has no kind".into())
            })?;
        let aggregate = slot(heap, state, SLOT_AGGREGATE)
            .as_object()
            .ok_or_else(|| {
                Throw::Message("TypeError: Promise combinator state has no promise".into())
            })?;
        Ok((state, aggregate, kind))
    }

    /// `combinator_begin`: the aggregate promise, its results array and the
    /// capability reject function, in one state object the interpreter drives
    /// element by element.
    ///
    /// This is the allocating half of `NewPromiseCapability(C)` (spec
    /// 27.2.4.1 step 6). Its `IsConstructor(C)` gate is applied by the caller
    /// — `Interp::is_capability_constructor` — which owns the interpreter's
    /// constructor oracle and runs it before `GetIterator`, so a
    /// non-constructor receiver never reaches this function.
    pub fn begin(ctx: &mut Ctx, this: JsValue, kind: i32) -> Result<JsValue, Throw> {
        if kind != KIND_ALL && kind != KIND_RACE {
            return Err(Throw::Message(
                "TypeError: unknown Promise combinator".into(),
            ));
        }
        let heap = &mut *ctx.heap;
        let sink = ctx.pending.clone().unwrap_or_default();
        // `this` is the receiver C: its `prototype` field hosts the aggregate's
        // `[[Prototype]]`, exactly as the array handlers did.
        let prototype = this.as_object().and_then(|ctor| heap.get(ctor).prototype);
        let aggregate = create_promise(heap, prototype, STATE_PENDING, JsValue::undefined());
        let (_, reject_v) = make_capability(heap, &sink, aggregate);
        // `KIND_RACE` never reads the results array, but keeping the layout
        // uniform means no slot is position-dependent.
        let results = heap.alloc(JsObject::array(Vec::new()));
        heap.add_root(JsValue::object(results));
        let mut state = JsObject::default();
        for _ in 0..STATE_SLOTS {
            state.properties.push(JsValue::undefined());
            state.property_keys.push(None);
        }
        state.properties[SLOT_KIND as usize] = smi(kind);
        state.properties[SLOT_AGGREGATE as usize] = JsValue::object(aggregate);
        state.properties[SLOT_REJECT as usize] = reject_v;
        state.properties[SLOT_RESULTS as usize] = JsValue::object(results);
        // The unsettled count starts at 1: a token for "the iterator has not
        // been exhausted yet". `step` adds one per element and each element
        // settlement takes one away, so the count only reaches zero once every
        // pulled element has settled *and* exhaustion has been recorded — the
        // element count is not known up front, so the token is what keeps a
        // first element from fulfilling the aggregate mid-iteration.
        //
        // `KIND_RACE` never moves the count: its spec loop settles the
        // aggregate on the first element and returns the capability promise
        // unchanged at exhaustion (27.2.4.2 step 8-f), so nothing reads it.
        state.properties[SLOT_REMAINING as usize] = smi(1);
        let handle = heap.alloc(state);
        heap.add_root(JsValue::object(handle));
        Ok(JsValue::object(handle))
    }

    /// One element of a combinator: records `value` at `index`, counts it
    /// unsettled, and builds the `[resolveElement, rejectElement]` pair for it.
    /// Returns that pair for the interpreter to hand to `then`, or `undefined`
    /// when `then` is not callable and there is nothing to invoke.
    ///
    /// `then` is the element's *only* reaction path, exactly as the spec has
    /// it (27.2.4.1 step 11-b / 27.2.4.2 step 8-f). An earlier revision also
    /// appended an engine watcher record here, which double-registered every
    /// promise element: the pair then fired from a record the spec never
    /// installs, so an already-settled input settled the aggregate a job
    /// earlier than `then` alone and the whole tail of the microtask sequence
    /// shifted (`built-ins/Promise/race/resolved-sequence*`). It also settled
    /// elements whose own `then` is a no-op, which the spec leaves pending.
    ///
    /// The pair stays first-wins per element (`settled`) because that is what
    /// `NewPromiseCapability`'s resolve/reject are: a `then` free to call both
    /// halves must not decrement `SLOT_REMAINING` twice and fulfill the
    /// aggregate with a hole in it.
    pub fn step(
        ctx: &mut Ctx,
        state_v: JsValue,
        index: u32,
        value: JsValue,
        then_v: JsValue,
    ) -> Result<JsValue, Throw> {
        let (state, aggregate, kind) = parts(ctx.heap, state_v)?;
        let sink = ctx.pending.clone().unwrap_or_default();
        let callable = then_v
            .as_object()
            .is_some_and(|f| matches!(ctx.heap.get(f).kind, Kind::Function | Kind::Proxy));
        if kind == KIND_ALL {
            // The slot starts out holding the value itself, which is what it
            // resolves to when `then` is not callable (spec: "Set result[index]
            // to nextPromise").
            if let Some(results) = slot(ctx.heap, state, SLOT_RESULTS).as_object() {
                set_result(ctx.heap, results, index, value);
            }
            let remaining = slot(ctx.heap, state, SLOT_REMAINING)
                .as_smi()
                .unwrap_or(0)
                .saturating_add(1);
            ctx.heap.get_mut(state).properties[SLOT_REMAINING as usize] = smi(remaining);
        }
        if !callable {
            if kind == KIND_ALL {
                drop_unsettled(ctx.heap, &sink, state);
            } else {
                // `KIND_RACE` with a non-callable `then`: the value itself
                // settles the combinator, first one wins.
                settle_sync(ctx.heap, &sink, aggregate, STATE_FULFILLED, value);
            }
            return Ok(JsValue::undefined());
        }
        let settled = Rc::new(Cell::new(false));
        let (resolve_v, reject_v) = match kind {
            KIND_ALL => all_element(ctx, state, index, &settled, &sink),
            _ => race_element(ctx, state, &settled, &sink),
        };
        // 27.5.4.1/27.5.4.5 build the per-element pair with
        // `CreateResolvingFunction` too, and hand it to a user-visible `then`,
        // so it carries the same spec shape as an executor's capability.
        shape_pair(ctx, resolve_v, reject_v);
        let pair = ctx.heap.alloc(JsObject::array(vec![resolve_v, reject_v]));
        ctx.heap.add_root(JsValue::object(pair));
        Ok(JsValue::object(pair))
    }

    /// Stamps the 27.5.1.3 resolving-function shape on a combinator
    /// `[resolveElement, rejectElement]` pair.
    fn shape_pair(ctx: &mut Ctx, resolve: JsValue, reject: JsValue) {
        for half in [resolve, reject] {
            let obj = half.as_object().expect("alloc_host returns a function");
            shape_resolving_function(ctx, obj);
        }
    }

    /// `combinator_settle`: `KIND_ALL` exhaustion fulfills the aggregate with
    /// the results array once every element has settled; the abrupt path
    /// rejects it with `reason`. Returns the aggregate promise — what the
    /// caller hands back to script.
    pub fn settle(
        ctx: &mut Ctx,
        state_v: JsValue,
        rejecting: bool,
        reason: JsValue,
    ) -> Result<JsValue, Throw> {
        let (state, aggregate, kind) = parts(ctx.heap, state_v)?;
        let sink = ctx.pending.clone().unwrap_or_default();
        if rejecting {
            // Through the state's own capability reject, so the reaction jobs it
            // schedules come from the same path an executor's reject takes. The
            // resolve half is never read on this path (a rejection adopts
            // nothing), so `undefined` stands in for it.
            let reject_v = slot(ctx.heap, state, SLOT_REJECT);
            let _ = capability_settle(
                ctx.heap,
                &sink,
                aggregate,
                JsValue::undefined(),
                reject_v,
                reason,
                true,
            );
        } else if kind == KIND_ALL {
            // Exhaustion: fulfill if nothing is still unsettled. Elements that
            // settle later fulfill it from their own closure.
            drop_unsettled(ctx.heap, &sink, state);
        }
        // `KIND_RACE` on exhaustion: the spec returns the capability promise
        // as it stands (27.2.4.2 step 8-f) — a still-pending race stays
        // pending until an element settles, and an already-settled one keeps
        // the winner `then` gave it. Dropping the unsettled count here would
        // instead fulfill the aggregate with the (never written) results
        // array, which is how `Promise.race([settledPromise])` used to resolve
        // to `[]` before its first element's reaction job ran and lose every
        // ordering race that followed.
        Ok(JsValue::object(aggregate))
    }

    /// Stores one result at `index`, keeping the array's `length` current.
    /// `length` is `properties[0]` for an array, and the results array is
    /// allocated empty (the element count is not known up front), so unlike
    /// the interpreter's `array_set_element` nothing else maintains it.
    fn set_result(heap: &mut Heap, results: Handle<JsObject>, index: u32, value: JsValue) {
        let before = heap.get(results).element_len() as u32;
        heap.get_mut(results).set_element(index, value);
        let after = heap.get(results).element_len() as u32;
        if after > before
            && let Some(length) = heap.get_mut(results).properties.first_mut()
        {
            *length = JsValue::from_i32_smi(after as i32).expect("array length fits Smi");
        }
    }

    /// `KIND_ALL`'s per-element pair: store the value at `index`, then drop the
    /// element off the unsettled count. The last one fulfills the aggregate
    /// with the results array.
    fn all_element(
        ctx: &mut Ctx,
        state: Handle<JsObject>,
        index: u32,
        settled: &Rc<Cell<bool>>,
        sink: &Rc<RefCell<Vec<Job>>>,
    ) -> (JsValue, JsValue) {
        let resolve_sink = Rc::clone(sink);
        let resolve_flag = Rc::clone(settled);
        let resolve = alloc_host(ctx.heap, move |heap, _this, args| {
            if resolve_flag.get() {
                return Ok(JsValue::undefined());
            }
            resolve_flag.set(true);
            let value = args.first().copied().unwrap_or_else(JsValue::undefined);
            if let Some(results) = slot(heap, state, SLOT_RESULTS).as_object() {
                set_result(heap, results, index, value);
            }
            drop_unsettled(heap, &resolve_sink, state);
            Ok(JsValue::undefined())
        });
        let reject_sink = Rc::clone(sink);
        let reject_flag = Rc::clone(settled);
        let reject = alloc_host(ctx.heap, move |heap, _this, args| {
            if reject_flag.get() {
                return Ok(JsValue::undefined());
            }
            reject_flag.set(true);
            // An element rejection rejects the aggregate; `settle_sync` is
            // first-wins, so a later element cannot revive it. The unsettled
            // count is left alone — the aggregate will never fulfill now.
            let reason = args.first().copied().unwrap_or_else(JsValue::undefined);
            if let Some(aggregate) = slot(heap, state, SLOT_AGGREGATE).as_object() {
                settle_sync(heap, &reject_sink, aggregate, STATE_REJECTED, reason);
            }
            Ok(JsValue::undefined())
        });
        (resolve, reject)
    }

    /// `KIND_RACE`'s per-element pair: the first callback wins.
    fn race_element(
        ctx: &mut Ctx,
        state: Handle<JsObject>,
        settled: &Rc<Cell<bool>>,
        sink: &Rc<RefCell<Vec<Job>>>,
    ) -> (JsValue, JsValue) {
        let resolve_sink = Rc::clone(sink);
        let resolve_flag = Rc::clone(settled);
        let resolve = alloc_host(ctx.heap, move |heap, _this, args| {
            if resolve_flag.get() {
                return Ok(JsValue::undefined());
            }
            resolve_flag.set(true);
            let value = args.first().copied().unwrap_or_else(JsValue::undefined);
            if let Some(aggregate) = slot(heap, state, SLOT_AGGREGATE).as_object() {
                settle_sync(heap, &resolve_sink, aggregate, STATE_FULFILLED, value);
            }
            Ok(JsValue::undefined())
        });
        let reject_sink = Rc::clone(sink);
        let reject_flag = Rc::clone(settled);
        let reject = alloc_host(ctx.heap, move |heap, _this, args| {
            if reject_flag.get() {
                return Ok(JsValue::undefined());
            }
            reject_flag.set(true);
            let reason = args.first().copied().unwrap_or_else(JsValue::undefined);
            if let Some(aggregate) = slot(heap, state, SLOT_AGGREGATE).as_object() {
                settle_sync(heap, &reject_sink, aggregate, STATE_REJECTED, reason);
            }
            Ok(JsValue::undefined())
        });
        (resolve, reject)
    }

    /// Drops one element off `SLOT_REMAINING` and fulfills the aggregate with
    /// the results array when the count reaches zero. No-op afterwards: once
    /// the aggregate has settled, `settle_sync` refuses.
    fn drop_unsettled(heap: &mut Heap, sink: &Rc<RefCell<Vec<Job>>>, state: Handle<JsObject>) {
        let remaining = slot(heap, state, SLOT_REMAINING)
            .as_smi()
            .unwrap_or(1)
            .saturating_sub(1);
        heap.get_mut(state).properties[SLOT_REMAINING as usize] = smi(remaining);
        if remaining > 0 {
            return;
        }
        if let Some(aggregate) = slot(heap, state, SLOT_AGGREGATE).as_object() {
            let results = slot(heap, state, SLOT_RESULTS);
            settle_sync(heap, sink, aggregate, STATE_FULFILLED, results);
        }
    }
}

/// `Promise.all(iterable)` — the standalone array-input handler.
///
/// The interpreter's streaming seam (`Interp::run_promise_combinator`) is the
/// live path: it drives `combinator_begin`/`_step`/`_settle` above. This
/// handler stays for a bare-heap embedder with no interpreter to drive them,
/// and reads its inputs from a `Kind::Array` — anything else is a `TypeError`.
/// Pending promise inputs are watched by host-closure reaction records appended
/// to the input's own reactions (the adoption-record shape
/// `capability_settle` uses): no polling jobs, so never-settling inputs cost
/// nothing and cannot spin the drain. Arbitrary thenable unwrapping and
/// `Symbol.species` selection of the constructor are out of scope.
pub fn promise_all(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let iterable = args.first().copied().unwrap_or_else(JsValue::undefined);
    let Some(iter_obj) = iterable.as_object() else {
        return Err(ctx.type_error("Promise.all requires an iterable"));
    };
    if ctx.heap.get(iter_obj).kind != Kind::Array {
        return Err(ctx.type_error("Promise.all requires an iterable"));
    }
    let inputs: Vec<JsValue> = ctx.heap.get(iter_obj).elements_snapshot();
    let prototype = this
        .as_object()
        .and_then(|ctor| ctx.heap.get(ctor).prototype);
    let aggregate = create_promise(ctx.heap, prototype, STATE_PENDING, JsValue::undefined());
    let aggregate_v = JsValue::object(aggregate);
    let results = ctx
        .heap
        .alloc(JsObject::array(vec![JsValue::undefined(); inputs.len()]));
    ctx.heap.add_root(JsValue::object(results));
    let results_v = JsValue::object(results);
    let sink = ctx.pending.clone().unwrap_or_default();

    let remaining = Rc::new(RefCell::new(inputs.len()));
    for (index, input) in inputs.into_iter().enumerate() {
        match promise_settlement(ctx.heap, input) {
            Some((_, STATE_FULFILLED, payload)) => {
                ctx.heap.get_mut(results).set_element(index as u32, payload);
                *remaining.borrow_mut() -= 1;
            }
            Some((input_obj, STATE_PENDING, _)) => {
                let res_h = results;
                let rem = Rc::clone(&remaining);
                let agg = aggregate;
                let res_v = results_v;
                let sink_f = Rc::clone(&sink);
                let fulfill = alloc_host(ctx.heap, move |heap, _this, args| {
                    let payload = args.first().copied().unwrap_or_else(JsValue::undefined);
                    heap.get_mut(res_h).set_element(index as u32, payload);
                    *rem.borrow_mut() -= 1;
                    if *rem.borrow() == 0 {
                        settle_sync(heap, &sink_f, agg, STATE_FULFILLED, res_v);
                    }
                    Ok(JsValue::undefined())
                });
                let agg = aggregate;
                let sink_r = Rc::clone(&sink);
                let reject = alloc_host(ctx.heap, move |heap, _this, args| {
                    let reason = args.first().copied().unwrap_or_else(JsValue::undefined);
                    settle_sync(heap, &sink_r, agg, STATE_REJECTED, reason);
                    Ok(JsValue::undefined())
                });
                push_record(ctx.heap, input_obj, fulfill, reject);
            }
            Some((_, _, reason)) => {
                // Already-rejected input: the aggregate rejects at once
                // (first rejection wins; pending inputs' watchers no-op
                // against the settled aggregate).
                settle_sync(ctx.heap, &sink, aggregate, STATE_REJECTED, reason);
                return Ok(aggregate_v);
            }
            None => {
                ctx.heap.get_mut(results).set_element(index as u32, input);
                *remaining.borrow_mut() -= 1;
            }
        }
    }
    if *remaining.borrow() == 0 {
        settle_sync(ctx.heap, &sink, aggregate, STATE_FULFILLED, results_v);
    }
    Ok(aggregate_v)
}

/// `Promise.race(iterable)`: settles with the first input's outcome.
///
/// Same array-of-collected-values input contract as [`promise_all`]: the
/// interpreter seam resolves the general iterable before the handler sees
/// it. Already-settled inputs (and non-promise inputs, which count as
/// fulfilled) settle the aggregate synchronously in iteration order; pending
/// inputs are watched by host-closure reaction records, first settlement
/// winning.
pub fn promise_race(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let iterable = args.first().copied().unwrap_or_else(JsValue::undefined);
    let Some(iter_obj) = iterable.as_object() else {
        return Err(ctx.type_error("Promise.race requires an iterable"));
    };
    if ctx.heap.get(iter_obj).kind != Kind::Array {
        return Err(ctx.type_error("Promise.race requires an iterable"));
    }
    let inputs: Vec<JsValue> = ctx.heap.get(iter_obj).elements_snapshot();
    let prototype = this
        .as_object()
        .and_then(|ctor| ctx.heap.get(ctor).prototype);
    let aggregate = create_promise(ctx.heap, prototype, STATE_PENDING, JsValue::undefined());
    let aggregate_v = JsValue::object(aggregate);
    let sink = ctx.pending.clone().unwrap_or_default();
    for input in inputs {
        match promise_settlement(ctx.heap, input) {
            Some((_, STATE_FULFILLED, payload)) => {
                settle_sync(ctx.heap, &sink, aggregate, STATE_FULFILLED, payload);
                return Ok(aggregate_v);
            }
            Some((input_obj, STATE_PENDING, _)) => {
                let agg = aggregate;
                let sink_f = Rc::clone(&sink);
                let fulfill = alloc_host(ctx.heap, move |heap, _this, args| {
                    let payload = args.first().copied().unwrap_or_else(JsValue::undefined);
                    settle_sync(heap, &sink_f, agg, STATE_FULFILLED, payload);
                    Ok(JsValue::undefined())
                });
                let agg = aggregate;
                let sink_r = Rc::clone(&sink);
                let reject = alloc_host(ctx.heap, move |heap, _this, args| {
                    let reason = args.first().copied().unwrap_or_else(JsValue::undefined);
                    settle_sync(heap, &sink_r, agg, STATE_REJECTED, reason);
                    Ok(JsValue::undefined())
                });
                push_record(ctx.heap, input_obj, fulfill, reject);
            }
            Some((_, _, reason)) => {
                settle_sync(ctx.heap, &sink, aggregate, STATE_REJECTED, reason);
                return Ok(aggregate_v);
            }
            None => {
                settle_sync(ctx.heap, &sink, aggregate, STATE_FULFILLED, input);
                return Ok(aggregate_v);
            }
        }
    }
    Ok(aggregate_v)
}

/// `Promise.prototype.finally(on_finally)`: runs `on_finally` when `this`
/// settles, passing the original outcome through.
///
/// The derived promise pends until `this` settles. Settlement is watched
/// by host-closure reaction records: each watcher enqueues one real job
/// that runs the callback via `JobCtx::call_object` (so user functions
/// work without builtin-time interpreter re-entry) and settles the derived
/// promise — with the original outcome, or with the callback's thrown
/// value when the callback throws. An already-settled `this` enqueues that
/// job directly. No polling, so a never-settling `this` costs nothing.
pub fn promise_finally(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    if !is_promise(ctx.heap, this) {
        return Err(ctx.type_error("Promise.prototype.finally requires a promise"));
    }
    let promise = this.as_object().expect("checked above");
    let on_finally = args.first().copied().unwrap_or_else(JsValue::undefined);
    ctx.heap.add_root(on_finally);
    let prototype = ctx.heap.get(promise).prototype;
    let derived = create_promise(ctx.heap, prototype, STATE_PENDING, JsValue::undefined());
    let sink = ctx.pending.clone().unwrap_or_default();
    let (state, payload) = {
        let o = ctx.heap.get(promise);
        (
            o.properties[0].as_smi().unwrap_or(STATE_PENDING),
            o.properties[1],
        )
    };
    if state == STATE_PENDING {
        let sink_f = Rc::clone(&sink);
        let fulfill = alloc_host(ctx.heap, move |_heap, _this, args| {
            let value = args.first().copied().unwrap_or_else(JsValue::undefined);
            sink_f.borrow_mut().push(finally_settle_job(
                derived,
                on_finally,
                STATE_FULFILLED,
                value,
            ));
            Ok(JsValue::undefined())
        });
        let sink_r = Rc::clone(&sink);
        let reject = alloc_host(ctx.heap, move |_heap, _this, args| {
            let reason = args.first().copied().unwrap_or_else(JsValue::undefined);
            sink_r.borrow_mut().push(finally_settle_job(
                derived,
                on_finally,
                STATE_REJECTED,
                reason,
            ));
            Ok(JsValue::undefined())
        });
        push_record(ctx.heap, promise, fulfill, reject);
    } else {
        sink.borrow_mut()
            .push(finally_settle_job(derived, on_finally, state, payload));
    }
    Ok(JsValue::object(derived))
}

/// One settlement job for [`promise_finally`]: calls the callback (when
/// callable) and settles `derived` with the watched outcome — or with the
/// callback's thrown value when the callback throws.
fn finally_settle_job(
    derived: v12_heap::Handle<JsObject>,
    on_finally: JsValue,
    state: i32,
    payload: JsValue,
) -> Job {
    Box::new(move |ctx: &mut JobCtx<'_, '_>| {
        let callable = on_finally
            .as_object()
            .is_some_and(|h| ctx.heap_mut().get(h).kind == Kind::Function);
        if callable {
            let handler = on_finally.as_object().expect("checked above");
            if let Err(JSException(thrown)) = ctx.call_object(handler, JsValue::undefined(), &[]) {
                settle(ctx, derived, STATE_REJECTED, thrown);
                return;
            }
        }
        settle(ctx, derived, state, payload);
    })
}
