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

// -- Handler-result adoption (spec 27.5.2.1 NewPromiseReactionJob) --------
//
// A reaction job that writes the handler's return value straight into the
// derived promise settles that promise without ever consulting the capability
// 27.5.1.2 stored on the record — so a returned thenable is fulfilled *as
// itself* and a species constructor's custom `[[Resolve]]` is skipped.
// 27.5.2.1 step 4 instead routes the result through
// `Call(capability.[[Resolve]], undefined, « value »)`, and that resolve
// function is 27.5.1.3's, so adoption is what falls out of it for free.

/// Settles `source` and returns the display string of the global `__out`.
fn then_settled(source: &str) -> String {
    settled(source, "globalThis.__out")
}

/// Puts the window between the `then` and the reaction job under a full
/// mark-sweep *and* enough allocation behind it to drain the free list — the
/// only way to observe whether a job capture is rooted.
///
/// Three steps, and each is load-bearing:
///
/// 1. a `gc_protect` safepoint, so the heap's root set is *republished from the
///    live value stack*. The `then` parks the pair on the stack and calls
///    `gc_protect`, then truncates the stack without republishing, so until the
///    next safepoint the root set still lists the pair as a root. Collecting
///    before that refresh would mark the pair live for the wrong reason.
/// 2. `force_collect` — the engine collects at a safepoint or an explicit
///    `force_collect` and never on a plain allocation, so a churn loop on its
///    own collects nothing.
/// 3. a second churn, because the sweep is *lazy*: `force_collect` marks and
///    counts, and `sweep_step` releases dead slots only as later allocations
///    need them. Without this a dead pair is still readable and the test would
///    pass either way.
///
/// This is the technique `promise_capability_seam.rs` and
/// `async_generator_request_queue.rs` use.
fn adopt_settled_after_gc(source: &str) -> String {
    let mut engine = Engine::new();
    engine.eval(source).expect("eval");
    churn(&mut engine);
    engine.heap_mut().force_collect();
    churn(&mut engine);
    engine.run_jobs();
    let value = engine.eval("globalThis.__out.join(',')").expect("read");
    engine.to_display_string(value)
}

/// Allocates enough to hand every dead slot in the free list to a new object.
fn churn(engine: &mut Engine) {
    engine
        .eval(
            "globalThis.__churn = []; \
             for (var i = 0; i < 4000; i++) { globalThis.__churn.push({ n: i, s: 'churn' + i }); }",
        )
        .expect("churn");
}

#[test]
fn an_on_fulfilled_returning_a_thenable_adopts_it() {
    // 27.5.2.1 step 4 + 27.5.1.3: the handler's return value goes through the
    // capability's `[[Resolve]]`, which reads `then` and calls it — so the
    // derived promise fulfills with the *inner* value, not with the thenable.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { \
               return { then: function (resolve) { resolve('adopted'); } }; \
             }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=adopted"
    );
}

#[test]
fn an_on_fulfilled_returning_a_nested_thenable_adopts_through() {
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { \
               return { then: function (resolve) { \
                 resolve({ then: function (r2) { r2('deep'); } }); } }; \
             }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=deep"
    );
}

#[test]
fn a_returned_object_whose_then_getter_throws_rejects_the_derived_promise() {
    // 27.5.1.3.2 step 6-a: the abrupt `Get(value, "then")` rejects, and the
    // error carried is the getter's own.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { \
               return { get then() { throw new TypeError('boom'); } }; \
             }).then( \
               function (v) { globalThis.__out = 'fulfilled:' + v; }, \
               function (e) { globalThis.__out = e.name + ':' + e.message; });"
        ),
        "TypeError:boom"
    );
}

#[test]
fn a_returned_object_with_a_poisoned_then_fulfills_with_the_object() {
    // 27.5.1.3.2 step 6-b-i: a `then` that reads as `undefined` is not an
    // adoption, so the promise fulfills with the thenable itself.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { \
               return { tag: 'poison', then: undefined }; \
             }).then( \
               function (v) { globalThis.__out = v.tag; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "poison"
    );
}

#[test]
fn a_returned_object_with_a_getter_returning_a_non_callable_fulfills_with_it() {
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { \
               return { tag: 'lazy', get then() { return 42; } }; \
             }).then( \
               function (v) { globalThis.__out = v.tag; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "lazy"
    );
}

