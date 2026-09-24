use std::time::{Duration, Instant};

use v12_heap::{FunctionTarget, GcPolicy, Heap, HostClosure, JsObject, JsValue};
use v12_interp::Interp;

fn add_native(_: &mut Heap, _: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    Ok(JsValue::from_f64(
        args.first().and_then(|v| v.as_f64()).unwrap_or_default()
            + args.get(1).and_then(|v| v.as_f64()).unwrap_or_default(),
    ))
}

fn number(value: JsValue) -> Option<f64> {
    value.as_f64().or_else(|| value.as_smi().map(f64::from))
}

fn throw_native(_: &mut Heap, _: JsValue, _: &[JsValue]) -> Result<JsValue, JsValue> {
    Err(JsValue::from_f64(1234.0))
}

#[test]
fn call_value_runs_bytecode_completion_with_this_arg() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let callee = heap.alloc(JsObject::function(FunctionTarget::Bytecode(1), None));
    let mut interp = Interp::from_source(&mut heap, "function f() { return this; }").unwrap();
    let this = JsValue::from_f64(7.0);
    let result = interp
        .call_value(JsValue::object(callee), this, &[])
        .unwrap();
    assert_eq!(result, this);
}

#[test]
fn call_value_runs_native_and_host_targets() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let native = heap.alloc(JsObject::function(FunctionTarget::Native(add_native), None));
    let host_closure = HostClosure::new(|_, _, args| {
        Ok(JsValue::from_f64(
            args.first().and_then(|v| v.as_f64()).unwrap_or_default() * 2.0,
        ))
    });
    let host = heap.alloc(JsObject::function(FunctionTarget::Host(host_closure), None));
    let mut interp = Interp::from_source(&mut heap, "function f() { return 1; }").unwrap();

    assert_eq!(
        interp
            .call_value(
                JsValue::object(native),
                JsValue::undefined(),
                &[JsValue::from_f64(2.0), JsValue::from_f64(3.0)],
            )
            .unwrap(),
        JsValue::from_f64(5.0)
    );
    assert_eq!(
        interp
            .call_value(
                JsValue::object(host),
                JsValue::undefined(),
                &[JsValue::from_f64(4.0)],
            )
            .unwrap(),
        JsValue::from_f64(8.0)
    );
}

#[test]
fn call_value_preserves_bound_prefix_and_proxy_call() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let target = heap.alloc(JsObject::function(FunctionTarget::Bytecode(1), None));
    let mut state = JsObject::default();
    state.elements.push(JsValue::object(target));
    state.elements.push(JsValue::from_f64(9.0));
    state.elements.push(JsValue::from_f64(3.0));
    let state = heap.alloc(state);
    let bound = heap.alloc(JsObject::function(FunctionTarget::Bound(state), None));
    let handler = heap.alloc(JsObject::default());
    let proxy = heap.alloc(JsObject::proxy(target, handler));
    let mut interp = Interp::from_source(&mut heap, "function f(a, b) { return a + b; }").unwrap();

    assert_eq!(
        number(
            interp
                .call_value(
                    JsValue::object(bound),
                    JsValue::from_f64(100.0),
                    &[JsValue::from_f64(2.0)],
                )
                .unwrap(),
        ),
        Some(5.0)
    );
    assert_eq!(
        number(
            interp
                .call_value(
                    JsValue::object(proxy),
                    JsValue::undefined(),
                    &[JsValue::from_f64(2.0), JsValue::from_f64(4.0)],
                )
                .unwrap(),
        ),
        Some(6.0)
    );
}

#[test]
fn call_value_preserves_thrown_value_and_supports_nested_bytecode() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let thrower = heap.alloc(JsObject::function(
        FunctionTarget::Native(throw_native),
        None,
    ));
    let inner = heap.alloc(JsObject::function(FunctionTarget::Bytecode(1), None));
    let outer = heap.alloc(JsObject::function(FunctionTarget::Bytecode(2), None));
    let mut interp = Interp::from_source(
        &mut heap,
        "function inner() { return 2; } function outer(inner) { return inner() + 1; }",
    )
    .unwrap();

    let error = interp
        .call_value(JsValue::object(thrower), JsValue::undefined(), &[])
        .unwrap_err();
    assert_eq!(error.0, JsValue::from_f64(1234.0));
    assert_eq!(
        number(
            interp
                .call_value(
                    JsValue::object(outer),
                    JsValue::undefined(),
                    &[JsValue::object(inner)],
                )
                .unwrap(),
        ),
        Some(3.0)
    );
}

#[test]
fn call_value_preserves_object_result_identity() {
    let mut heap = Heap::new(GcPolicy::Growth { floor_bytes: 0 });
    let callee = heap.alloc(JsObject::function(FunctionTarget::Bytecode(1), None));
    let argument = heap.alloc(JsObject::default());
    heap.add_root(JsValue::object(callee));
    heap.add_root(JsValue::object(argument));
    heap.force_collect();
    let mut interp = Interp::from_source(&mut heap, "function f(value) { return value; }").unwrap();
    let result = interp
        .call_value(
            JsValue::object(callee),
            JsValue::undefined(),
            &[JsValue::object(argument)],
        )
        .unwrap();
    assert_eq!(result.as_object(), Some(argument));
}

#[test]
fn call_value_does_not_execute_generator_or_async_body_inline() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let generator = heap.alloc(JsObject::function(FunctionTarget::Bytecode(1), None));
    let async_fn = heap.alloc(JsObject::function(FunctionTarget::Bytecode(2), None));
    let mut interp = Interp::from_source(
        &mut heap,
        "function* g() { return 1; } async function a() { return 2; }",
    )
    .unwrap();

    let generator_result = interp
        .call_value(JsValue::object(generator), JsValue::undefined(), &[])
        .unwrap();
    let async_result = interp
        .call_value(JsValue::object(async_fn), JsValue::undefined(), &[])
        .unwrap();
    assert!(generator_result.as_object().is_some());
    assert!(async_result.as_object().is_some());
}

#[test]
fn call_value_recovers_after_deadline_error() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let spin = heap.alloc(JsObject::function(FunctionTarget::Bytecode(1), None));
    let native = heap.alloc(JsObject::function(FunctionTarget::Native(add_native), None));
    let mut interp = Interp::from_source(&mut heap, "function f() { while (true) {} }").unwrap();
    interp.set_deadline(Some(Instant::now() + Duration::from_millis(1)));
    assert!(
        interp
            .call_value(JsValue::object(spin), JsValue::undefined(), &[])
            .is_err()
    );
    interp.set_deadline(None);
    assert_eq!(
        interp
            .call_value(
                JsValue::object(native),
                JsValue::undefined(),
                &[JsValue::from_f64(1.0), JsValue::from_f64(2.0)],
            )
            .unwrap(),
        JsValue::from_f64(3.0)
    );
}
