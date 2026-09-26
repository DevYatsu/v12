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
//! `new Promise(executor)` splits across the two crates: the capability
//! ([`promise_capability`] — the promise plus the resolving pair) is engine
//! work, and the executor call itself is `Interp::run_promise_construct`,
//! because calling user code needs the machine. Spec 27.5.1.1 step 5 wants
//! that call *synchronous*, which is why it does not go through a job the way
//! the thenable adoption below does.
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
//! pinned at creation with [`Heap::add_pinned_root`]: jobs run after the
//! program's operand stack is gone, so everything a job reaches must survive
//! collection. This trades reclamation for soundness (the objects are small
//! and few in harness workloads) — the pinning is permanent, with no unpin
//! path, precisely because there is no point at which the engine can know a
//! job will never run again.
//!
//! Why pinned and not [`Heap::add_root`]: the ordinary root set is transient.
//! `Interp::gc_protect` republishes it wholesale at every safepoint — it
//! `roots.clear()`s first and then re-adds only the operand stack, the frames,
//! the interpreter's pending-await/settlement/adoption queues, the last result
//! and its cached natives — so any root an embedder added is gone by the time
//! the `heap.safepoint()` inside that same call collects. A promise reachable
//! only from a job closure (the collector cannot see `Box<dyn FnOnce(&mut
//! JobCtx)>`) therefore died there, and the next `settle` wrote through a
//! stale handle. The pinned set is marked separately and is never cleared.
//!
//! Still out of scope: `Symbol.species` constructor selection (every promise
//! here uses `%Promise%` or the receiver's own `prototype`), and adoption
//! when a *reaction handler's* return value is a thenable (`settle` writes
//! the handler's value straight into the derived promise).

use std::cell::RefCell;
use std::rc::Rc;

use v12_heap::{Heap, JsObject, JsValue, Kind};
use v12_native::Throw;
use v12_native::promise_capability::{
    CAPABILITY_USED, SLOT_ALREADY_RESOLVED, SLOT_REJECT as CAP_SLOT_REJECT,
    SLOT_RESOLVE as CAP_SLOT_RESOLVE,
};

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

/// Pins an object the module hands to a job, reaction record, or capability
/// closure. Non-object values (a non-callable `finally` callback, say) have no
/// handle to pin and cannot be called from a job either, so they are skipped.
fn pin(heap: &mut Heap, value: JsValue) {
    if let Some(obj) = value.as_object() {
        heap.add_pinned_root(obj);
    }
}

/// Pins the three values a [`reaction_job`] closure captures.
///
/// A `Box<dyn FnOnce(&mut JobCtx)>` is opaque to the collector, so a job's
/// captures are roots by virtue of being pinned here and nowhere else. The
/// handler and the payload are the two that are *not* reachable from a pinned
/// promise: the record that holds a handler is drained (its elements taken)
/// before the job runs, and the settled promise carrying the payload may be
/// unreachable from script the moment `then` returns.
fn pin_job_captures(
    heap: &mut Heap,
    handler: JsValue,
    payload: JsValue,
    derived: v12_heap::Handle<JsObject>,
) {
    pin(heap, handler);
    pin(heap, payload);
    heap.add_pinned_root(derived);
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
    heap.add_pinned_root(reactions);
    // `alloc_promise` binds the reserved promise shape, so a user-defined own
    // property on this object is numbered past `[[State]]`/`[[Result]]`/
    // `[[Reactions]]` instead of aliasing them.
    let promise = heap.alloc_promise(JsObject {
        kind: v12_heap::Kind::Promise,
        properties: smallvec::smallvec![smi(state), payload, JsValue::object(reactions)],
        prototype,
        ..JsObject::default()
    });
    heap.add_pinned_root(promise);
    promise
}

/// The instance a `Promise` construction initializes, when the receiver is
/// already the object to initialize (spec 27.5.1.1, arm B of
/// [`promise_capability`]).
///
/// `None` means arm A: allocate a fresh promise. That covers the constructor
/// itself, every other callable receiver, and a proxy in front of one — a
/// proxy is callable, so it belongs on the arm-A side even though it is neither
/// a `Kind::Function` nor a promise, and turning one into a `Kind::Promise`
/// would be meaningless.
fn in_place_instance(heap: &Heap, this: JsValue) -> Option<v12_heap::Handle<JsObject>> {
    let instance = this.as_object()?;
    if !matches!(heap.get(instance).kind, Kind::Promise)
        && !matches!(heap.get(instance).kind, Kind::Function | Kind::Proxy)
        && !is_promise(heap, this)
    {
        Some(instance)
    } else {
        None
    }
}

