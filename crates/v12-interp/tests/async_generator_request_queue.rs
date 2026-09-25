//! Task 4: async-generator request promises are queued FIFO and rooted.
//!
//! The request queue is internal engine state, so these tests drive it through
//! the public surface only: `next`/`return`/`throw` on an async generator
//! while it is parked on an `await`, then the deterministic settlement log
//! built from `take_pending_settlements()` (the same sink the engine's
//! microtask checkpoint drains).
//!
//! The bare `Interp` realm has no `Promise` global, so the awaited value comes
//! from an async function call, which returns its own promise.

use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue, Kind};
use v12_interp::{Interp, NativeId};

/// One entry of the settlement log. The value is rendered when the settlement
/// is applied, so a later collection cannot invalidate the record.
#[derive(Clone, Debug)]
struct Settlement {
    promise: Handle<JsObject>,
    rejected: bool,
    value: String,
    done: bool,
}

impl Settlement {
    /// The resolved iterator result: (display value, done).
    fn result(&self) -> (String, bool) {
        assert!(!self.rejected, "settlement is a rejection: {}", self.value);
        (self.value.clone(), self.done)
    }

    /// The rejected payload, as a display string.
    fn rejection(&self) -> String {
        assert!(self.rejected, "settlement is a resolution");
        self.value.clone()
    }
}

/// The most recently created live generator. Async function calls are
/// generators too, so the tests create the awaited promise *before* the async
/// generator and take the newest one.
fn target_generator(interp: &Interp<'_>) -> Handle<JsObject> {
    interp
        .heap()
        .roots()
        .iter()
        .copied()
        .filter_map(|value| value.as_object())
        .filter(|&h| {
            let generator = interp.heap().get(h);
            generator.kind == Kind::Generator
                && generator
                    .properties
                    .get(2)
                    .and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64()))
                    != Some(1.0)
        })
        .max_by_key(|h| h.index())
        .expect("test setup creates a generator")
}

fn run_setup(interp: &mut Interp<'_>) {
    match interp.run() {
        Ok(()) => {}
        Err(error) => panic!("setup: {}", interp.to_display_string(error.0)),
    }
}

fn request(
    interp: &mut Interp<'_>,
    method: NativeId,
    generator: Handle<JsObject>,
    value: JsValue,
) -> JsValue {
    let request = interp
        .call_function(method as u32, JsValue::object(generator), &[value])
        .expect("generator request call");
    request
        .as_object()
        .expect("an async-generator request returns a promise");
    request
}

/// Records what a settlement carries, rendering the value eagerly: the
/// payload may be unreachable again once the test collects. `apply` also
/// settles the promise, which is what makes a later `await` observe it.
fn record(
    interp: &mut Interp<'_>,
    settlement: (Handle<JsObject>, JsValue, bool),
    apply: bool,
) -> Settlement {
    let (promise, payload, rejected) = settlement;
    let (value, done) = if rejected {
        (interp.to_display_string(payload), true)
    } else if let Some(result) = payload
        .as_object()
        .filter(|&h| interp.heap().get(h).kind != Kind::Array)
    {
        let (value, done) = {
            let object = interp.heap().get(result);
            (object.properties[0], object.properties[1].is_true())
        };
        (interp.to_display_string(value), done)
    } else {
        (interp.to_display_string(payload), false)
    };
    if apply {
        let state = if rejected { 2 } else { 1 };
        let object = interp.heap_mut().get_mut(promise);
        object.properties[0] = JsValue::from_i32_smi(state).unwrap();
        object.properties[1] = payload;
    }
    Settlement {
        promise,
        rejected,
        value,
        done,
    }
}

/// Runs microtasks until quiescent, applying and logging every settlement.
fn drive(interp: &mut Interp<'_>) -> Vec<Settlement> {
    let mut log = Vec::new();
    for _ in 0..8 {
        interp.run_jobs();
        let batch = interp.take_pending_settlements();
        if batch.is_empty() {
            break;
        }
        for settlement in batch {
            let entry = record(interp, settlement, true);
            log.push(entry);
        }
    }
    log
}

/// Runs microtasks until quiescent, logging settlements *without* applying
/// them: the promise the body awaits stays pending, so the generator keeps its
/// park while the requests queued behind it still settle.
fn drive_without_applying(interp: &mut Interp<'_>) -> Vec<Settlement> {
    let mut log = Vec::new();
    for _ in 0..4 {
        interp.run_jobs();
        let batch = interp.take_pending_settlements();
        if batch.is_empty() {
            break;
        }
        for settlement in batch {
            let entry = record(interp, settlement, false);
            log.push(entry);
        }
    }
    log
}

