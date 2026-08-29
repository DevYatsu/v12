use v12_engine::Engine;

#[test]
fn async_function_returns_promise() {
    let mut engine = Engine::new();
    let result = engine.eval("async function f(){ return 1; } throw f();").unwrap_err();
    assert!(result.as_object().is_some());
}

#[test]
fn promise_resolve_then_runs_callback_via_run_jobs() {
    // The recorded gate: Promise.resolve().then(cb) must run cb when the
    // host calls engine.run_jobs(). Today the callback is enqueued onto
    // the engine's job sink but the constructor's elements[0] is 0, so
    // Promise.resolve() routes into the bytecode path and throws.
    let mut engine = Engine::new();
    let src = "globalThis.__done = false; \
               Promise.resolve(1).then(function (v) { globalThis.__done = v; });";
    engine.eval(src).expect("eval");
    engine.run_jobs();
    let v = engine.eval("globalThis.__done").expect("read");
    assert_eq!(engine.to_display_string(v), "1");
}

#[test]
fn promise_chained_then_drains_fia_run_jobs() {
    let mut engine = Engine::new();
    let src = "globalThis.__log = []; \
               Promise.resolve(10).then(function (a) { return a + 1; }) \
                                  .then(function (b) { globalThis.__log.push(b); });";
    engine.eval(src).expect("eval");
    engine.run_jobs();
    let v = engine.eval("globalThis.__log.join(',')").expect("read");
    assert_eq!(engine.to_display_string(v), "11");
}
