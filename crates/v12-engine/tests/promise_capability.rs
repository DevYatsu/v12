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
/// The executor runs synchronously (spec 27.5.1.1 step 5), so `__cap` is
/// already set when the `new Promise` expression itself returns; the drain
/// below is only there for tests that settle afterwards.
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

// ---------------------------------------------------------------------------
// GC soundness of engine-owned promise objects
// ---------------------------------------------------------------------------
//
// `Interp::gc_protect` clears the whole root set and republishes only the
// interpreter's own live state, so a plain `Heap::add_root` from the engine is
// gone by the time the `heap.safepoint()` inside that same call collects. A
// reaction job's `derived` promise is captured by a Rust closure the collector
// cannot see, so without the pinned root set (`Heap::add_pinned_root`) it dies
// and the next `settle` writes through a stale handle.
//
// Each test collects on *every* safepoint (`gc_stress(Some(1))`), so the
// exposure window is the whole program rather than one lucky allocation
// budget.
//
// Two conventions, both forced by the host API:
//
// - Results travel on `globalThis`, never on a `var`. `Engine::eval` compiles a
//   fresh program each time, and a top-level `var` in one program is not
//   visible to the next. `globalThis` is the same object across calls, which is
//   what the `__cap` capture above already relies on.
// - Where a value has to be produced in one program and *used* in a later one,
//   the steps are passed as a slice. The boundary is forced by the host API,
//   not by promise semantics: `Engine::eval` compiles a fresh program each
//   time, and a top-level `let` in one program is not visible to the next.

/// Runs each program in `steps` under collection-on-every-safepoint, draining
/// the job queue between them, then evaluates `read` and returns its display
/// string (or the display string of whatever it threw).
fn under_gc_pressure(steps: &[&str], read: &str) -> String {
    let mut engine = Engine::new();
    engine.heap_mut().gc_stress(Some(1));
    for step in steps {
        engine.eval(step).expect("eval");
        engine.run_jobs();
    }
    match engine.eval(read) {
        Ok(value) => engine.to_display_string(value),
        Err(thrown) => engine.to_display_string(thrown),
    }
}

/// Convenience wrapper for the single-program case.
fn under_gc_pressure_once(source: &str, read: &str) -> String {
    under_gc_pressure(&[source], read)
}

/// A chained `then` whose results nothing holds: the first derived promise is
/// reachable only from the reaction record its own `then` extended, and that
/// record is in turn reachable only from the first derived promise. With no
/// pinned root the pair dies, `drain_reaction_jobs` reads a stale handle, and
/// `settle` writes through it.
///
/// The expected values pin the spec, not the panic: a handler with no `return`
/// yields `undefined`, so the third link sees `undefined` and `n` ends
/// `undefined`. (Asserting `"2"` here would be asserting a wrong engine.)
#[test]
fn chained_then_on_unheld_results_survives_collection() {
    let source = "globalThis.log = []; globalThis.n = 0; \
                  Promise.resolve(1).then(function(v){ \
                    globalThis.log.push('A1:' + v); globalThis.n = v; }); \
                  Promise.resolve(2).then(function(v){ \
                    globalThis.log.push('A2:' + v); globalThis.n = v; }).then(function(v){ \
                    globalThis.log.push('B:' + v); globalThis.n = v; });";
    assert_eq!(
        under_gc_pressure_once(source, "globalThis.log.join(',');"),
        "A1:1,A2:2,B:undefined"
    );
    assert_eq!(
        under_gc_pressure_once(source, "String(globalThis.n);"),
        "undefined"
    );
}

/// The async-generator request-queue version of the same bug: three nested
/// `next()` promises, none held. The dead slot is reused by a fresh
/// one-property object, so the failure surfaces as an out-of-bounds write on
/// `properties[1]` rather than a stale-handle assert.
#[test]
fn nested_async_generator_requests_survive_collection() {
    let source = "async function* g(){ yield 1; } globalThis.it = g(); \
                  globalThis.seen = 0; \
                  globalThis.it.next().then(function(r){ globalThis.seen = 1; \
                    globalThis.it.next().then(function(r2){ globalThis.seen = 2; \
                      globalThis.it.next(); }); });";
    assert_eq!(
        under_gc_pressure_once(source, "String(globalThis.seen);"),
        "2"
    );
}

/// A capability function is a host closure whose only remaining handle lives in
/// the Rust job queue — the collector cannot see `Box<dyn FnOnce(&mut
/// JobCtx)>`. Once the executor job has been drained, `make_capability`'s pin
/// is the only thing keeping the pair alive, so a capability reclaimed in the
/// meantime reads as `callee is not a function` when script later calls it.
///
/// The call has to be a *later* program here: each `engine.eval` compiles a
/// fresh program, so a `let` from one is not in scope in the next — which is
/// why the two steps below are separate programs sharing `globalThis`.
#[test]
fn captured_capability_survives_collection_and_stays_callable() {
    let captured = under_gc_pressure_once(
        "globalThis.__cap = null; \
         globalThis.p = new Promise(function (resolve, reject) { \
           globalThis.__cap = [resolve, reject]; \
         });",
        "typeof globalThis.__cap[0];",
    );
    assert_eq!(captured, "function");
    // A second collection separates capturing from calling: the capability,
    // the promise, and the reaction record installed below must all survive
    // the gap for `got` to read back 42.
    assert_eq!(
        under_gc_pressure(
            &[
                "globalThis.got = 'unset'; globalThis.__cap = null; \
                 globalThis.p = new Promise(function (resolve) { \
                   globalThis.__cap = resolve; \
                 });",
                "globalThis.p.then(function(x){ globalThis.got = x; }); \
                 globalThis.__cap(42);",
            ],
            "String(globalThis.got);",
        ),
        "42"
    );
}