/// The settlements of one request promise, in log order.
fn of_request<'a>(log: &'a [Settlement], request: JsValue) -> Vec<&'a Settlement> {
    let request = request.as_object().expect("request is a promise");
    log.iter().filter(|s| s.promise == request).collect()
}

/// The single settlement of `request`, asserting exactly-once.
fn settle<'a>(log: &'a [Settlement], request: JsValue) -> &'a Settlement {
    let matches = of_request(log, request);
    assert_eq!(
        matches.len(),
        1,
        "request settles exactly once, log had {:?}",
        matches.iter().map(|s| &s.value).collect::<Vec<_>>()
    );
    matches[0]
}

/// Index of `request`'s settlement in the log (its FIFO position).
fn position(log: &[Settlement], request: JsValue) -> usize {
    let request = request.as_object().expect("request is a promise");
    log.iter()
        .position(|s| s.promise == request)
        .unwrap_or_else(|| panic!("request {} never settled", request.index()))
}

/// Keeps `value` alive across an explicit `force_collect`. The bare `Interp`
/// realm roots no global object, and `force_collect` marks from the current
/// root set without the interpreter's `gc_protect` republish, so the test
/// supplies the roots the engine's own checkpoint would.
fn keep_alive(interp: &mut Interp<'_>, value: JsValue) {
    interp.heap_mut().add_root(value);
}

/// A body whose first statement parks on an `await`: the request queue fills
/// up while the generator has not reached a `yield` yet.
const PARKING_SOURCE: &str = "async function tick() { return 'w'; } \
                              async function* ag() { \
                                var awaited = await tick(); \
                                yield 'a'; yield 'b'; yield 'c'; \
                              } \
                              var it = ag();";

#[test]
fn two_next_requests_parked_on_await_settle_in_arrival_order() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, PARKING_SOURCE).expect("compile");
    run_setup(&mut interp);
    let generator = target_generator(&interp);

    let first = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    // The body parked on its internal `await`: the request is queued, not lost.
    assert!(
        interp.take_pending_settlements().is_empty(),
        "an await suspension does not settle the request"
    );
    let second = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    assert_ne!(
        first.as_object(),
        second.as_object(),
        "each request gets its own promise"
    );

    let log = drive(&mut interp);
    assert_eq!(settle(&log, first).result(), ("a".into(), false));
    assert_eq!(settle(&log, second).result(), ("b".into(), false));
    assert!(position(&log, first) < position(&log, second), "FIFO order");
}

#[test]
fn three_next_requests_parked_on_await_settle_in_arrival_order() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, PARKING_SOURCE).expect("compile");
    run_setup(&mut interp);
    let generator = target_generator(&interp);

    let first = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let second = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let third = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let log = drive(&mut interp);
    assert_eq!(settle(&log, first).result(), ("a".into(), false));
    assert_eq!(settle(&log, second).result(), ("b".into(), false));
    assert_eq!(settle(&log, third).result(), ("c".into(), false));
    let positions = [
        position(&log, first),
        position(&log, second),
        position(&log, third),
    ];
    assert!(
        positions[0] < positions[1] && positions[1] < positions[2],
        "FIFO settlement order, got {positions:?}"
    );
}

#[test]
fn settled_requests_leave_the_queue_so_later_requests_still_settle() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, PARKING_SOURCE).expect("compile");
    run_setup(&mut interp);
    let generator = target_generator(&interp);

    let first = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let queued = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let log = drive(&mut interp);
    settle(&log, first);
    settle(&log, queued);
    // Drain what is left of the queue, one resume per request, until the
    // generator completes.
    let mut settled: Vec<(String, bool)> = Vec::new();
    for _ in 0..4 {
        if settled.iter().any(|(_, done)| *done) {
            break;
        }
        let next = request(
            &mut interp,
            NativeId::GeneratorNext,
            generator,
            JsValue::undefined(),
        );
        let log = drive(&mut interp);
        if of_request(&log, next).is_empty() {
            continue;
        }
        settled.push(settle(&log, next).result());
    }
    assert_eq!(
        settled,
        vec![("c".to_string(), false), ("undefined".to_string(), true)],
        "the remaining queue resumes in order and the completion drains it"
    );
    // Nothing settles twice, ever.
    let log = drive(&mut interp);
    assert!(log.is_empty(), "quiescent after completion, got {log:?}");
}

/// The body awaits a promise the test never settles, so the generator stays
/// parked while further requests queue up behind the in-flight one.
const STUCK_SOURCE: &str = "async function tick() { return 'w'; } \
                            async function* ag() { await tick(); yield 'first'; } \
                            var it = ag();";

