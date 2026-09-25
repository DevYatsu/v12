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

#[test]
fn deferred_read_triggers_exactly_once_and_waits_for_the_read() {
    let dir = fixture("trigger-once");
    write_module(
        &dir,
        "dep.js",
        "globalThis.depRuns = (globalThis.depRuns || 0) + 1; export const value = 7;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as ns from './dep.js'; globalThis.ns = ns; globalThis.before = globalThis.depRuns;",
    );

    assert_true(&mut engine, "globalThis.before === undefined");
    assert_true(&mut engine, "globalThis.ns.value === 7");
    assert_true(&mut engine, "globalThis.depRuns === 1");
    assert_true(&mut engine, "globalThis.ns.missing === undefined");
    assert_true(&mut engine, "globalThis.depRuns === 1");
}

#[test]
fn deferred_symbol_and_then_reads_do_not_trigger() {
    let dir = fixture("skip-keyed");
    write_module(
        &dir,
        "dep.js",
        "globalThis.depRuns = (globalThis.depRuns || 0) + 1; export const then = 3; export const value = 4;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as ns from './dep.js'; globalThis.ns = ns; globalThis.symbolRead = ns[Symbol('missing')]; try { ns.then; } catch (e) { globalThis.thenError = e; } globalThis.before = globalThis.depRuns;",
    );

    assert_true(&mut engine, "globalThis.symbolRead === undefined");
    assert_true(
        &mut engine,
        "globalThis.thenError instanceof ReferenceError",
    );
    assert_true(&mut engine, "globalThis.before === undefined");
    assert_true(&mut engine, "globalThis.ns.value === 4");
    assert_true(&mut engine, "globalThis.depRuns === 1");
}

#[test]
fn deferred_enumeration_and_own_queries_trigger_once() {
    let dir = fixture("own-queries");
    write_module(
        &dir,
        "dep.js",
        "globalThis.depRuns = (globalThis.depRuns || 0) + 1; export const value = 5;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as ns from './dep.js'; globalThis.ns = ns; globalThis.keys = Object.keys(ns); globalThis.names = Object.getOwnPropertyNames(ns); globalThis.reflectKeys = Reflect.ownKeys(ns); globalThis.has = Object.hasOwn(ns, 'missing'); globalThis.reflectHas = Reflect.has(ns, 'missing'); globalThis.desc = Object.getOwnPropertyDescriptor(ns, 'missing');",
    );

    assert_true(&mut engine, "globalThis.keys.indexOf('value') !== -1");
    assert_true(&mut engine, "globalThis.names.indexOf('value') !== -1");
    assert_true(
        &mut engine,
        "globalThis.reflectKeys.indexOf('value') !== -1",
    );
    assert_true(
        &mut engine,
        "globalThis.has === false && globalThis.reflectHas === false",
    );
    assert_true(&mut engine, "globalThis.desc === undefined");
    assert_true(&mut engine, "globalThis.depRuns === 1");
}

#[test]
fn deferred_prototype_super_read_triggers() {
    let dir = fixture("super-trigger");
    write_module(
        &dir,
        "dep.js",
        "globalThis.depRuns = (globalThis.depRuns || 0) + 1; export const value = 6;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as ns from './dep.js'; let obj = { read() { return super.value; } }; Object.setPrototypeOf(obj, ns); globalThis.result = obj.read();",
    );

    assert_true(
        &mut engine,
        "globalThis.result === 6 && globalThis.depRuns === 1",
    );
}

#[test]
fn deferred_star_reexport_read_triggers() {
    let dir = fixture("reexport-trigger");
    write_module(
        &dir,
        "dep.js",
        "globalThis.depRuns = (globalThis.depRuns || 0) + 1; export const value = 8;",
    );
    write_module(&dir, "bridge.js", "export * from './dep.js';");
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as bridge from './bridge.js'; globalThis.bridge = bridge;",
    );

    assert_true(&mut engine, "globalThis.depRuns === undefined");
    assert_true(
        &mut engine,
        "globalThis.bridge.value === 8 && globalThis.depRuns === 1",
    );
}

#[test]
fn deferred_nested_import_uses_the_module_referrer() {
    let dir = fixture("nested-referrer");
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).expect("create nested module directory");
    write_module(&dir, "shared.js", "export const value = 9;");
    write_module(
        &nested,
        "dep.js",
        "import { value } from '../shared.js'; export const nestedValue = value;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as dep from './nested/dep.js'; globalThis.dep = dep;",
    );

    assert_true(&mut engine, "globalThis.dep.nestedValue === 9");
}

#[test]
fn deferred_evaluation_error_is_cached_by_identity() {
    let dir = fixture("cached-error");
    write_module(
        &dir,
        "dep.js",
        "globalThis.depRuns = (globalThis.depRuns || 0) + 1; throw new Error('boom');",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as ns from './dep.js'; globalThis.ns = ns; try { ns.value; } catch (e) { globalThis.firstError = e; } try { ns.missing; } catch (e) { globalThis.secondError = e; }",
    );

    assert_true(
        &mut engine,
        "globalThis.firstError === globalThis.secondError",
    );
    assert_true(&mut engine, "globalThis.firstError instanceof Error");
    assert_true(&mut engine, "globalThis.depRuns === 1");
}

#[test]
fn deferred_self_trigger_fails_with_type_error() {
    let dir = fixture("self-trigger");
    write_module(
        &dir,
        "dep.js",
        "import defer * as self from './dep.js'; export const value = self.value;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "import defer * as ns from './dep.js'; globalThis.ns = ns; try { ns.value; } catch (e) { globalThis.error = e; }",
    );

    assert_true(&mut engine, "globalThis.error instanceof TypeError");
}
