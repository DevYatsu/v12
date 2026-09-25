//! Runtime IteratorClose behaviour for array binding patterns (spec 7.4.17).
//!
//! The standalone `v12-interp` has no `Symbol` intrinsic, so custom iterables
//! can only run here; the bytecode-level shape is asserted in
//! `v12-interp`'s `binding_iterables` tests.

use v12_engine::Engine;

fn run(src: &str) -> String {
    let mut engine = Engine::new();
    engine.eval(src).expect("eval");
    let v = engine.eval("globalThis.__out").expect("read");
    engine.to_display_string(v)
}

fn display_of(src: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(src).expect_err("source must throw");
    engine.to_display_string(value)
}

#[test]
fn early_completion_does_not_consume_an_extra_value() {
    // Spec 7.4.17 step 6 reads the record's [[Done]] flag. A probe `next()`
    // here would advance the iterator a second time.
    let out = run("globalThis.__calls = 0; \
         globalThis.__source = { [Symbol.iterator]() { return { next() { globalThis.__calls++; \
         return { value: globalThis.__calls, done: false }; } }; } }; \
         let [a] = globalThis.__source; globalThis.__out = [a, globalThis.__calls].join(',');");
    assert_eq!(out, "1,1");
}

#[test]
fn early_completion_closes_once_with_normal_completion() {
    let out = run("globalThis.__closed = 0; \
         globalThis.__source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, \
         return() { globalThis.__closed++; return {}; } }; } }; \
         let [a] = globalThis.__source; globalThis.__out = globalThis.__closed;");
    assert_eq!(out, "1");
}

#[test]
fn exhausted_pattern_does_not_close() {
    // The *last taken* step already reported `done`: the record's [[Done]] is
    // true, so spec 7.4.17 step 6 skips the close entirely.
    let out = run("globalThis.__closed = 0; let i = 0; \
         globalThis.__source = { [Symbol.iterator]() { return { next() { return { value: ++i, done: i >= 2 }; }, \
         return() { globalThis.__closed++; return {}; } }; } }; \
         let [a, b] = globalThis.__source; \
         globalThis.__out = [a, String(b), globalThis.__closed].join(',');");
    assert_eq!(out, "1,undefined,0");
}

#[test]
fn live_iterator_at_the_last_element_still_closes() {
    // Contrast with the exhausted case: when the last step reports
    // `done: false` the iterator is still live and gets the normal-completion
    // close (and `return()`'s Object result is validated).
    let out = run("globalThis.__closed = 0; let i = 0; \
         globalThis.__source = { [Symbol.iterator]() { return { next() { return { value: ++i, done: false }; }, \
         return() { globalThis.__closed++; return {}; } }; } }; \
         let [a, b] = globalThis.__source; globalThis.__out = [a, b, globalThis.__closed].join(',');");
    assert_eq!(out, "1,2,1");
}

#[test]
fn rest_pattern_drains_without_closing() {
    let out = run("globalThis.__closed = 0; let i = 0; \
         globalThis.__source = { [Symbol.iterator]() { return { next() { return { value: ++i, done: i > 3 }; }, \
         return() { globalThis.__closed++; return {}; } }; } }; \
         let [a, ...rest] = globalThis.__source; \
         globalThis.__out = [a, rest.join(''), globalThis.__closed].join(',');");
    assert_eq!(out, "1,23,0");
}

#[test]
fn reentrant_return_closes_each_iterator_once() {
    // The outer `return()` re-enters the same pattern, which builds a *new*
    // iterator and closes it once. The outer iterator must not be closed again
    // when the nested close error path unwinds through its handler.
    let out = run("globalThis.__closes = 0; globalThis.__reentered = false; \
         globalThis.__source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, \
         return() { globalThis.__closes++; if (!globalThis.__reentered) { globalThis.__reentered = true; \
         let [nested] = globalThis.__source; } return {}; } }; } }; \
         let [a] = globalThis.__source; globalThis.__out = globalThis.__closes;");
    assert_eq!(out, "2");
}

#[test]
fn throwing_return_does_not_replace_the_original_error() {
    // Spec 7.4.6: on a throw completion the close is best-effort and the
    // original abrupt wins.
    let thrown = display_of(
        "let source = { [Symbol.iterator]() { return { next() { throw 'next'; }, \
         return() { throw 'return'; } }; } }; let [a] = source;",
    );
    assert!(
        thrown.contains("next") && !thrown.contains("return"),
        "original error must win: {thrown}"
    );
}

#[test]
fn non_object_return_result_throws_on_the_normal_path() {
    // Normal completion validates the `return()` result as an Object.
    let thrown = display_of(
        "let source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, \
         return() { return 1; } }; } }; let [a] = source;",
    );
    assert!(thrown.contains("TypeError"), "expected TypeError: {thrown}");
}