#[test]
fn queued_return_request_survives_a_parked_generator() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, STUCK_SOURCE).expect("compile");
    run_setup(&mut interp);
    let generator = target_generator(&interp);

    let in_flight = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let value = JsValue::string(interp.heap_mut().intern_text("returned"));
    let queued = request(&mut interp, NativeId::GeneratorReturn, generator, value);
    assert_ne!(in_flight.as_object(), queued.as_object());

    // The resume behind the queued `return` reaches the first `yield`, so the
    // in-flight request settles; the queued one keeps waiting.
    let log = drive_without_applying(&mut interp);
    assert_eq!(settle(&log, in_flight).result(), ("first".into(), false));
    assert!(
        of_request(&log, queued).is_empty(),
        "the queued return request stays pending while the generator is stuck"
    );

    // Resuming again completes the body: the queued `return` request settles
    // with the returned value, and a request queued behind it drains too.
    let behind = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let log = drive_without_applying(&mut interp);
    let queued_settlement = settle(&log, queued);
    let behind_settlement = settle(&log, behind);
    assert!(
        !queued_settlement.rejected,
        "queued return request resolves"
    );
    assert!(queued_settlement.done, "the return completed the body");
    assert!(behind_settlement.done, "the trailing request is drained");
    assert!(
        position(&log, queued) < position(&log, behind),
        "FIFO order"
    );
}

#[test]
fn queued_throw_request_survives_a_parked_generator() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, STUCK_SOURCE).expect("compile");
    run_setup(&mut interp);
    let generator = target_generator(&interp);

    let in_flight = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let reason = JsValue::string(interp.heap_mut().intern_text("injected"));
    let queued = request(&mut interp, NativeId::GeneratorThrow, generator, reason);
    assert_ne!(in_flight.as_object(), queued.as_object());

    // The queued throw is delivered at the suspension point (the engine's
    // resume path, unchanged here), so the in-flight request takes the
    // rejection and the body completes; the request queued behind it observes
    // the completed generator.
    let log = drive_without_applying(&mut interp);
    assert_eq!(settle(&log, in_flight).rejection(), "injected");
    let queued_settlement = settle(&log, queued);
    assert!(!queued_settlement.rejected, "queued throw request resolves");
    assert!(queued_settlement.done, "the body is completed");
    assert!(
        position(&log, in_flight) < position(&log, queued),
        "FIFO order"
    );

    // A completed generator keeps answering new requests exactly once.
    let after = request(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    let log = drive_without_applying(&mut interp);
    assert!(settle(&log, after).done, "done");
    assert!(
        drive_without_applying(&mut interp).is_empty(),
        "no late settlement"
    );
}

#[test]
fn request_queue_settles_exactly_once_under_gc() {
    // The same scenario with a collection between every request. Requests are
    // settled exactly once and in arrival order even when the heap reclaims
    // slots in between.
    let mut heap = Heap::new(GcPolicy::Growth { floor_bytes: 0 });
    let mut interp = Interp::from_source(&mut heap, PARKING_SOURCE).expect("compile");
    run_setup(&mut interp);
    let generator = target_generator(&interp);
    let mut log = Vec::new();
    let mut requests = Vec::new();
    for _ in 0..3 {
        let next = request(
            &mut interp,
            NativeId::GeneratorNext,
            generator,
            JsValue::undefined(),
        );
        // The engine's checkpoint keeps a request alive until the drain
        // settles it; do the same before the explicit collection.
        requests.push(next);
        // Re-root every request seen so far: `gc_protect` clears the root set
        // at each safepoint, and a freed slot would be handed to the next
        // request, making two distinct requests share a handle.
        for &seen in requests.iter() {
            keep_alive(&mut interp, seen);
        }
        keep_alive(&mut interp, JsValue::object(generator));
        interp.heap_mut().force_collect();
        log.extend(drive(&mut interp));
    }
    assert!(heap.collections() > 0, "the GC policy actually collected");

    let (first, second, third) = (requests[0], requests[1], requests[2]);
    assert_eq!(settle(&log, first).result(), ("a".into(), false));
    assert_eq!(settle(&log, second).result(), ("b".into(), false));
    assert_eq!(settle(&log, third).result(), ("c".into(), false));
    let positions = [
        position(&log, first),
        position(&log, second),
        position(&log, third),
    ];
    assert!(
        positions[0] < positions[1] && positions[1] < positions[2],
        "FIFO settlement order under GC, got {positions:?}"
    );
}
