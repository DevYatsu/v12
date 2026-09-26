//! Spec shape of the capability pair a `new Promise(executor)` executor
//! receives (ECMA-262 27.5.1.3 CreateResolvingFunctions).
//!
//! Both functions are `CreateBuiltinFunction`-like: `length: 1`, `name: ""`,
//! `[[Prototype]] = %Function.prototype%`, no own `prototype` property (so
//! they are not constructors), both extensible, and both `length`/`name`
//! descriptors `{ writable: false, enumerable: false, configurable: true }`.
//! The own-property *order* is observable too — test262
//! `built-ins/Promise/prototype/then/reject-function-property-order.js` pins
//! `length` before `name`.
//!
//! Constructibility itself (`Reflect.construct(resolve, [])` throwing) is
//! deliberately not asserted here: `Reflect.construct` routing is Task 0b's
//! dependency, so a pin on it would measure the wrong lane. The observable
//! half — no own `prototype`, so `resolve.prototype` is `undefined` — is
//! asserted below.

use v12_engine::Engine;

/// Captures the executor's `(resolve, reject)` pair on `globalThis.__cap`.
/// The executor runs as a microtask job, so the read happens after the drain.
const CAPTURE: &str = "globalThis.__cap = null; \
                      new Promise(function (resolve, reject) { \
                        globalThis.__cap = [resolve, reject]; \
                      });";

/// Evaluates `source`, drains the microtask queue, then evaluates `read` and
/// returns the display string of its result (or of the value it threw).
fn after_jobs(source: &str, read: &str) -> String {
    let mut engine = Engine::new();
    engine.eval(source).expect("eval");
    engine.run_jobs();
    match engine.eval(read) {
        Ok(value) => engine.to_display_string(value),
        Err(thrown) => engine.to_display_string(thrown),
    }
}

/// Evaluates `body` — a JS function body over a parameter named `fn` — for
/// both captured functions and joins the two answers with ` | `.
fn both(body: &str) -> String {
    let source = format!(
        "let pair = globalThis.__cap; \
         let f = function (fn) {{ {body} }}; \
         throw [f(pair[0]), f(pair[1])].join(' | ');"
    );
    after_jobs(CAPTURE, &source)
}

#[test]
fn executor_receives_two_functions() {
    assert_eq!(
        after_jobs(
            CAPTURE,
            "throw [globalThis.__cap.length, \
              typeof globalThis.__cap[0], typeof globalThis.__cap[1]].join(',');"
        ),
        "2,function,function"
    );
}

#[test]
fn capability_functions_have_length_one() {
    assert_eq!(
        both("return Object.getOwnPropertyDescriptor(fn, 'length').value === 1;"),
        "true | true"
    );
}

#[test]
fn capability_functions_have_an_empty_name() {
    assert_eq!(both("return fn.name === '';"), "true | true");
}

#[test]
fn capability_functions_inherit_function_prototype() {
    assert_eq!(
        both("return Object.getPrototypeOf(fn) === Function.prototype;"),
        "true | true"
    );
}

#[test]
fn capability_functions_have_no_own_prototype_property() {
    assert_eq!(
        both("return Object.getOwnPropertyNames(fn).indexOf('prototype');"),
        "-1 | -1"
    );
    assert_eq!(
        both("return String(fn.prototype);"),
        "undefined | undefined"
    );
}

#[test]
fn capability_functions_are_extensible() {
    assert_eq!(both("return Object.isExtensible(fn);"), "true | true");
}

#[test]
fn capability_function_length_descriptor_matches_the_spec() {
    assert_eq!(
        both(
            "let d = Object.getOwnPropertyDescriptor(fn, 'length'); \
              return [d.writable, d.enumerable, d.configurable].join('/');"
        ),
        "false/false/true | false/false/true"
    );
}

#[test]
fn capability_function_name_descriptor_matches_the_spec() {
    assert_eq!(
        both(
            "let d = Object.getOwnPropertyDescriptor(fn, 'name'); \
              return [d.writable, d.enumerable, d.configurable].join('/');"
        ),
        "false/false/true | false/false/true"
    );
}

#[test]
fn capability_function_own_property_order_is_length_then_name() {
    assert_eq!(
        both("return Object.getOwnPropertyNames(fn).slice(0, 2).join(',');"),
        "length,name | length,name"
    );
}