#[test]
fn a_handler_returning_the_derived_promise_rejects_with_a_type_error() {
    // 27.5.1.3.2 step 8: resolving a promise with itself must reject rather
    // than attach a reaction record to its own reactions array, which would
    // never drain.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             var derived = Promise.resolve('a').then(function () { \
               return derived; \
             }); \
             derived.then( \
               function (v) { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e.name; });"
        ),
        "TypeError"
    );
}

#[test]
fn a_handler_returning_undefined_fulfills_with_undefined() {
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { return undefined; }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=undefined"
    );
}

#[test]
fn a_throwing_handler_still_rejects_the_derived_promise() {
    // 27.5.2.1 step 4's abrupt arm: the handler's throw calls
    // `capability.[[Reject]]`.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { \
               throw new RangeError('nope'); \
             }).then( \
               function (v) { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e.name + ':' + e.message; });"
        ),
        "RangeError:nope"
    );
}

#[test]
fn an_on_rejected_returning_a_value_fulfills_the_derived_promise() {
    // The recovering case: a rejection handler's *return* value is a
    // resolution, not a rejection.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.reject('bad').then( \
               function () { globalThis.__out = 'unexpected'; }, \
               function (e) { return 'recovered:' + e; }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=recovered:bad"
    );
}

#[test]
fn an_on_rejected_returning_a_thenable_adopts_it() {
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.reject('bad').catch(function () { \
               return { then: function (resolve) { resolve('healed'); } }; \
             }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=healed"
    );
}

#[test]
fn a_chained_on_fulfilled_whose_predecessor_adopted_still_sees_the_inner_value() {
    // Two hops of adoption: the first handler's thenable is adopted, and the
    // *second* handler's result is adopted in turn. A per-promise adoption
    // count of one would show up as the outer thenable surfacing.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { \
               return { then: function (resolve) { resolve('one'); } }; \
             }).then(function (v) { \
               return { then: function (resolve) { resolve(v + '-two'); } }; \
             }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=one-two"
    );
}

// -- finally (spec 27.5.5.8) ----------------------------------------------

#[test]
fn a_finally_callback_returning_a_thenable_waits_and_keeps_the_watched_value() {
    // 27.5.5.8 steps 3-5: the callback's result is resolved (so a *thenable*
    // is adopted and the derived promise waits for it), but what the derived
    // promise ends up carrying is the **watched** value, not the callback's
    // return. `finally` is a pass-through, not a transform — the returned
    // thenable is observable only in that it delays.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').finally(function () { \
               return { then: function (resolve) { resolve('from-finally'); } }; \
             }).then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=a"
    );
}

#[test]
fn a_finally_callback_returning_a_rejecting_thenable_rejects_with_its_reason() {
    // The half of steps 3-5 that *is* observable in the value: an adoption
    // that fails abruptly replaces the outcome. This is the
    // `p.finally(() => Promise.reject(e))` case.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').finally(function () { \
               return { then: function (resolve, reject) { reject('from-finally'); } }; \
             }).then( \
               function (v) { globalThis.__out = 'fulfilled:' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "rejected:from-finally"
    );
}

#[test]
fn a_finally_callback_returning_a_rejected_promise_rejects_with_its_reason() {
    // The same rule with a real promise rather than a bare thenable: the
    // promise-adoption arm of `settle_with_adoption`.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').finally(function () { \
               return Promise.reject('from-finally'); \
             }).then( \
               function (v) { globalThis.__out = 'fulfilled:' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "rejected:from-finally"
    );
}

#[test]
fn a_throwing_finally_callback_rejects_with_its_error() {
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').finally(function () { \
               throw new RangeError('cleanup'); \
             }).then( \
               function (v) { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e.name + ':' + e.message; });"
        ),
        "RangeError:cleanup"
    );
}

#[test]
fn finally_still_passes_a_fulfilled_value_through() {
    // The overwhelmingly common shape: a `finally` that returns nothing must
    // not disturb the value, and must not swallow it either.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('kept').finally(function () { globalThis.__ran = 1; }).then( \
               function (v) { globalThis.__out = v + ':' + globalThis.__ran; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "kept:1"
    );
}

#[test]
fn finally_still_passes_a_rejection_through() {
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.reject('why').finally(function () { globalThis.__ran = 1; }).then( \
               function (v) { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e + ':' + globalThis.__ran; });"
        ),
        "why:1"
    );
}

