use v12_engine::Engine;
#[test]
fn async_function_returns_promise() {
    let mut engine = Engine::new();
    let result = engine.eval("async function f(){ return 1; } throw f();").unwrap_err();
    assert!(result.as_object().is_some());
}
