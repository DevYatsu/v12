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
//   the steps are passed as a slice. The boundary matters: a `new Promise`
//   executor is enqueued as a job rather than run synchronously, so nothing
//   the executor defines exists yet during the program that constructs it.

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
/// The call has to be a *later* program: the executor is a job, so within the
/// program that runs `new Promise` the capture slot is still `null`.
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

/// The `resolve`-captured-out-of-the-executor idiom, which 73 of the failing
/// `built-ins/Promise` tests use, is **not** unblocked by pinning, and this
/// test records why. `promise_construct` enqueues the executor as a job rather
/// than calling it synchronously (the divergence note at the top of the
/// `promise` module), so the spec order — executor first, then the next
/// statement — is inverted, and a `resolve` captured in the executor is still
/// unset when the following statement calls it. That call throws
/// `callee is not a function` with liveness never at stake; the fix belongs to
/// the synchronous-executor task, not the root-set task.
#[test]
fn executor_is_deferred_so_a_same_program_resolve_call_cannot_work() {
    let source = "globalThis.order = []; \
                  globalThis.p = new Promise(function (r) { \
                    globalThis.order.push('exec'); \
                  }); \
                  globalThis.order.push('after');";
    // Spec would be "exec,after"; the deferral makes it "after,exec".
    assert_eq!(
        under_gc_pressure_once(source, "globalThis.order.join(',');"),
        "after,exec"
    );
}
