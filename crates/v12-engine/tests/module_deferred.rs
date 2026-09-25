use std::path::{Path, PathBuf};

use v12_engine::Engine;

fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("v12-task6b-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create module fixture directory");
    dir
}

fn write_module(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, source).expect("write module fixture");
    path
}

fn evaluate_entry(engine: &mut Engine, dir: &Path, source: &str) {
    let path = write_module(dir, "main.js", source);
    if let Err(error) = engine.eval_module_source_at(source, dir, Some(&path)) {
        panic!(
            "entry module evaluates: {}",
            engine.to_display_string(error)
        );
    }
}

fn assert_true(engine: &mut Engine, source: &str) {
    let value = engine.eval(source).expect("assertion evaluates");
    assert_eq!(value.as_bool(), Some(true), "assertion failed: {source}");
}

#[test]
fn eager_and_deferred_namespaces_are_distinct_but_share_bindings() {
    let dir = fixture("identity");
    write_module(
        &dir,
        "dep.js",
        "export let value = 1; export function update() { value = 2; }",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import * as eager from './dep.js'; import defer * as deferred from './dep.js'; globalThis.eager = eager; globalThis.deferred = deferred; eager.update();",
    );

    assert_true(&mut engine, "globalThis.eager !== globalThis.deferred");
    assert_true(
        &mut engine,
        "globalThis.eager.value === 2 && globalThis.deferred.value === 2",
    );
}

#[test]
fn eager_and_deferred_requests_evaluate_execution_state_once() {
    let dir = fixture("single-execution");
    write_module(
        &dir,
        "dep.js",
        "globalThis.depRuns = (globalThis.depRuns || 0) + 1; export const value = 1;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import * as eager from './dep.js'; import defer * as deferred from './dep.js'; globalThis.eager = eager; globalThis.deferred = deferred;",
    );

    assert_true(&mut engine, "globalThis.depRuns === 1");
    assert_true(
        &mut engine,
        "globalThis.eager.value === globalThis.deferred.value",
    );
}

#[test]
fn namespace_imports_preserve_phase_separation_for_deduplication() {
    let dir = fixture("phase");
    write_module(
        &dir,
        "dep.js",
        "export let value = 1; export function update() { value = 3; }",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as deferred from './dep.js'; import * as eager from './dep.js'; globalThis.eager = eager; globalThis.deferred = deferred; eager.update();",
    );

    assert_true(&mut engine, "globalThis.eager !== globalThis.deferred");
    assert_true(&mut engine, "globalThis.deferred.value === 3");
}

#[test]
fn deferred_namespace_reuses_the_existing_binding_cells() {
    let dir = fixture("cells");
    write_module(
        &dir,
        "dep.js",
        "export let value = 1; export function update() { value += 1; }",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import * as eager from './dep.js'; import defer * as deferred from './dep.js'; globalThis.eager = eager; globalThis.deferred = deferred; eager.update(); globalThis.eagerAgain = deferred.value;",
    );

    assert_true(&mut engine, "globalThis.eagerAgain === 2");
    assert_true(
        &mut engine,
        "globalThis.eager.value === globalThis.deferred.value",
    );
}
