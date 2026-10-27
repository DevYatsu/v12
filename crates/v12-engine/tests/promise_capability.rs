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

// ---------------------------------------------------------------------------
// 27.5.1.5 NewPromiseCapability — the capability executor
// ---------------------------------------------------------------------------
//
// `NewPromiseCapability(C)` is the spec's own constructor path: it makes a
// `PromiseCapability Record`, hands `C` a `GetCapabilitiesExecutor` builtin,
// and reads the recorded pair back out *after* the `Construct` returned. The
// pair the record ends up holding is therefore exactly the pair the
// constructor's own code received — the difference from 27.5.1.1, where the
// resolving functions are created up front and the executor is only handed
// them.
//
// The two halves sit on either side of the registry seam: the interpreter owns
// the `IsConstructor` gate, the `Construct` call and the post-hoc callable
// checks; the engine owns the executor object and the record. The engine half
// is asserted here directly against the two `NativeRegistry` methods (a bare
// heap, no interpreter needed); the interpreter half is asserted in
// `v12-interp`'s `promise_capability_seam.rs`, which is the only level at
// which `Construct` is reachable without a combinator rewiring that belongs to
// a later task.

use v12_engine::builtins::registry::NativeRegistry as EngineNatives;
use v12_heap::{FunctionTarget, GcPolicy, Handle, Heap, JsObject, JsValue, Kind, PropKey};
use v12_native::NativeRegistry as _;

/// 27.5.1.5 step 2: a fresh `PromiseCapability Record` — a two-slot object
/// whose `[[Resolve]]`/`[[Reject]]` both start out `undefined`.
/// The `PromiseCapability Record` spec 27.5.1.5 step 2 asks for: `[[Resolve]]`,
/// `[[Reject]]` and `[[AlreadyResolved]]`, in that order (slot names in
/// `v12_native::promise_capability`).
fn capability_record(heap: &mut Heap) -> Handle<JsObject> {
    heap.alloc(JsObject::ordinary(
        smallvec::smallvec![
            JsValue::undefined(),
            JsValue::undefined(),
            JsValue::from_i32_smi(v12_native::promise_capability::CAPABILITY_FRESH).expect("Smi"),
        ],
        smallvec::smallvec![None; 3],
    ))
}

/// Reads one own data property out of the shape, as `(value, attrs)`.
fn own_prop(heap: &mut Heap, obj: Handle<JsObject>, name: &str) -> (JsValue, v12_heap::Attrs) {
    let key = PropKey::from_string(heap.intern_text(name));
    let shape = heap.shape_of(obj);
    let desc = heap
        .lookup_property(shape, key)
        .copied()
        .unwrap_or_else(|| panic!("`{name}` must be an own property"));
    let slot = desc.slot().expect("`{name}` must be a data property") as usize;
    (heap.get(obj).properties[slot], desc.attrs())
}

/// Invokes a host-closure function object directly — the capability executor
/// is one, and a bare heap has no interpreter to `Call` it through.
fn call_host(heap: &mut Heap, func: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let h = func.as_object().expect("callee is an object");
    match heap.get(h).callable {
        FunctionTarget::Host(closure) => closure.call(heap, JsValue::undefined(), args),
        _ => panic!("the capability executor must be a host function"),
    }
}

/// The error name of a thrown value, read off the `[name, message]` layout
/// `JsObject::error` uses.
fn error_name(heap: &mut Heap, thrown: JsValue) -> String {
    let h = thrown.as_object().expect("a TypeError is an object");
    assert_eq!(heap.get(h).kind, Kind::Error, "the throw must be an error");
    let name = heap.get(h).properties[0];
    let h = name.as_string().expect("the error name is a string");
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
        _ => String::new(),
    }
}

/// A pair of distinct function objects to hand the executor as
/// `[[Resolve]]`/`[[Reject]]`.
fn fake_pair(heap: &mut Heap) -> (JsValue, JsValue) {
    let mk = |heap: &mut Heap| {
        let f = heap.alloc(JsObject::function(FunctionTarget::Bytecode(u32::MAX), None));
        heap.add_pinned_root(f);
        JsValue::object(f)
    };
    (mk(heap), mk(heap))
}

