use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue, Kind};
use v12_interp::{Interp, NativeId};

struct IteratorResult {
    value: String,
    raw_value: JsValue,
    done: bool,
    rejected: bool,
}

fn last_generator(interp: &Interp<'_>) -> Handle<JsObject> {
    interp
        .heap()
        .roots()
        .iter()
        .copied()
        .filter_map(|value| value.as_object())
        .filter(|&h| {
            let generator = interp.heap().get(h);
            generator.kind == Kind::Generator
                && generator.properties.get(2).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())) != Some(1.0)
        })
        .last()
        .expect("test setup creates a generator")
}

fn settle_ready_promises(interp: &mut Interp<'_>) {
    interp.run_jobs();
    for settlement in interp.take_pending_settlements() {
        apply_settlement(interp, settlement);
    }
}

fn apply_settlement(
    interp: &mut Interp<'_>,
    (promise, payload, rejected): (Handle<JsObject>, JsValue, bool),
) {
    let state = if rejected { 2 } else { 1 };
    let object = interp.heap_mut().get_mut(promise);
    object.properties[0] = JsValue::from_i32_smi(state).unwrap();
    object.properties[1] = payload;
}

fn drive_request(interp: &mut Interp<'_>, request: JsValue) {
    let request = request.as_object().expect("request is a promise");
    let mut outer = None;
    for _ in 0..4 {
        interp.run_jobs();
        for settlement in interp.take_pending_settlements() {
            if settlement.0 == request {
                assert!(outer.is_none(), "request settles exactly once");
                outer = Some(settlement);
            } else {
                apply_settlement(interp, settlement);
            }
        }
        if outer.is_some() {
            break;
        }
    }
    let (promise, payload, rejected) = outer.expect("request did not settle");
    interp.push_pending_settlement(promise, payload, rejected);
}

fn run_setup(interp: &mut Interp<'_>) {
    match interp.run() {
        Ok(()) => {}
        Err(error) => panic!("setup: {}", interp.to_display_string(error.0)),
    }
}

fn call_generator_method(
    interp: &mut Interp<'_>,
    method: NativeId,
    generator: Handle<JsObject>,
    value: JsValue,
) -> JsValue {
    interp
        .call_function(method as u32, JsValue::object(generator), &[value])
        .expect("generator request call")
}

fn pending_promise(interp: &Interp<'_>) -> JsValue {
    interp
        .heap()
        .roots()
        .iter()
        .copied()
        .filter_map(|value| value.as_object())
        .find(|&h| {
            let promise = interp.heap().get(h);
            promise.kind == Kind::Promise
                && (promise.properties[0].as_smi() == Some(0)
                    || promise.properties[0].as_f64() == Some(0.0))
        })
        .map(JsValue::object)
        .expect("test setup creates a pending promise")
}

fn take_iterator_result(
    interp: &mut Interp<'_>,
    request: JsValue,
) -> IteratorResult {
    let mut settlements = interp.take_pending_settlements();
    assert_eq!(settlements.len(), 1, "one request settles exactly once");
    let (promise, payload, rejected) = settlements.pop().unwrap();
    assert_eq!(Some(promise), request.as_object());
    if rejected {
        return IteratorResult {
            value: interp.to_display_string(payload),
            raw_value: payload,
            done: true,
            rejected: true,
        };
    }
    let result = payload.as_object().expect("resolved request has an iterator result");
    let (value, done) = {
        let object = interp.heap().get(result);
        assert_eq!(object.properties.len(), 2);
        (object.properties[0], object.properties[1].is_true())
    };
    IteratorResult {
        value: interp.to_display_string(value),
        raw_value: value,
        done,
        rejected: false,
    }
}

#[test]
fn async_yield_star_uses_async_iterator_and_awaits_next_result() {
    let src = "var iterable = { \
                 next() { return result(); } \
               }; \
               async function result() { return { value: 'delegated', done: false }; } \
               async function* ag() { yield* iterable; return 'outer'; } \
               var it = ag();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    run_setup(&mut interp);
    let generator = last_generator(&interp);
    let request = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, request);

    let result = take_iterator_result(&mut interp, request);
    assert_eq!(result.value, "delegated");
    assert!(!result.done);
    assert!(!result.rejected);
}

#[test]
fn async_yield_star_preserves_manual_async_iterator_value_promise() {
    let src = "var iterable = { \
                 next() { return resultPromise(); } \
               }; \
               async function resultPromise() { \
                 return { value: valuePromise(), done: false }; \
               } \
               async function valuePromise() { return 'unwrapped value'; } \
               async function* ag() { return yield* iterable; } \
               var it = ag();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    run_setup(&mut interp);
    let generator = last_generator(&interp);
    let request = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, request);

    let result = take_iterator_result(&mut interp, request);
    assert_eq!(result.value, "[object Object]");
    assert!(
        result
            .raw_value
            .as_object()
            .is_some_and(|h| interp.heap().get(h).kind == Kind::Promise)
    );
    assert!(!result.done);
    assert!(!result.rejected);
}

