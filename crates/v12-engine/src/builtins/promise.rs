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
//! Arbitrary thenable unwrapping is out of scope: handlers are called as
//! plain functions, and a handler's return value fulfills the derived promise
//! directly.
//!
//! Promises, reaction records, reaction arrays, and queued callbacks are
//! rooted at creation: jobs run after the program's operand stack is gone, so
//! everything a job reaches must survive collection. This trades reclamation
//! for soundness (the objects are small and few in harness workloads).

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
/// engine-created only, so no user object collides in practice.
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
    let promise = heap.alloc(JsObject {
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
    (resolve_v, reject_v)
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