// ---------------------------------------------------------------------------
// The executor runs synchronously (spec 27.5.1.1 step 5)
// ---------------------------------------------------------------------------
//
// `Call(executor, undefined, « [[Resolve]], [[Reject]] »)` happens before the
// constructor returns, so script that captures a resolving function can call it
// in the *same* turn — the idiom 66 of the failing `built-ins/Promise` tests
// use. Each test below is one program: a `let` would not survive into the
// follow-up `eval`, so the results travel on `globalThis`.

/// Evaluates `source` — which must do all its own asserting through
/// `globalThis` — drains the job queue, then reads `read`.
fn settled(source: &str, read: &str) -> String {
    let mut engine = Engine::new();
    engine.eval(source).expect("eval");
    engine.run_jobs();
    let value = engine.eval(read).expect("read");
    engine.to_display_string(value)
}

#[test]
fn a_captured_resolve_is_callable_in_the_same_turn() {
    // The resolving function is live the instant `new Promise` returns, so the
    // very next statement can call it. Before the synchronous executor this
    // read `undefined` and the call threw `callee is not a function`.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let resolve; \
             let p = new Promise(function (r) { resolve = r; }); \
             p.then(function (v) { globalThis.__out = 'fulfilled:' + v; }, \
                    function (e) { globalThis.__out = 'rejected:' + e; }); \
             resolve(42);",
            "globalThis.__out"
        ),
        "fulfilled:42"
    );
}

#[test]
fn a_captured_reject_is_callable_in_the_same_turn() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let reject; \
             let p = new Promise(function (r, j) { reject = j; }); \
             p.then(function (v) { globalThis.__out = 'fulfilled:' + v; }, \
                    function (e) { globalThis.__out = 'rejected:' + e; }); \
             reject('boom');",
            "globalThis.__out"
        ),
        "rejected:boom"
    );
}

#[test]
fn the_executor_runs_before_the_statement_after_the_constructor() {
    // 27.5.1.1 step 5 orders the executor *before* step 7's return, so the log
    // reads executor-then-after rather than the reverse.
    assert_eq!(
        settled(
            "globalThis.__log = []; \
             globalThis.p = new Promise(function () { \
               globalThis.__log.push('executor'); \
             }); \
             globalThis.__log.push('after');",
            "globalThis.__log.join(',')"
        ),
        "executor,after"
    );
}

#[test]
fn a_throwing_executor_rejects_the_promise_with_the_thrown_value() {
    // 27.5.1.1 step 6 `IfAbruptRejectPromise`: the throw becomes the rejection
    // reason and the constructor still returns the promise.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             globalThis.p = new Promise(function () { throw 'nope'; }); \
             globalThis.p.then(function (v) { globalThis.__out = 'fulfilled:' + v; }, \
                               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "rejected:nope"
    );
}

#[test]
fn the_executor_call_context_is_undefined() {
    // 27.5.1.1 step 5 passes `undefined` as `this`. Only a *strict* executor
    // observes it: sloppy `Call(f, undefined, …)` rebinds `this` to the global
    // object (7.3.14 OrdinaryCallBindThis), which is why test262 pins this in
    // `executor-call-context-strict.js` with `flags: [onlyStrict]`. Before the
    // synchronous executor the body never ran at all, so a non-strict reading
    // would have passed either way — a false pass.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             new Promise(function () { 'use strict'; globalThis.__out = String(this); });",
            "globalThis.__out"
        ),
        "undefined"
    );
}

#[test]
fn a_sloppy_executor_sees_the_global_object_as_its_this() {
    // The other half of the same rule, and the one that makes the strict pin
    // above non-vacuous: `this` is coerced, not left as `undefined`.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             new Promise(function () { globalThis.__out = (this === globalThis) ? 'global' : 'other'; });",
            "globalThis.__out"
        ),
        "global"
    );
}

#[test]
fn a_resolution_with_a_non_callable_then_fulfills_and_terminates() {
    // `resolve({then: null})` is not an adoption (27.2.1.3.1 step 3-b-i): the
    // promise fulfills with the thenable itself. Routing that through the
    // ordinary settle re-read the same non-callable `then` and queued the same
    // job again — the drain spun to its 20 000-job cap and the handler below
    // never ran, which is what pinned
    // `Promise/resolve-non-thenable-immed.js` to a full-deadline stall.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let thenable = { then: null }; \
             let p = new Promise(function (resolve) { resolve(thenable); }); \
             p.then(function (v) { globalThis.__out = (v === thenable) ? 'same' : 'other'; }, \
                    function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "same"
    );
}

#[test]
fn a_resolution_with_a_callable_then_is_adopted_synchronously() {
    // The adoption's `then` call is still a job (spec 27.2.1.3.1 step 3-b-ii
    // runs at the checkpoint), but the *read* that decides it happened during
    // the executor, so a getter on `then` is already accounted for by the time
    // the constructor returns.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let p = new Promise(function (resolve) { \
               resolve({ then: function (r) { r('via-then'); } }); \
             }); \
             p.then(function (v) { globalThis.__out = v; }, \
                    function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "via-then"
    );
}

#[test]
fn a_settling_executor_does_not_hide_later_reaction_work() {
    // The synchronous executor does not collapse the queue: the reaction jobs
    // its settlement schedules still run at the checkpoint, in FIFO order.
    assert_eq!(
        settled(
            "globalThis.__log = []; \
             new Promise(function (resolve) { resolve(1); }) \
               .then(function (v) { globalThis.__log.push('a' + v); }); \
             Promise.resolve(2).then(function (v) { globalThis.__log.push('b' + v); }); \
             globalThis.__log.push('sync');",
            "globalThis.__log.join(',')"
        ),
        "sync,a1,b2"
    );
}