/// Arm B of [`promise_capability`]: turns `instance` into a pending promise
/// where it stands.
///
/// `create_promise` is exactly these three fields plus an allocation, and the
/// allocation is the part arm B does not do — spec 9.7.1's `[[Construct]]`
/// chain already made the object (`OrdinaryCreateFromConstructor`) before the
/// base constructor ran, so the base fills it in. The receiver is therefore
/// fresh: no own property exists to lose, which is why `property_keys` is left
/// alone rather than resized to match.
fn init_promise_in_place(
    heap: &mut Heap,
    instance: v12_heap::Handle<JsObject>,
) -> v12_heap::Handle<JsObject> {
    let reactions = heap.alloc(JsObject::array(Vec::new()));
    heap.add_pinned_root(reactions);
    // The reserved promise shape, as `alloc_promise` binds it, so a
    // user-defined own property added after `super(...)` is numbered past the
    // three internal slots instead of aliasing them.
    let promise_shape = heap.promise_shape();
    let obj = heap.get_mut(instance);
    obj.shape = promise_shape;
    obj.kind = Kind::Promise;
    obj.properties = smallvec::smallvec![
        smi(STATE_PENDING),
        JsValue::undefined(),
        JsValue::object(reactions)
    ];
    // `[[Prototype]]` stays as the construct chain set it (`new_target.prototype`).
    heap.add_pinned_root(instance);
    instance
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
            ctx.heap.add_pinned_root(record);
            ctx.heap
                .get_mut(reactions)
                .elements
                .push(JsValue::object(record));
        }
        STATE_FULFILLED => enqueue_reaction(
            ctx.heap,
            &mut |job| sink.borrow_mut().push(job),
            handler,
            payload,
            derived,
            false,
        ),
        _ => enqueue_reaction(
            ctx.heap,
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
    // Pin the callback: the job outlives the program stack that referenced it.
    pin(ctx.heap, cb);
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
    heap: &mut Heap,
    push: &mut dyn FnMut(Job),
    handler: JsValue,
    payload: JsValue,
    derived: v12_heap::Handle<JsObject>,
    rejecting: bool,
) {
    // A job closure is invisible to the collector, and `this`'s own promise
    // (and any record on it) can be reclaimed the moment the drain moves on,
    // so every value the job captures is pinned here — not just the record.
    pin_job_captures(heap, handler, payload, derived);
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
        pin_job_captures(heap, handler, value, derived);
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
    settle_with_adoption(
        heap, sink, promise, resolve_v, reject_v, value, rejecting, true,
    )
}

/// [`capability_settle`] with the thenable-adoption step switched off: `value`
/// is taken as the resolution itself, so a value that is *already* known not to
/// be adoptable cannot loop back into the adoption that produced it.
///
/// The one caller is `thenable_job`'s "present but non-callable `then`" branch
/// (spec 27.2.1.3.1 step 3-b-i: `FulfillPromise(promise, resolution)`). Going
/// through `capability_settle` there re-entered `start_thenable_adoption`,
/// which re-read the same non-callable `then` and queued another job, forever:
/// `new Promise(r => r({then: null}))` burned the whole run deadline without
/// ever settling. Pinned by `Promise/resolve-non-thenable-immed.js`.
#[allow(clippy::too_many_arguments)]
fn settle_now(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
    resolve_v: JsValue,
    reject_v: JsValue,
    value: JsValue,
    rejecting: bool,
) -> Result<JsValue, JsValue> {
    settle_with_adoption(
        heap, sink, promise, resolve_v, reject_v, value, rejecting, false,
    )
}

