use v12_engine::Engine;

#[test]
fn engine_owns_async_promise() {
    let mut engine = Engine::new();
    let h = engine.new_async_promise();
    assert_eq!(engine.heap().get(h).properties[0].as_smi(), Some(0));
}

#[test]
fn top_level_await_in_module_body_resolves() {
    // ROADMAP G: a module main whose body contains `await` compiles as
    // async; `run()` defers it and the checkpoint drain runs it to
    // completion instead of throwing `await outside async`.
    let mut engine = Engine::new();
    engine
        .eval_module_source(
            "globalThis.__tla = await Promise.resolve(41);",
            std::path::Path::new("."),
        )
        .expect("tla module evaluates");
    let v = engine.eval("globalThis.__tla").expect("read");
    assert_eq!(engine.to_display_string(v), "41");
}
