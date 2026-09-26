//! Arbitrary thenable adoption (spec 27.2.1.3.1 / 27.7.5.1).
//!
//! The `then` read is the resolution decision, so it happens once per
//! adoption: the interpreter reads it through the ordinary property path
//! when it parks an `await`, and the engine reads it inside the thenable job
//! it enqueues. Either way the *job* — never a synchronous re-entry — calls
//! `then(resolve, reject)`, so a thenable that never calls back leaves the
//! checkpoint quiescent instead of spinning.

use v12_engine::Engine;

/// Evaluates `source`, drains the microtask checkpoint, then returns the
/// display string of `read` (a follow-up expression against the mutated
/// global).
fn settled(source: &str, read: &str) -> String {
    let mut engine = Engine::new();
    engine.eval(source).expect("eval");
    engine.run_jobs();
    let value = engine.eval(read).expect("read");
    engine.to_display_string(value)
}

#[test]
fn promise_resolve_adopts_a_plain_thenable() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve({ then: function (resolve) { resolve('adopted'); } }).then( \
               function (v) { globalThis.__out = v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "adopted"
    );
}

#[test]
fn promise_resolve_adopts_a_nested_thenable() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve({ then: function (resolve) { \
               resolve({ then: function (r2) { r2(7); } }); } }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "v=7"
    );
}

#[test]
fn a_rejecting_thenable_propagates_the_exact_reason() {
    assert_eq!(
        settled(
            "globalThis.__reason = { tag: 'why' }; globalThis.__out = 'unset'; \
             let reason = globalThis.__reason; \
             Promise.resolve({ then: function (resolve, reject) { reject(reason); } }).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e === reason ? 'same' : 'other'; });",
            "globalThis.__out"
        ),
        "same"
    );
}

#[test]
fn a_throwing_then_getter_rejects_with_the_thrown_error() {
    // `Promise/resolve/arg-poisoned-then.js`: the abrupt `Get` rides the
    // promise, it does not escape `Promise.resolve`.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve({ get then() { throw new TypeError('poisoned'); } }).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e instanceof TypeError ? 'TypeError:' + e.message : 'other'; });",
            "globalThis.__out"
        ),
        "TypeError:poisoned"
    );
}

#[test]
fn a_non_callable_then_fulfills_with_the_object() {
    // Spec 27.2.1.3.1 step 3-b-i: a present but non-callable `then` is not
    // an adoption, the promise fulfills with the thenable itself. Pinned by
    // `Promise/resolve/arg-non-thenable.js` (`then: null`).
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let obj = { then: null }; \
             Promise.resolve(obj).then( \
               function (v) { globalThis.__out = v === obj ? 'same' : 'other'; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "same"
    );
}

#[test]
fn a_non_callable_then_returned_by_a_getter_fulfills_with_the_object() {
    // Same rule when `then` only becomes visible through a getter: the
    // adoption decision is made after the read, wherever it happens.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let obj = { get then() { return 1; } }; \
             Promise.resolve(obj).then( \
               function (v) { globalThis.__out = v === obj ? 'same' : 'other'; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "same"
    );
}

#[test]
fn a_throwing_then_call_rejects_with_the_thrown_error() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve({ then: function () { throw new RangeError('inside'); } }).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e instanceof RangeError ? 'RangeError:' + e.message : 'other'; });",
            "globalThis.__out"
        ),
        "RangeError:inside"
    );
}

#[test]
fn await_resumes_after_the_thenable_job_runs() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             (async function () { \
               let v = await { then: function (resolve) { resolve('awaited'); } }; \
               globalThis.__out = v; \
             })();",
            "globalThis.__out"
        ),
        "awaited"
    );
}

#[test]
fn await_adopts_a_nested_thenable() {
    // One async body per engine: a *second* concurrent async body whose await
    // rejects never resumes its `catch` (pre-existing at 3d7489fd, unrelated to
    // adoption), so the two shapes are checked in isolation.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             (async function () { \
               globalThis.__out = await { then: function (r) { \
                 r({ then: function (r2) { r2('deep'); } }); } }; \
             })();",
            "globalThis.__out"
        ),
        "deep"
    );
}

#[test]
fn await_rejects_on_a_rejecting_thenable() {
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             (async function () { \
               try { await { then: function (resolve, reject) { reject('nope'); } }; \
                     globalThis.__out += 'no-throw'; } \
               catch (e) { globalThis.__out += '|' + e; } \
             })();",
            "globalThis.__out"
        ),
        "|nope"
    );
}

#[test]
fn a_never_settling_thenable_leaves_the_checkpoint_quiescent() {
    let mut engine = Engine::new();
    engine
        .eval(
            "globalThis.__out = 'unset'; \
             Promise.resolve({ then: function () {} }).then( \
               function (v) { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = 'rejected'; });",
        )
        .expect("eval");
    // Terminates because the thenable job runs once and the promise stays
    // pending: a second adoption pass would spin.
    engine.run_jobs();
    let value = engine.eval("globalThis.__out").expect("read");
    assert_eq!(engine.to_display_string(value), "unset");
}

#[test]
fn a_never_settling_await_leaves_the_checkpoint_quiescent() {
    let mut engine = Engine::new();
    engine
        .eval(
            "globalThis.__out = 'unset'; \
             (async function () { \
               await { then: function () {} }; \
               globalThis.__out = 'resumed'; \
             })();",
        )
        .expect("eval");
    engine.run_jobs();
    let value = engine.eval("globalThis.__out").expect("read");
    assert_eq!(engine.to_display_string(value), "unset");
}

#[test]
fn a_promise_resolved_with_a_thenable_adopts_it() {
    // The capability resolve function, not just `Promise.resolve`: this is
    // the chokepoint `await` and the combinators route through.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             new Promise(function (resolve) { \
               resolve({ then: function (r) { r('from-executor'); } }); \
             }).then( \
               function (v) { globalThis.__out = v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "from-executor"
    );
}

#[test]
fn adoption_does_not_disturb_plain_promise_resolution() {
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             Promise.resolve('a').then(function (v) { globalThis.__out += v; }); \
             Promise.resolve(23).then(function (v) { globalThis.__out += v; }); \
             Promise.reject('r').then( \
               function () {}, function (e) { globalThis.__out += '!' + e; });",
            "globalThis.__out"
        ),
        "a23!r"
    );
}
