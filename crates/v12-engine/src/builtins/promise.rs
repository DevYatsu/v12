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

use crate::job_queue::{Job, JobCtx};
use v12_interp::JSException;

/// `[[State]]`: pending.
const STATE_PENDING: i32 = 0;
/// `[[State]]`: fulfilled.
const STATE_FULFILLED: i32 = 1;
/// `[[State]]`: rejected.
const STATE_REJECTED: i32 = 2;

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
        properties: vec![smi(state), payload, JsValue::object(reactions)],
        prototype,
        ..JsObject::default()
    });
    heap.add_root(JsValue::object(promise));
    promise
}

/// `Promise.resolve(x)`: identity for promises; otherwise a fulfilled promise
/// carrying `x` (`undefined` when the argument is missing).
pub fn promise_resolve(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    if is_promise(heap, value) {
        return Ok(value);
    }
    // Called as a method, `this` is the Promise constructor whose `prototype`
    // link hosts `Promise.prototype`; the interpreter's `then` fast path
    // recognizes instances by that identity. Unbound calls (e.g. a destructured
    // `const r = Promise.resolve`) degrade gracefully: the promise works but
    // its `then` is unreachable from script.
    let prototype = this.as_object().and_then(|ctor| heap.get(ctor).prototype);
    Ok(JsValue::object(create_promise(
        heap,
        prototype,
        STATE_FULFILLED,
        value,
    )))
}

/// `Promise.reject(x)`: a rejected promise carrying `x`.
pub fn promise_reject(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    let prototype = this.as_object().and_then(|ctor| heap.get(ctor).prototype);
    Ok(JsValue::object(create_promise(
        heap,
        prototype,
        STATE_REJECTED,
        value,
    )))
}

/// `Promise.prototype.then(on_fulfilled, on_rejected)`.
///
/// On a pending promise: appends a reaction record. On a settled promise:
/// enqueues the reaction job immediately (the job runs at the next
/// checkpoint and settles the derived promise).
pub fn promise_then(
    heap: &mut Heap,
    this: JsValue,
    args: &[JsValue],
    sink: &Rc<RefCell<Vec<Job>>>,
) -> Result<JsValue, Throw> {
    if !is_promise(heap, this) {
        return Err(Throw::type_error(heap, "Promise.prototype.then requires a promise"));
    }
    let promise = this.as_object().expect("checked above");
    let handler = args.first().copied().unwrap_or_else(JsValue::undefined);
    let on_rejected = args.get(1).copied().unwrap_or_else(JsValue::undefined);
    let prototype = heap.get(promise).prototype;
    let derived = create_promise(heap, prototype, STATE_PENDING, JsValue::undefined());

    let (state, payload) = {
        let p = heap.get(promise);
        (
            p.properties[0].as_smi().unwrap_or(STATE_PENDING),
            p.properties[1],
        )
    };
    match state {
        STATE_PENDING => {
            let reactions = heap.get(promise).properties[2]
                .as_object()
                .expect("promise carries a reactions array");
            let record = heap.alloc(JsObject::ordinary(
                vec![handler, on_rejected, JsValue::object(derived)],
                vec![None; 3],
            ));
            heap.add_root(JsValue::object(record));
            heap.get_mut(reactions).elements.push(JsValue::object(record));
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
pub fn queue_microtask(
    heap: &mut Heap,
    args: &[JsValue],
    sink: &Rc<RefCell<Vec<Job>>>,
) -> Result<JsValue, Throw> {
    let cb = args.first().copied().unwrap_or_else(JsValue::undefined);
    if cb.as_object().is_none() {
        return Err(Throw::type_error(heap, "queueMicrotask requires a function"));
    }
    // Root the callback: the job outlives the program stack that referenced it.
    heap.add_root(cb);
    let cb_obj = cb.as_object().expect("checked above");
    sink.borrow_mut().push(Box::new(move |ctx| {
        let _ = ctx.call_object(cb_obj, JsValue::undefined(), &[]);
    }));
    Ok(JsValue::undefined())
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
    let job: Job = Box::new(move |ctx: &mut JobCtx<'_, '_>| {
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
    });
    push(job);
}

/// Settles `promise` with `state`/`value` and schedules one job per queued
/// reaction record (microtask checkpoint semantics: the jobs join the
/// current drain via `ctx.enqueue`).
fn settle(ctx: &mut JobCtx<'_, '_>, promise: v12_heap::Handle<JsObject>, state: i32, value: JsValue) {
    let reactions = {
        let heap = ctx.heap_mut();
        heap.get_mut(promise).properties[0] = smi(state);
        heap.get_mut(promise).properties[1] = value;
        heap.get(promise).properties[2].as_object()
    };
    let Some(reactions) = reactions else {
        return;
    };
    let records = std::mem::take(&mut ctx.heap_mut().get_mut(reactions).elements);
    for record_v in records {
        let Some(record) = record_v.as_object() else {
            continue;
        };
        let (fulfill, reject, derived_v) = {
            let r = ctx.heap_mut().get(record);
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
        enqueue_reaction(
            &mut |job| {
                ctx.enqueue(job);
            },
            handler,
            value,
            derived,
            rejecting,
        );
    }
}
