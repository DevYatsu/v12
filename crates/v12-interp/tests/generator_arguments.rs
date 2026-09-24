use v12_heap::{GcPolicy, Heap, JsValue};
use v12_interp::Interp;

fn completion(source: &str) -> JsValue {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, source).expect("source compiles");
    if let Err(error) = interp.run() {
        panic!("source runs: {}", interp.to_display_string(error.0));
    }
    interp.completion_value().expect("source has completion")
}

fn number(value: JsValue) -> Option<f64> {
    value.as_f64().or_else(|| value.as_smi().map(f64::from))
}

#[test]
fn sync_generator_retains_full_actuals_across_suspend_and_resume() {
    assert_eq!(
        number(completion(
            "function* g() { yield arguments.length; yield arguments[2]; } var it = g(1, 2, 3); it.next(); it.next().value;"
        )),
        Some(3.0)
    );
}

#[test]
fn generator_direct_eval_source_compiles_with_restored_owner_metadata() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let _ = Interp::from_source(
        &mut heap,
        "function* g() { var x = eval('arguments[1]'); yield x; } var it = g(7, 8); it.next().value;",
    )
    .expect("direct-eval generator source compiles");
}

#[test]
fn arguments_object_has_configurable_length_and_callee() {
    assert!(
        completion(
            "function f() { return arguments; } var a = f(1, 2); a.length === 2 && a.callee === f;"
        )
        .is_true()
    );
}

#[test]
fn strict_arguments_callee_throws_but_sloppy_callee_is_readable() {
    assert!(completion(
        "function f() { 'use strict'; return arguments.callee; } try { f(); } catch (e) { e.name === 'TypeError'; }"
    )
    .is_true());
    assert_eq!(
        completion("function f() { return arguments.callee === f; } f();").is_true(),
        true
    );
}