#[test]
fn async_yield_star_awaits_resume_value_before_forwarding_next() {
    let src = "var seen = 'missing'; var step = 0; \
               async function resumePromise() { return 42; } \
               var resumePromiseValue = resumePromise(); \
               var iterable = { \
                  next(value) { \
                    step += 1; \
                    if (step === 1) return { value: 'first', done: false }; \
                    seen = value; \
                    return { value: seen, done: true }; \
                  } \
                }; \
                async function* ag() { return yield* iterable; } \
                var it = ag();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    run_setup(&mut interp);
    let promise_value = pending_promise(&interp);
    settle_ready_promises(&mut interp);
    let promise = promise_value.as_object().unwrap();
    assert_eq!(interp.heap().get(promise).properties[0].as_smi(), Some(1));
    assert_eq!(interp.to_display_string(interp.heap().get(promise).properties[1]), "42");
    let generator = last_generator(&interp);
    let first = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, first);
    let first_result = take_iterator_result(&mut interp, first);
    assert_eq!(first_result.value, "first");
    assert!(!first_result.done);

    let second = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::from_i32_smi(42).unwrap(),
    );
    drive_request(&mut interp, second);
    let second_result = take_iterator_result(&mut interp, second);
    assert_eq!(second_result.value, "42");
    assert!(second_result.done);
    assert!(!second_result.rejected);
}

#[test]
fn async_yield_star_preserves_delegated_completion_value() {
    let src = "var iterable = { \
                 next() { return { value: 'delegated-return', done: true }; } \
               }; \
               async function* ag() { return yield* iterable; } \
               var it = ag();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    run_setup(&mut interp);
    let generator = last_generator(&interp);
    let request = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, request);

    let result = take_iterator_result(&mut interp, request);
    assert_eq!(result.value, "delegated-return");
    assert!(result.done);
    assert!(!result.rejected);
}

#[test]
fn async_yield_star_delegates_return_after_awaiting_values() {
    let src = "async function returnValuePromise() { return 7; } \
               var returnValue = returnValuePromise(); \
               var iterable = { \
                 next() { return { value: 'first', done: false }; }, \
                 return(value) { return { value: 'inner:' + value, done: true }; } \
               }; \
               async function* ag() { yield* iterable; } \
               var it = ag();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    run_setup(&mut interp);
    let promise_value = pending_promise(&interp);
    settle_ready_promises(&mut interp);
    let promise = promise_value.as_object().unwrap();
    assert_eq!(interp.heap().get(promise).properties[0].as_smi(), Some(1));
    assert_eq!(interp.to_display_string(interp.heap().get(promise).properties[1]), "7");
    let generator = last_generator(&interp);
    let first = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, first);
    let first_result = take_iterator_result(&mut interp, first);
    assert_eq!(first_result.value, "first");
    assert!(!first_result.done);

    let request = call_generator_method(
        &mut interp,
        NativeId::GeneratorReturn,
        generator,
        promise_value,
    );
    drive_request(&mut interp, request);

    let result = take_iterator_result(&mut interp, request);
    assert_eq!(result.value, "inner:7");
    assert!(result.done);
    assert!(!result.rejected);
}

#[test]
fn async_yield_star_return_survives_finally_yield() {
    let src = "async function* ag() { \
                 try { yield 'first'; } \
                 finally { yield 2; } \
               } \
               var it = ag();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    run_setup(&mut interp);
    let generator = last_generator(&interp);

    let first = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, first);
    let first_result = take_iterator_result(&mut interp, first);
    assert_eq!(first_result.value, "first");
    assert!(!first_result.done);

    let sent_value = JsValue::string(interp.heap_mut().intern_text("sent-value"));
    let returned = call_generator_method(
        &mut interp,
        NativeId::GeneratorReturn,
        generator,
        sent_value,
    );
    drive_request(&mut interp, returned);
    let yielded = take_iterator_result(&mut interp, returned);
    assert_eq!(yielded.value, "2");
    assert!(!yielded.done);

    let completed = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, completed);
    let result = take_iterator_result(&mut interp, completed);
    assert_eq!(result.value, "sent-value");
    assert!(result.done);
    assert!(!result.rejected);
}

#[test]
fn async_yield_star_rejects_once_when_delegated_next_throws() {
    let src = "var iterable = { \
                 next() { throw 'boom'; } \
               }; \
               async function* ag() { yield* iterable; } \
               var it = ag();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    run_setup(&mut interp);
    let generator = last_generator(&interp);
    let request = call_generator_method(
        &mut interp,
        NativeId::GeneratorNext,
        generator,
        JsValue::undefined(),
    );
    drive_request(&mut interp, request);

    let result = take_iterator_result(&mut interp, request);
    assert_eq!(result.value, "boom");
    assert!(result.done);
    assert!(result.rejected);
    assert!(interp.take_pending_settlements().is_empty());
}
