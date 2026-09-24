use std::path::{Path, PathBuf};

use v12_engine::Engine;

fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("v12-task6a-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create module fixture directory");
    dir
}

fn write_module(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, source).expect("write module fixture");
    path
}

fn evaluate_entry(engine: &mut Engine, dir: &Path, name: &str, source: &str) {
    let path = write_module(dir, name, source);
    if let Err(error) = engine.eval_module_source_at(source, dir, Some(&path)) {
        let stage = engine
            .eval("String(globalThis.stage)")
            .map(|value| engine.to_display_string(value))
            .unwrap_or_else(|_| "<unavailable>".into());
        panic!(
            "entry module evaluates at {stage}: {}",
            engine.to_display_string(error)
        );
    }
}

fn assert_true(engine: &mut Engine, source: &str) {
    let value = engine.eval(source).expect("assertion evaluates");
    assert_eq!(value.as_bool(), Some(true), "assertion failed: {source}");
}

#[test]
fn namespace_reads_follow_later_local_writes() {
    let dir = fixture("live-update");
    write_module(
        &dir,
        "dep.js",
        "export let value = 1; export { value as alias }; export function update() { value = 2; return value; }",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "main.js",
        "import * as ns from './dep.js'; globalThis.ns = ns; ns.update();",
    );

    assert_true(&mut engine, "globalThis.ns.value === 2 && globalThis.ns.alias === 2");
}

#[test]
fn self_and_cycle_imports_preserve_namespace_identity() {
    let dir = fixture("identity");
    write_module(
        &dir,
        "b.js",
        "import * as an from './a.js'; import * as bnSelf from './b.js'; export let b = 2; globalThis.bCycle = an; globalThis.bSelf = bnSelf;",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "a.js",
        "import * as an from './a.js'; import * as an2 from './a.js'; import * as bn from './b.js'; export let a = 1; globalThis.selfSame = an === an2; globalThis.aSelf = an; globalThis.aCycle = bn;",
    );

    assert_true(&mut engine, "globalThis.selfSame === true");
    assert_true(&mut engine, "globalThis.bCycle === globalThis.aSelf");
    assert_true(&mut engine, "globalThis.aCycle === globalThis.bSelf");
    assert_true(&mut engine, "globalThis.aSelf.a === 1 && globalThis.aCycle.b === 2");
}

#[test]
fn explicit_reexport_aliases_share_the_source_cell() {
    let dir = fixture("reexport");
    write_module(
        &dir,
        "source.js",
        "export let value = 1; export function update() { value = 3; }",
    );
    write_module(
        &dir,
        "bridge.js",
        "export { value as alias, update as setValue } from './source.js';",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "main.js",
        "import * as source from './source.js'; import * as bridge from './bridge.js'; globalThis.source = source; globalThis.bridge = bridge; bridge.setValue();",
    );

    assert_true(
        &mut engine,
        "globalThis.source.value === 3 && globalThis.bridge.alias === 3",
    );
}

#[test]
fn imported_cell_reports_tdz_before_local_initialization() {
    let dir = fixture("tdz");
    write_module(
        &dir,
        "reader.js",
        "import { value } from './a.js'; export function read() { return value; }",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "a.js",
        "import { read } from './reader.js'; import * as readerNs from './reader.js'; try { read(); } catch (e) { globalThis.tdzName = e.name; globalThis.readType = typeof read; globalThis.nsReadType = typeof readerNs.read; } export let value = 1; value = 2; globalThis.initializedValue = read(); globalThis.reader = read;",
    );

    assert_true(&mut engine, "globalThis.tdzName === 'ReferenceError'");
    assert_true(&mut engine, "globalThis.readType === 'function'");
    assert_true(&mut engine, "globalThis.nsReadType === 'function'");
    assert_true(&mut engine, "globalThis.initializedValue === 2");
    assert_true(&mut engine, "globalThis.reader() === 2");
}

#[test]
fn namespace_descriptor_dereferences_cells_and_keeps_spec_attributes() {
    let dir = fixture("descriptor");
    write_module(
        &dir,
        "dep.js",
        "export let value = 1; export function update() { value = 4; }",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "main.js",
        "import * as ns from './dep.js'; globalThis.ns = ns; ns.update();",
    );

    assert_true(
        &mut engine,
        "var d = Object.getOwnPropertyDescriptor(globalThis.ns, 'value'); d.value === 4 && d.writable === true && d.enumerable === true && d.configurable === false && typeof d.value === 'number'",
    );
    assert_true(
        &mut engine,
        "'use strict'; try { globalThis.ns.value = 9; false; } catch (e) { e instanceof TypeError && globalThis.ns.value === 4 }",
    );
    assert_true(&mut engine, "delete globalThis.ns.value === false");
}

#[test]
fn direct_eval_reads_and_writes_module_cells() {
    let dir = fixture("direct-eval");
    write_module(
        &dir,
        "dep.js",
        "export let value = 1; export function update() { return eval('value = value + 1'); }",
    );
    let mut engine = Engine::new();
    evaluate_entry(
        &mut engine,
        &dir,
        "main.js",
        "import * as ns from './dep.js'; globalThis.ns = ns; ns.update();",
    );

    assert_true(&mut engine, "globalThis.ns.value === 2");
}

#[test]
fn live_binding_cells_survive_gc_stress() {
    let dir = fixture("gc");
    write_module(
        &dir,
        "dep.js",
        "export const value = { marker: 'kept' }; export function update() { return value.marker; }",
    );
    let mut engine = Engine::new();
    engine.heap_mut().gc_stress(Some(1));
    evaluate_entry(
        &mut engine,
        &dir,
        "main.js",
        "import * as ns from './dep.js'; globalThis.ns = ns; for (let i = 0; i < 20; i = i + 1) { ({ marker: i }); }",
    );
    engine.heap_mut().force_collect();

    assert_true(
        &mut engine,
        "globalThis.ns.value.marker === 'kept' && globalThis.ns.update() === 'kept'",
    );
}