#[test]
fn the_capability_executor_has_length_two_and_an_empty_name() {
    // 27.5.1.5 step 4: `CreateBuiltinFunction(executorClosure, 2, "", «»)`.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut natives = EngineNatives::new();
    let state = capability_record(&mut heap);
    let executor = natives
        .capability_executor(&mut heap, JsValue::object(state))
        .expect("capability executor");
    let h = executor.as_object().expect("executor is an object");

    assert_eq!(own_prop(&mut heap, h, "length").0.as_smi(), Some(2));
    let empty = JsValue::string(heap.intern_text(""));
    assert_eq!(own_prop(&mut heap, h, "name").0, empty);
    // Same `BUILTIN` attrs the resolving functions carry: both properties are
    // non-writable, non-enumerable, configurable.
    let builtin = v12_heap::Attrs::new(false, false, true);
    assert_eq!(own_prop(&mut heap, h, "length").1, builtin);
    assert_eq!(own_prop(&mut heap, h, "name").1, builtin);
    // `length` before `name`, as `Promise/prototype/then/reject-function-property-order.js`
    // pins for the resolving pair.
    let length_key = PropKey::from_string(heap.intern_text("length"));
    let keys = heap.get(h).property_keys.clone();
    assert_eq!(keys[0].expect("`length` has a key"), length_key);
    // `«»`: the builtin carries no own `prototype`, which is what keeps it
    // non-constructible.
    let prototype_key = PropKey::from_string(heap.intern_text("prototype"));
    assert!(
        heap.lookup_property(heap.shape_of(h), prototype_key)
            .is_none(),
        "the capability executor must have no own `prototype` property"
    );
}

#[test]
fn the_capability_executor_records_the_pair_and_returns_undefined() {
    // 27.5.1.5 step 3: with both slots `undefined`, store the two functions
    // and return `undefined` — the constructor's own `Construct` result is the
    // object, not the executor's return value.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut natives = EngineNatives::new();
    let state = capability_record(&mut heap);
    let executor = natives
        .capability_executor(&mut heap, JsValue::object(state))
        .expect("capability executor");
    let (resolve, reject) = fake_pair(&mut heap);

    assert_eq!(
        call_host(&mut heap, executor, &[resolve, reject]).expect("first call"),
        JsValue::undefined()
    );
    let (got_resolve, got_reject) = natives
        .capability_pair(&mut heap, JsValue::object(state))
        .expect("capability pair");
    assert_eq!(got_resolve, resolve);
    assert_eq!(got_reject, reject);
}

#[test]
fn calling_the_capability_executor_twice_throws_a_type_error() {
    // 27.5.1.5 step 3, the guard: `If resolvingFunctions.[[Resolve]] is not
    // undefined or rejectingFunctions.[[Reject]] is not undefined, throw a
    // TypeError`. A `NewPromiseCapability` consumer that keeps the executor and
    // calls it again is the only way to reach it, and the throw must not
    // disturb the pair already recorded.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut natives = EngineNatives::new();
    let state = capability_record(&mut heap);
    let executor = natives
        .capability_executor(&mut heap, JsValue::object(state))
        .expect("capability executor");
    let (resolve, reject) = fake_pair(&mut heap);
    call_host(&mut heap, executor, &[resolve, reject]).expect("first call");

    let (other_resolve, other_reject) = fake_pair(&mut heap);
    let thrown = call_host(&mut heap, executor, &[other_resolve, other_reject])
        .expect_err("the second call must throw");
    assert_eq!(error_name(&mut heap, thrown), "TypeError");

    // The record still holds the *first* pair: the guard throws before either
    // store, so it is not a silent overwrite.
    let (got_resolve, got_reject) = natives
        .capability_pair(&mut heap, JsValue::object(state))
        .expect("capability pair");
    assert_eq!((got_resolve, got_reject), (resolve, reject));
}

#[test]
fn a_constructor_that_never_calls_the_executor_leaves_the_record_unset() {
    // The half of 27.5.1.5 steps 7-8 this seam can see: both slots stay
    // `undefined`, which is what makes the interpreter's `IsCallable` gate
    // throw for a constructor that ignores its executor.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut natives = EngineNatives::new();
    let state = capability_record(&mut heap);
    let _executor = natives
        .capability_executor(&mut heap, JsValue::object(state))
        .expect("capability executor");

    let (resolve, reject) = natives
        .capability_pair(&mut heap, JsValue::object(state))
        .expect("capability pair");
    assert_eq!(resolve, JsValue::undefined());
    assert_eq!(reject, JsValue::undefined());
}