#[test]
fn a_rejection_survives_a_finally_whose_callback_returns_a_value() {
    // 27.5.5.8: on the reject path the callback's return value replaces
    // nothing — the original reason rides through. A `finally` returning a
    // value must not turn a rejection into a fulfillment.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.reject('why').finally(function () { return 'ignored'; }).then( \
               function (v) { globalThis.__out = 'fulfilled:' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "rejected:why"
    );
}

#[test]
fn a_non_callable_finally_argument_leaves_the_outcome_alone() {
    // `onFinally` absent is not a call: 27.5.5.8 only calls it when
    // `IsCallable`, so `finally()` with no argument is a pass-through.
    assert_eq!(
        then_settled(
            "globalThis.__out = 'unset'; \
             Promise.resolve('kept').finally().then( \
               function (v) { globalThis.__out = 'v=' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });"
        ),
        "v=kept"
    );
}

// -- Species-selected capability (27.5.5.4 step 3 + 27.5.1.2) -------------

/// A species constructor that hands `executor` resolving functions of its
/// own, logging each half that runs. The result is a real promise (so `then`
/// is reachable from script) but the pair is *not* the one `%Promise%` made,
/// so anything that settles it directly instead of through the capability
/// cannot produce the log.
const LOGGED_CTOR: &str = "function Ctor(executor) { \
    var pair = null; \
    var p = new Promise(function (r, j) { pair = [r, j]; }); \
    executor( \
      function (v) { globalThis.__log.push('custom-resolve:' + v); pair[0](v); }, \
      function (e) { globalThis.__log.push('custom-reject:' + e); pair[1](e); }); \
    return p; \
  } \
  class Sub extends Promise {} \
  Object.defineProperty(Sub, Symbol.species, { value: Ctor }); ";

#[test]
fn a_species_constructor_supplies_its_own_capability_for_the_handler_result() {
    // The point of 27.5.2.1 step 4: the handler's result is handed to the
    // capability `NewPromiseCapability(C)` built, so a species constructor that
    // installs its own resolving functions sees the value.
    assert_eq!(
        then_settled(&format!(
            "globalThis.__out = 'unset'; globalThis.__log = []; \
             {LOGGED_CTOR} \
             new Sub(function (r) {{ r('a'); }}) \
               .then(function (v) {{ return v + '-handled'; }}) \
               .then( \
                 function (v) {{ globalThis.__out = globalThis.__log.join('|') + ' => ' + v; }}, \
                 function (e) {{ globalThis.__out = 'rejected:' + e; }});"
        )),
        "custom-resolve:a-handled => a-handled"
    );
}

#[test]
fn a_species_constructor_sees_a_rejected_handler_result_through_its_reject() {
    assert_eq!(
        then_settled(&format!(
            "globalThis.__out = 'unset'; globalThis.__log = []; \
             {LOGGED_CTOR} \
             new Sub(function (r) {{ r('a'); }}) \
               .then(function () {{ throw new RangeError('from-handler'); }}) \
               .then( \
                 function (v) {{ globalThis.__out = 'fulfilled'; }}, \
                 function (e) {{ globalThis.__out = globalThis.__log.join('|') + ' => ' + e.name; }});"
        )),
        "custom-reject:RangeError: from-handler => RangeError"
    );
}

#[test]
fn a_species_constructor_adopts_a_thenable_through_its_own_capability() {
    assert_eq!(
        then_settled(&format!(
            "globalThis.__out = 'unset'; globalThis.__log = []; \
             {LOGGED_CTOR} \
             new Sub(function (r) {{ r('a'); }}) \
               .then(function () {{ return {{ then: function (res) {{ res('adopted'); }} }}; }}) \
               .then( \
                 function (v) {{ globalThis.__out = globalThis.__log.join('|') + ' => ' + v; }}, \
                 function (e) {{ globalThis.__out = 'rejected:' + e; }});"
        )),
        "custom-resolve:[object Object] => adopted"
    );
}

