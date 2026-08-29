use v12_engine::Engine;

#[test]
fn engine_owns_async_promise() {
    let mut engine = Engine::new();
    let h = engine.new_pending_promise();
    assert_eq!(engine.heap().get(h).properties[0].as_f64(), Some(0.0));
}