// ---------------------------------------------------------------------------
// 27.5.1.1 with a derived constructor, and `Construct` reached from Reflect
// ---------------------------------------------------------------------------

#[test]
fn a_derived_constructor_initializes_its_own_instance_as_the_promise() {
    // `super(executor)` in a derived class is spec 9.7.1's `[[Construct]]`
    // chain: the base constructor initializes the receiver the derived
    // `OrdinaryCreateFromConstructor` already made, it does not allocate a
    // second object. Before the in-place arm the base allocated a *fresh*
    // promise and threw `TypeError: Promise constructor requires 'new'` on the
    // way, so the whole idiom was unavailable.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             class X extends Promise { \
               constructor(executor) { super(executor); } \
             } \
             globalThis.p = new X(function (resolve) { resolve(7); }); \
             globalThis.__shape = [p instanceof X, p instanceof Promise, \
                                   p.constructor === X].join(','); \
             globalThis.p.then(function (v) { globalThis.__out = 'v:' + v; }, \
                              function (e) { globalThis.__out = 'e:' + e; });",
            "globalThis.__out + '|' + globalThis.__shape"
        ),
        "v:7|true,true,true"
    );
}

#[test]
fn a_derived_constructor_own_properties_land_on_the_initialized_promise() {
    // The `this.tag = 1` half of the same idiom, split out because of a
    // pre-existing rule that is not this task's to change: `is_promise` counts
    // the three internal slots *exactly*, so an own property added to a promise
    // stops it answering as one, and `Promise.prototype.then` then throws
    // `requires a promise`. That is true of a plain `new Promise(...)` too, and
    // `is_promise`'s own comment names the `ctor-null` / `uniq-ctor` tests that
    // pin the exact count. So this asserts what *is* observable — the property
    // is on the promise the base constructor made — and the test above covers
    // settlement.
    assert_eq!(
        settled(
            "class X extends Promise { \
               constructor(executor) { super(executor); this.tag = 1; } \
             } \
             let p = new X(function (resolve) { resolve(7); }); \
             globalThis.__out = [p instanceof X, p instanceof Promise, p.tag, \
                                 p.constructor === X].join(',');",
            "globalThis.__out"
        ),
        "true,true,1,true"
    );
}

#[test]
fn reflect_construct_on_promise_runs_the_executor() {
    // `Reflect.construct(Promise, « executor »)` is the same 27.5.1.1 body as
    // `new Promise(executor)`; only the Call seam differs. This pins that the
    // constructor is in the interpreter's constructor oracle — the `%Promise%`
    // callable is an out-of-range bytecode index, so the oracle has to name it
    // explicitly for `Reflect.construct` to route it at all.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let p = Reflect.construct(Promise, [function (resolve) { resolve(3); }]); \
             p.then(function (v) { globalThis.__out = 'v:' + v; }, \
                    function (e) { globalThis.__out = 'e:' + e; });",
            "globalThis.__out"
        ),
        "v:3"
    );
}

#[test]
fn reflect_construct_on_a_subclass_yields_the_subclass_instance() {
    // `newTarget` is the subclass, so the capability promise's
    // `[[Prototype]]` is `X.prototype` and the result is an `X` — which is only
    // true because arm B keeps the prototype the construct chain installed
    // instead of overwriting it with the receiver's own `prototype` field.
    assert_eq!(
        settled(
            "class X extends Promise {} \
             let p = Reflect.construct(X, [function (resolve) { resolve(1); }]); \
             p.then(function () {}); \
             globalThis.__out = [p instanceof X, p instanceof Promise].join(',');",
            "globalThis.__out"
        ),
        "true,true"
    );
}

#[test]
fn promise_call_on_a_plain_object_throws() {
    // `%Promise%` has no usable `[[Call]]` (ES 27.2.3.1 step 1: `NewTarget`
    // is undefined), so calling it on a plain receiver is a `TypeError` and
    // leaves the receiver untouched. This was the one documented v1
    // divergence while a derived `super(executor)` reached `%Promise%`
    // through the *Call* seam; `super(…)` is a real `Construct` now.
    assert_eq!(
        settled(
            "let o = {}; let r; \
             try { Promise.call(o, function () {}); r = 'no throw'; } \
             catch (e) { r = (e instanceof TypeError) + ',' + \
                         (Object.getPrototypeOf(o) === Object.prototype); } \
             globalThis.__out = r;",
            "globalThis.__out"
        ),
        "true,true"
    );
}