#[test]
fn the_species_capability_survives_a_collection_between_then_and_the_job() {
    // The rooting argument for `[[Capability]]`, as a behaviour test.
    //
    // A `Box<dyn FnOnce(&mut JobCtx)>` is opaque to the collector, so a job's
    // captures are roots only because something pinned them. The window is
    // real: `then` returns, the reaction job is queued, and the source promise
    // may be unreachable from script before the job runs — the job holds the
    // pair in a Rust closure and nothing else.
    //
    // The sweep is *forced* rather than left to the growth policy, and followed
    // by a churn, because that is the only combination that puts the window
    // under the collector at all: the engine collects at a `Heap` safepoint or
    // an explicit `force_collect` and never on a plain allocation, and the
    // sweep is lazy, so a dead pair stays readable until later allocations
    // reuse its slot. A churn-only version of this test passes whether or not
    // anything is pinned, which is what the inherited one did.
    //
    // What it does *not* isolate: with all four routes of the pair removed —
    // `capability_executor`'s contract pin, `make_capability`'s two,
    // `install_then_record`'s, and `pin_job_captures`'s — it still passes, so
    // the pair's liveness rests on `capability_executor` pinning the capability
    // record (see `pin_job_captures`). This is a regression test for that
    // contract plus the reaction path as a whole, not for one line.
    //
    // The species constructor is the sharp case: it hands `executor` closures
    // of its own, so nothing about them is engine-allocated except the
    // reaction record's slots.
    assert_eq!(
        adopt_settled_after_gc(&format!(
            "globalThis.__out = []; globalThis.__log = []; \
             {LOGGED_CTOR} \
             new Sub(function (r) {{ r('a'); }}) \
               .then(function (v) {{ return v + '-handled'; }}) \
               .then( \
                 function (v) {{ globalThis.__out.push(v + '/' + globalThis.__log.join('+')); }}, \
                 function (e) {{ globalThis.__out.push('rejected:' + e); }});"
        )),
        "a-handled/custom-resolve:a-handled"
    );
}

#[test]
fn the_species_capability_survives_a_collection_behind_a_pending_source() {
    // Same window on the *pending* arm, where 27.5.2.1's record is what carries
    // `[[Capability]]`: the source is still pending when `then` installs the
    // record, and the record is drained only when the source settles — so the
    // pair has to survive a sweep with the record as its only in-heap route.
    let mut engine = Engine::new();
    engine
        .eval(&format!(
            "globalThis.__out = []; globalThis.__log = []; \
             {LOGGED_CTOR} \
             globalThis.__source = new Sub(function (r) {{ globalThis.__r = r; }}); \
             globalThis.__source \
               .then(function (v) {{ return v + '-handled'; }}) \
               .then( \
                 function (v) {{ globalThis.__out.push(v + '/' + globalThis.__log.join('+')); }}, \
                 function (e) {{ globalThis.__out.push('rejected:' + e); }});"
        ))
        .expect("eval");
    churn(&mut engine);
    engine.heap_mut().force_collect();
    churn(&mut engine);
    // Settle the source from a *fresh* program, so the promise holding it is
    // unreachable from anything the collector can see.
    engine.eval("globalThis.__r('a');").expect("settle");
    churn(&mut engine);
    engine.heap_mut().force_collect();
    churn(&mut engine);
    engine.run_jobs();
    let value = engine.eval("globalThis.__out.join(',')").expect("read");
    assert_eq!(
        engine.to_display_string(value),
        "a-handled/custom-resolve:a-handled"
    );
}

#[test]
fn adoption_survives_a_collection_under_the_default_constructor() {
    // The control for the two tests above: the default capability's halves are
    // host closures pinned inside `make_capability`, so the same forced sweep
    // changes nothing.
    assert_eq!(
        adopt_settled_after_gc(
            "globalThis.__out = []; \
             Promise.resolve('a').then(function (v) { return v + '-handled'; }).then( \
               function (v) { globalThis.__out.push(v); }, \
               function (e) { globalThis.__out.push('rejected:' + e); });"
        ),
        "a-handled"
    );
}

#[test]
fn a_never_settling_handler_leaves_the_checkpoint_quiescent() {
    // A handler that never returns is a handler that never settles its derived
    // promise: nothing in the reaction job may spin waiting for it.
    let mut engine = Engine::new();
    engine
        .eval(
            "globalThis.__out = 'unset'; \
             Promise.resolve('a').then(function () { return new Promise(function () {}); }).then( \
               function (v) { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = 'rejected'; });",
        )
        .expect("eval");
    engine.run_jobs();
    let value = engine.eval("globalThis.__out").expect("read");
    assert_eq!(engine.to_display_string(value), "unset");
}