/// The shared body of [`capability_settle`] and [`settle_now`]. `may_adopt`
/// selects whether a thenable `value` is handed to a [`thenable_job`] instead
/// of settling `promise` outright.
#[allow(clippy::too_many_arguments)]
fn settle_with_adoption(
    heap: &mut Heap,
    sink: &Rc<RefCell<Vec<Job>>>,
    promise: v12_heap::Handle<JsObject>,
    resolve_v: JsValue,
    reject_v: JsValue,
    value: JsValue,
    rejecting: bool,
    may_adopt: bool,
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
        heap.add_pinned_root(record);
        if let Some(reactions) = heap.get(value_obj).properties[2].as_object() {
            heap.get_mut(reactions)
                .elements
                .push(JsValue::object(record));
        }
        return Ok(JsValue::undefined());
    }
    if may_adopt && !rejecting && start_thenable_adoption(heap, sink, promise, value) {
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
            pin(heap, value);
            heap.add_pinned_root(promise);
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
    // Pinned per the module contract: the job outlives the program stack that
    // built the thenable, and the sink is invisible to the collector.
    pin(heap, thenable);
    heap.add_pinned_root(promise);
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
            //
            // `settle_now`, not `capability_settle`: this branch already *is*
            // the adoption, so the full path would read the same non-callable
            // `then` again and queue this very job once more, forever.
            let _ = settle_now(
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

/// `NewPromiseCapability(C)` for `new Promise(executor)`: the pending promise
/// plus the `[[Resolve]]`/`[[Reject]]` pair the executor receives (spec
/// 27.5.1.1 step 3, `NewPromiseCapability` 27.2.3.1).
///
/// The three values travel back as one heap array — the same carrier
/// `combinator::step` uses for its pair — because a registry seam hands over
/// a single value. The executor itself is *not* run here: calling user code
/// needs the machine, so 27.5.1.1 step 5 belongs to
/// `Interp::run_promise_construct`, which owns the synchronous call.
///
/// The spec gates are the caller's: `Interp::run_promise_construct` applies
/// `IsConstructor(C)` through `Interp::is_capability_constructor` (the same
/// oracle the combinators use, so a proxy in front of `Promise` is honored)
/// and `IsCallable(executor)` before it gets here, exactly as
/// `combinator::begin`'s `IsConstructor` gate is applied by
/// `Interp::run_promise_combinator`.
///
/// The receiver picks the arm, per [`in_place_instance`]:
///
/// - **Arm A** — the receiver is the constructor (or a callable wrapper on it),
///   so a *fresh* promise is allocated with `[[Prototype]]` read off the
///   receiver's `prototype` field, which is where `%Promise.prototype%` lives.
/// - **Arm B** — the receiver is already the instance a derived constructor's
///   `super(executor)` made, so it is initialized in place and keeps the
///   `[[Prototype]]` the construct chain gave it (`newTarget.prototype`).
///   Without this arm a subclass's promise was a different object from the one
///   its constructor body had been given, and `super(executor)` threw
///   `TypeError: Promise constructor requires 'new'` on the way.
///
///   Arm B is also what `Promise.call(obj, executor)` and
///   `Reflect.apply(Promise, obj, « executor »)` reach, because a derived
///   `super(…)` is lowered to a `Call` and is therefore indistinguishable from
///   them at this seam: all three initialize `obj` as a promise, where a real
///   engine throws for the first two (`%Promise%` has no `[[Call]]`). That is
///   the one documented v1 divergence of this path;
///   `promise_call_on_a_plain_object_initializes_it_in_place` pins it as a test
///   rather than leaving it an accident. Making the three distinguishable needs
///   the compiler to lower `super(…)` to a real `Construct` with a
///   `newTarget` — a separate change, not a promise one.
pub fn promise_capability(ctx: &mut Ctx, this: JsValue) -> Result<JsValue, Throw> {
    let promise = match in_place_instance(ctx.heap, this) {
        Some(instance) => init_promise_in_place(ctx.heap, instance),
        None => {
            let prototype = this
                .as_object()
                .and_then(|ctor| ctx.heap.get(ctor).prototype);
            create_promise(ctx.heap, prototype, STATE_PENDING, JsValue::undefined())
        }
    };
    let pending: Rc<RefCell<Vec<Job>>> = ctx.pending.clone().unwrap_or_default();
    // Capability: the resolve/reject function objects handed to the executor.
    // They are ordinary function objects whose targets are host closures
    // capturing the shared pending-jobs sink — host closures cannot touch the
    // interpreter, so reaction settling they trigger joins the same sink
    // `Promise#then` on a settled promise uses.
    let (resolve_v, reject_v) = make_capability(ctx.heap, &pending, promise);
    let carrier = ctx.heap.alloc(JsObject::array(vec![
        JsValue::object(promise),
        resolve_v,
        reject_v,
    ]));
    // Pinned per the module contract: the interpreter holds the three values
    // in Rust locals across the executor call, and the collector can run
    // inside it.
    ctx.heap.add_pinned_root(carrier);
    Ok(JsValue::object(carrier))
}

/// `NewPromiseCapability`'s `GetCapabilitiesExecutor` builtin (spec
/// 27.5.1.5.1 steps 1-7), for the seam where the record is filled in *by* the
/// constructor rather than handed to it ready-made.
///
/// `state` is the `PromiseCapability Record` the interpreter allocated in step
/// 2: a three-slot object whose `[[Resolve]]`/`[[Reject]]` start `undefined`
/// and whose third slot is `[[AlreadyResolved]]`. The returned function is
/// `CreateBuiltinFunction(executorClosure, 2, "", «»)`:
/// `length: 2`, `name: ""`, `%Function.prototype%`, and no own `prototype`
/// (which is what keeps it non-constructible).
///
/// Called, it throws a `TypeError` if the record is already resolved and
/// otherwise records the two arguments and returns `undefined` — so the pair
/// `NewPromiseCapability` ends up with is exactly the pair the constructor's own
/// code received. `Construct` itself is the interpreter's; a host closure runs
/// synchronously inside it and needs no job.
///
/// The guard reads `[[AlreadyResolved]]` rather than "are both halves still
/// `undefined`", which is the older text of the same step. The two differ for
/// `executor()` — a first call with no arguments leaves both halves
/// `undefined`, and under the older rule a second call would be accepted,
/// recording over the first. `[[AlreadyResolved]]` is the current rule, and
/// `built-ins/Promise/{all,race}/capability-executor-called-twice.js` pins it.
pub(crate) fn capability_executor(ctx: &mut Ctx, state: JsValue) -> Result<JsValue, Throw> {
    let Some(record) = state.as_object() else {
        return Err(Throw::Message(
            "TypeError: the capability record is not an object".into(),
        ));
    };
    // The collector cannot see the handle this closure captures, and the
    // interpreter drops its own reference the moment `Construct` returns, so
    // the record is pinned for the rest of the run.
    ctx.heap.add_pinned_root(record);
    let executor = alloc_host(ctx.heap, move |heap, _this, args| {
        let used = heap.get(record).properties[SLOT_ALREADY_RESOLVED].as_smi();
        if used == Some(CAPABILITY_USED) {
            return Err(v12_native::error_object(
                heap,
                "TypeError",
                "capability resolve/reject functions already set",
            ));
        }
        let obj = heap.get_mut(record);
        obj.properties[SLOT_ALREADY_RESOLVED] = smi(CAPABILITY_USED);
        obj.properties[CAP_SLOT_RESOLVE] = args.first().copied().unwrap_or_else(JsValue::undefined);
        obj.properties[CAP_SLOT_REJECT] = args.get(1).copied().unwrap_or_else(JsValue::undefined);
        Ok(JsValue::undefined())
    });
    let func = executor.as_object().expect("alloc_host returns an object");
    shape_builtin_function(ctx, func, 2);
    Ok(executor)
}

/// The recorded `(resolve, reject)` pair of a `PromiseCapability Record` (spec
/// 27.5.1.5 step 7). Both halves are `undefined` until the executor has run;
/// the caller applies the two `IsCallable` checks, so a constructor that
/// ignored its executor surfaces as a `TypeError` from the interpreter.
pub(crate) fn capability_pair(ctx: &mut Ctx, state: JsValue) -> Result<(JsValue, JsValue), Throw> {
    let Some(record) = state.as_object() else {
        return Err(Throw::Message(
            "TypeError: the capability record is not an object".into(),
        ));
    };
    let obj = ctx.heap.get(record);
    Ok((
        obj.properties[CAP_SLOT_RESOLVE],
        obj.properties[CAP_SLOT_REJECT],
    ))
}

/// [`capability_settle`] across the registry seam: the interpreter holds the
/// capability as values (the promise plus the two resolving functions) while
/// the heap-level half works in handles.
///
/// 27.5.1.1 step 6 `IfAbruptRejectPromise` — an executor that threw rejects its
/// own promise with the thrown value, and the constructor still returns it.
pub(crate) fn capability_settle_values(
    ctx: &mut Ctx,
    promise: JsValue,
    resolve_v: JsValue,
    reject_v: JsValue,
    value: JsValue,
    rejecting: bool,
) -> Result<JsValue, Throw> {
    // Only the promise needs a handle: the pair is passed straight through to
    // `capability_settle`, which needs no handles of its own for it.
    let Some(promise) = promise.as_object() else {
        // A capability is always a promise object (`promise_capability` builds
        // it), so a non-object here means the interpreter handed back
        // something this seam never produced. Nothing to settle.
        return Ok(JsValue::undefined());
    };
    let sink = ctx.pending.clone().unwrap_or_default();
    let _ = capability_settle(
        ctx.heap, &sink, promise, resolve_v, reject_v, value, rejecting,
    );
    Ok(JsValue::undefined())
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
        heap.add_pinned_root(func);
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
/// Thin wrapper over [`shape_builtin_function`] with the resolving functions'
/// arity — the pair is the only `CreateBuiltinFunction` in this module with
/// `length: 1`, so the arity is spelled at the call site rather than threaded
/// through every caller.
pub(crate) fn shape_resolving_function(ctx: &mut Ctx, func: v12_heap::Handle<JsObject>) {
    shape_builtin_function(ctx, func, 1);
}

/// Stamps the spec shape of a promise `CreateBuiltinFunction` result onto
/// `func`: `length: <arity>`, then `name: ""`, and
/// `[[Prototype]] = %Function.prototype%`.
///
/// The order of the two installs is observable — test262
/// `built-ins/Promise/prototype/then/reject-function-property-order.js` pins
/// `length` before `name` — so `length` goes first. No own `prototype` property
/// is created: that absence is what makes the function non-constructible
/// (27.5.1.3 step 1-a), and the engine already leaves fresh host functions
/// without one.
///
/// Shape only: no settlement, ordering, or combinator behaviour changes here.
fn shape_builtin_function(ctx: &mut Ctx, func: v12_heap::Handle<JsObject>, arity: i32) {
    if let Some(proto) = ctx.function_proto() {
        ctx.heap.get_mut(func).prototype = Some(proto);
    }
    ctx.define_data_prop_with_attrs(
        func,
        "length",
        JsValue::from_i32_smi(arity).expect("builtin arity fits Smi"),
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
    heap.add_pinned_root(func);
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
    heap.add_pinned_root(record);
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
/// iterator steps and invokes `promiseResolve` and `then` — each needs the
/// machine — while the results array, the unsettled count and the reaction
/// records live here. One element at a time, which is what lets an abrupt
/// element settle the combinator instead of stalling on an unbounded iterator.
///
/// The aggregate promise is *not* one of the engine's: it is
/// `promiseCapability.[[Promise]]`, the object `NewPromiseCapability(C)` had
/// `C` construct, so a subclassed receiver gets its own promise back. Every
/// settlement of it therefore goes through a `Call` of the capability's own
/// `[[Resolve]]`/`[[Reject]]` — user code whenever `C` supplied its own — and
/// that call is the interpreter's. What is left here is bookkeeping and the
/// answer to "does this element settle the aggregate, and with what".
pub mod combinator {
    use v12_heap::{Handle, Heap, JsObject, JsValue, Kind};
    use v12_native::NativeId;
    use v12_native::Throw;
    use v12_native::promise_combinator::{
        KIND_ALL, KIND_ALL_SETTLED, KIND_ANY, KIND_RACE, SLOT_AGGREGATE, SLOT_ERRORS, SLOT_KIND,
        SLOT_REJECT, SLOT_REMAINING, SLOT_RESULTS, STATE_SLOTS,
    };

    use super::{shape_resolving_function, smi};
    use crate::builtins::ctx::Ctx;

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

    /// `combinator_begin`: the results array, the unsettled count and the
    /// capability's `[[Reject]]`, in one state object the interpreter drives
    /// element by element.
    ///
    /// The aggregate promise is not allocated here. 27.5.4.1 step 6 is
    /// `NewPromiseCapability(C)` — the spec's own constructor path, run by
    /// `Interp::new_promise_capability` before it gets here — so `promise` is
    /// the object `C` constructed and what script receives at step 13. That is
    /// also why the two gates 27.5.1.5 enforces are the caller's: step 1
    /// (`IsConstructor`) and steps 8-9 (`IsCallable` on both halves of the
    /// record) run inside `new_promise_capability`, ahead of `GetIterator`, so
    /// a non-constructor receiver never reaches this function.
    pub fn begin(
        ctx: &mut Ctx,
        kind: i32,
        promise: JsValue,
        reject_v: JsValue,
    ) -> Result<JsValue, Throw> {
        if kind != KIND_ALL && kind != KIND_RACE && kind != KIND_ALL_SETTLED && kind != KIND_ANY {
            return Err(Throw::Message(
                "TypeError: unknown Promise combinator".into(),
            ));
        }
        let heap = &mut *ctx.heap;
        let Some(aggregate) = promise.as_object() else {
            return Err(Throw::Message(
                "TypeError: Promise combinator capability has no promise".into(),
            ));
        };
        // `KIND_RACE` never reads the results array, but keeping the layout
        // uniform means no slot is position-dependent.
        let results = heap.alloc(JsObject::array(Vec::new()));
        heap.add_pinned_root(results);
        // `KIND_ANY`'s `errors` list (27.5.4.3.1 step 2). Allocated for every
        // kind so the layout is uniform and `SLOT_ERRORS` is never a hole;
        // only `KIND_ANY` reads it.
        let errors = heap.alloc(JsObject::array(Vec::new()));
        heap.add_pinned_root(errors);
        let mut state = JsObject::default();
        for _ in 0..STATE_SLOTS {
            state.properties.push(JsValue::undefined());
            state.property_keys.push(None);
        }
        state.properties[SLOT_KIND as usize] = smi(kind);
        state.properties[SLOT_AGGREGATE as usize] = JsValue::object(aggregate);
        state.properties[SLOT_REJECT as usize] = reject_v;
        state.properties[SLOT_RESULTS as usize] = JsValue::object(results);
        state.properties[SLOT_ERRORS as usize] = JsValue::object(errors);
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
        heap.add_pinned_root(handle);
        Ok(JsValue::object(handle))
    }

    /// One element of a combinator: counts it unsettled and builds the pair
    /// `then` is invoked with — or reports that there is nothing to invoke.
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
    /// The pair is the capability's own, never a fresh one — except for
    /// `KIND_ALL_SETTLED`, which the spec gives a pair of its own because
    /// neither half may reach the capability directly:
    ///
    /// - `KIND_RACE` builds none (27.5.4.5.1 step 8-e):
    ///   `[promiseCapability.[[Resolve]], promiseCapability.[[Reject]]]`
    ///   verbatim, so a race element is settled by exactly the functions the
    ///   receiver's constructor handed to its executor.
    /// - `KIND_ALL` builds only the fulfilled half (27.5.4.1.1 step 4):
    ///   `[onFulfilledElement, promiseCapability.[[Reject]]]`. The reject half
    ///   needed no per-element wrapper either, so an element rejection
    ///   rejects the aggregate directly.
    /// - `KIND_ALL_SETTLED` builds **both** halves (27.5.4.2.1 steps 4 and
    ///   5), each writing a `{ status, value }` / `{ status, reason }` object
    ///   at the element's own index, and sharing one `alreadyCalled` record.
    ///   `promiseCapability.[[Reject]]` is never in the pair: an element
    ///   rejection is a *result*, not the aggregate's.
    /// - `KIND_ANY` is `KIND_ALL` with the halves the other way round
    ///   (27.5.4.3.1 step 7-r):
    ///   `[promiseCapability.[[Resolve]], rejectElement]` — the fulfilled half
    ///   **is** the capability's own `[[Resolve]]`, so any fulfilment settles
    ///   the aggregate with no per-element closure at all, and the rejected
    ///   half is the one per-element handler.
    ///
    /// First-wins per element is now the capability's own business — the
    /// aggregate's resolving functions ignore a second settlement — so a `then`
    /// free to call both halves can no longer decrement `SLOT_REMAINING` twice
    /// and fulfill the aggregate with a hole in it: it is already rejected by
    /// then, and its `[[Resolve]]` is a no-op.
    pub fn step(
        ctx: &mut Ctx,
        state_v: JsValue,
        index: u32,
        value: JsValue,
        then_v: JsValue,
        resolve_v: JsValue,
    ) -> Result<JsValue, Throw> {
        let (state, _aggregate, kind) = parts(ctx.heap, state_v)?;
        let callable = then_v
            .as_object()
            .is_some_and(|f| matches!(ctx.heap.get(f).kind, Kind::Function | Kind::Proxy));
        // Every kind but `KIND_RACE` counts its elements unsettled, so a first
        // element cannot settle the aggregate before the iterator has been
        // exhausted — the one extra count `begin` seeds is exactly that token,
        // and `KIND_ANY` spends it on its `errors` list instead of a values
        // array.
        if kind != KIND_RACE {
            let remaining = slot(ctx.heap, state, SLOT_REMAINING)
                .as_smi()
                .unwrap_or(0)
                .saturating_add(1);
            ctx.heap.get_mut(state).properties[SLOT_REMAINING as usize] = smi(remaining);
        }
        if kind == KIND_ALL_SETTLED {
            // 27.5.4.2.1 has no `IsCallable(nextPromise.then)` branch: it goes
            // straight from `Call(promiseResolve, …)` to `Invoke(nextPromise,
            // "then", « onFulfilled, onRejected »)`, so a non-callable `then`
            // is the `Invoke`'s `TypeError` and rejects the aggregate. Building
            // the pair unconditionally is what keeps that the case — the
            // `settle_now` carrier below would settle such an element in place
            // instead, which is `KIND_ALL`'s rule and not this one's.
            return Ok(all_settled_pair(ctx, state, index, resolve_v));
        }
        if !callable {
            // `IsCallable(then) is false` (27.5.4.1 step 11-c / 27.5.4.5.1
            // step 8-e): the spec hands the element's own handlers to
            // `promiseResolve` rather than invoking anything, and the
            // bookkeeping is the interpreter's — through `element_settled`,
            // which is where the results write and the count live now.
            return Ok(settle_now(ctx, value));
        }
        let reject_v = slot(ctx.heap, state, SLOT_REJECT);
        let on_fulfilled = match kind {
            KIND_ALL => all_on_fulfilled(ctx, state, index, resolve_v),
            // `KIND_RACE` and `KIND_ANY` both hand the capability's own
            // `[[Resolve]]` to `then` verbatim (27.5.4.5.1 step 8-e /
            // 27.5.4.3.1 step 7-r), so neither needs a fulfilled closure.
            _ => resolve_v,
        };
        let on_rejected = match kind {
            KIND_ANY => any_on_rejected(ctx, state, index, resolve_v),
            _ => reject_v,
        };
        let pair = ctx
            .heap
            .alloc(JsObject::array(vec![on_fulfilled, on_rejected]));
        ctx.heap.add_pinned_root(pair);
        Ok(JsValue::object(pair))
    }

    /// The one-element carrier: "there is no `then` to invoke — settle this
    /// element yourself, through `element_settled`, with this value".
    fn settle_now(ctx: &mut Ctx, value: JsValue) -> JsValue {
        let carrier = ctx.heap.alloc(JsObject::array(vec![value]));
        ctx.heap.add_pinned_root(carrier);
        JsValue::object(carrier)
    }

    /// `PerformPromiseAll`'s `onFulfilledElement` (spec 27.5.4.1.1 step 4), the
    /// one per-element handler the capability rewiring leaves in place.
    ///
    /// It is not a host closure, because its last step is
    /// `Perform ? Call(resultCapability.[[Resolve]], undefined, « valuesArray
    /// »)` and for a subclassed receiver that is user code — which a `&mut
    /// Heap` closure cannot reach. So the function object is an
    /// interpreter-intercepted native ([`NativeId::PromiseCombinatorElement`])
    /// carrying its element record in `elements`
    /// ([`v12_native::promise_combinator::element`]), and the `Call` is the
    /// interpreter's.
    ///
    /// Spec shape: a `CreateBuiltinFunction` of arity 1, so it carries
    /// `length: 1`, `name: ""` and `%Function.prototype%` like any other
    /// resolving function.
    fn all_on_fulfilled(
        ctx: &mut Ctx,
        state: Handle<JsObject>,
        index: u32,
        resolve_v: JsValue,
    ) -> JsValue {
        // The spec's `alreadyResolved[index]`, as a one-slot object: the flag
        // has to outlive the call that set it, and a `Vec` element cannot.
        let already = ctx.heap.alloc(JsObject::ordinary(
            smallvec::smallvec![JsValue::from_bool(false)],
            smallvec::smallvec![None],
        ));
        let func = element_handler(ctx, state, index, resolve_v, already, true);
        JsValue::object(func)
    }

    /// `PerformPromiseAllSettled`'s pair (spec 27.5.4.2.1 steps 4 and 5):
    /// `[onFulfilled, onRejected]`, **both** per-element, sharing one
    /// `alreadyCalled` record.
    ///
    /// The shared record is the load-bearing part (step 4-c / 5-c set
    /// `onRejected.[[AlreadyCalled]]` to the record step 4-b made for
    /// `onFulfilled`): a `then` free to call both halves records the first
    /// outcome only, so the unsettled count cannot reach zero twice and the
    /// aggregate cannot be fulfilled with a half-written entry. Each function's
    /// own `[[Index]]` is the same `index`; `element::FULFILLED` is what tells
    /// the two halves apart, since the records are otherwise identical.
    ///
    /// The handlers are interpreter-intercepted natives
    /// ([`NativeId::PromiseCombinatorElement`]) rather than host closures
    /// because each one's last step is
    /// `Return ? Call(resultCapability.[[Resolve]], undefined, « valuesArray »)`
    /// and for a subclassed receiver that is user code. The status object each
    /// builds is a plain `OrdinaryObjectCreate` plus two
    /// `CreateDataPropertyOrThrow`s, which the interpreter's property path
    /// owns (it is the `Call` of `[[Resolve]]` that needs the machine, not the
    /// object).
    fn all_settled_pair(
        ctx: &mut Ctx,
        state: Handle<JsObject>,
        index: u32,
        resolve_v: JsValue,
    ) -> JsValue {
        // Step 4-b: `Let alreadyCalled be the Record { [[Value]]: false }`.
        let already = ctx.heap.alloc(JsObject::ordinary(
            smallvec::smallvec![JsValue::from_bool(false)],
            smallvec::smallvec![None],
        ));
        let on_fulfilled = element_handler(ctx, state, index, resolve_v, already, true);
        let on_rejected = element_handler(ctx, state, index, resolve_v, already, false);
        let pair = ctx.heap.alloc(JsObject::array(vec![
            JsValue::object(on_fulfilled),
            JsValue::object(on_rejected),
        ]));
        ctx.heap.add_pinned_root(pair);
        JsValue::object(pair)
    }

    /// `PerformPromiseAny`'s `rejectElement` (spec 27.5.4.3.2) — the *only*
    /// per-element handler `Promise.any` installs, and the reason the pair is
    /// not the capability's own pair verbatim.
    ///
    /// The pair is `[promiseCapability.[[Resolve]], rejectElement]`
    /// (27.5.4.3.1 step 7-r): the fulfilled half is the capability's own
    /// resolving function, so a fulfilment needs no bookkeeping at all and this
    /// handler is never on the fulfilled path. Only the rejected half has work
    /// to do — write the reason at the element's own index, take the element
    /// off the unsettled count — so only the rejected half gets a function.
    ///
    /// It is an interpreter-intercepted native
    /// ([`NativeId::PromiseCombinatorElement`]) rather than a host closure for
    /// the same reason `all_on_fulfilled` is: its last step builds an
    /// `AggregateError` and `Call`s `promiseCapability.[[Reject]]`, and for a
    /// subclassed receiver that is user code.
    fn any_on_rejected(
        ctx: &mut Ctx,
        state: Handle<JsObject>,
        index: u32,
        resolve_v: JsValue,
    ) -> JsValue {
        // Step 4-b: `Let alreadyCalled be the Record { [[Value]]: false }` — the
        // same per-element first-wins flag `allSettled`'s shared record is.
        // `onFulfilledElement` has none of its own here, but the spec still
        // guards the rejected half, so a `then` free to call `rejectElement`
        // twice records the first reason only.
        let already = ctx.heap.alloc(JsObject::ordinary(
            smallvec::smallvec![JsValue::from_bool(false)],
            smallvec::smallvec![None],
        ));
        let func = element_handler(ctx, state, index, resolve_v, already, false);
        JsValue::object(func)
    }

    /// One `CreateBuiltinFunction` of a per-element handler: the intercepted
    /// native carrying its [`v12_native::promise_combinator::element`] record,
    /// shaped like any other resolving function (`length: 1`, `name: ""`,
    /// `%Function.prototype%`).
    ///
    /// Rooted like every other combinator object: the only reference the
    /// reaction record holds is the handler, and the element record — the state
    /// object and the `alreadyCalled` flag included — rides this one. The flag
    /// is not pinned separately because `elements` is traced from a pinned
    /// function.
    fn element_handler(
        ctx: &mut Ctx,
        state: Handle<JsObject>,
        index: u32,
        resolve_v: JsValue,
        already: Handle<JsObject>,
        fulfilled: bool,
    ) -> Handle<JsObject> {
        let func = ctx.heap.alloc(JsObject::function(
            v12_heap::FunctionTarget::Bytecode(NativeId::PromiseCombinatorElement.into()),
            None,
        ));
        ctx.heap.add_pinned_root(func);
        ctx.heap.get_mut(func).elements = vec![
            JsValue::object(state),
            smi(index as i32),
            resolve_v,
            JsValue::object(already),
            JsValue::from_bool(fulfilled),
        ];
        shape_resolving_function(ctx, func);
        func
    }

    /// `combinator_element_settled`: the element bookkeeping — the outcome at
    /// `index`, the unsettled count, and (for the last outstanding element) the
    /// value the aggregate settles with.
    ///
    /// The three kinds differ in *what* they write and *what* they hand back,
    /// and `KIND_ANY` splits the difference between them:
    ///
    /// - `KIND_RACE` writes nothing and counts nothing: it settles on its first
    ///   element, so the value itself is the whole answer and
    ///   `SLOT_REMAINING` is left at the exhaustion token. Its only way in here
    ///   is the non-callable-`then` path, where the value is what the aggregate
    ///   settles with.
    /// - `KIND_ALL` and `KIND_ALL_SETTLED` write the value (or the element's
    ///   status object) at `index` in the *results* array and take one off the
    ///   count, handing the **results array** back when the count reaches zero
    ///   (27.5.4.1.1 step 4-b-iii-2).
    /// - `KIND_ANY` counts its elements like `KIND_ALL` but has the opposite
    ///   polarity, so it takes both branches: a *fulfilment* writes nothing and
    ///   counts nothing — the capability's own `[[Resolve]]` ran it directly,
    ///   and the only way here with `fulfilled == true` is the
    ///   non-callable-`then` path, which is a fulfilment of the aggregate with
    ///   the element's own value. A *rejection* writes the reason at `index` in
    ///   the **errors** array (27.5.4.3.2 step 4-e — stored as-is, never
    ///   adopted, whatever the reason is) and hands the **errors array** back
    ///   when the count reaches zero, which is the interpreter's cue to build
    ///   the `AggregateError`.
    ///
    /// `fulfilled` is the pair half that fired. `KIND_ALL` only ever installs
    /// the fulfilled one, so for it the flag is always `true`; `KIND_ALL_SETTLED`
    /// installs both and the value is already the element's status object by
    /// the time it arrives (the interpreter's handler built it, steps 4-b-iii-iv
    /// / 5-b-iii-iv); `KIND_ANY` installs only the rejected one.
    pub fn element_settled(
        ctx: &mut Ctx,
        state_v: JsValue,
        index: u32,
        fulfilled: bool,
        value: JsValue,
    ) -> Result<Option<JsValue>, Throw> {
        let (state, _aggregate, kind) = parts(ctx.heap, state_v)?;
        if kind == KIND_RACE {
            return Ok(Some(value));
        }
        if kind == KIND_ANY {
            if fulfilled {
                return Ok(Some(value));
            }
            if let Some(errors) = slot(ctx.heap, state, SLOT_ERRORS).as_object() {
                set_result(ctx.heap, errors, index, value);
            }
            return Ok(drop_unsettled(ctx.heap, state, SLOT_ERRORS));
        }
        if let Some(results) = slot(ctx.heap, state, SLOT_RESULTS).as_object() {
            set_result(ctx.heap, results, index, value);
        }
        Ok(drop_unsettled(ctx.heap, state, SLOT_RESULTS))
    }

    /// `combinator_settle`: exhaustion spends the one count `PerformPromiseAll`
    /// keeps back for "the iterator has not run out yet", and hands back the
    /// value the aggregate settles with when nothing is still outstanding.
    /// Everything else is the interpreter's `Call` of the capability's own
    /// functions — the abrupt path rejects through
    /// `promiseCapability.[[Reject]]`, which may be user code.
    ///
    /// The three kinds again:
    ///
    /// - `KIND_RACE` on exhaustion is a no-op on the aggregate (27.2.4.2 step
    ///   8-f returns the capability promise unchanged).
    /// - `KIND_ALL` / `KIND_ALL_SETTLED` hand the **values array** back, which
    ///   the interpreter resolves the aggregate with.
    /// - `KIND_ANY` hands the **errors array** back, which the interpreter wraps
    ///   in an `AggregateError` and rejects the aggregate with. This is the
    ///   empty-iterable case of 27.5.4.3.1 step 6 (the array is empty, because
    ///   the loop pulled nothing) *and* the ordinary all-rejected case, since a
    ///   combinator whose elements all rejected before the iterator ran out
    ///   arrives here with the count already down to the token.
    pub fn settle(
        ctx: &mut Ctx,
        state_v: JsValue,
        exhausted: bool,
    ) -> Result<Option<JsValue>, Throw> {
        let (state, _aggregate, kind) = parts(ctx.heap, state_v)?;
        if exhausted && kind != KIND_RACE {
            let payoff = if kind == KIND_ANY {
                SLOT_ERRORS
            } else {
                SLOT_RESULTS
            };
            return Ok(drop_unsettled(ctx.heap, state, payoff));
        }
        // `KIND_RACE` on exhaustion: the spec returns the capability promise
        // as it stands (27.2.4.2 step 8-f) — a still-pending race stays
        // pending until an element settles, and an already-settled one keeps
        // the winner `then` gave it. Dropping the unsettled count here would
        // instead fulfill the aggregate with the (never written) results
        // array, which is how `Promise.race([settledPromise])` used to resolve
        // to `[]` before its first element's reaction job ran and lose every
        // ordering race that followed.
        Ok(None)
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

    /// Drops one element off `SLOT_REMAINING` and, when the count reaches zero,
    /// hands back the array named by `payoff` (`SLOT_RESULTS` for the kinds that
    /// fulfil with a values array, `SLOT_ERRORS` for `KIND_ANY`). It is the one
    /// thing left for the interpreter to do: `Call(capability.[[Resolve]],
    /// undefined, « payoff »)` for a values array, or
    /// `Call(capability.[[Reject]], undefined, « AggregateError(errors) »)` for
    /// the errors one. A later element cannot settle twice — the aggregate's
    /// resolving functions are first-wins.
    fn drop_unsettled(heap: &mut Heap, state: Handle<JsObject>, payoff: i32) -> Option<JsValue> {
        let remaining = slot(heap, state, SLOT_REMAINING)
            .as_smi()
            .unwrap_or(1)
            .saturating_sub(1);
        heap.get_mut(state).properties[SLOT_REMAINING as usize] = smi(remaining);
        if remaining > 0 {
            return None;
        }
        Some(slot(heap, state, payoff))
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
    ctx.heap.add_pinned_root(results);
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
    pin(ctx.heap, on_finally);
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
        // `this` is settled, so nothing else holds its payload once the
        // watcher record is not installed: pin it for the job below.
        pin(ctx.heap, payload);
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
