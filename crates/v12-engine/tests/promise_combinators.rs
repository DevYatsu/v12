//! `Promise.all` / `Promise.race` over general iterables. The iterator half
//! of the spec algorithm (GetIterator, IteratorStep/Value, IteratorClose)
//! re-enters the machine and runs in the interpreter; the settled-promise
//! aggregation (reaction watchers) stays in the engine handler.

use v12_engine::Engine;

/// Evaluates `source` and returns the thrown value's display string.
fn display(source: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(source).expect_err("source must throw");
    engine.to_display_string(value)
}

/// Evaluates `source`, drains the job queue, then returns the display string
/// of `read` (a follow-up expression against the mutated global).
fn settled(source: &str, read: &str) -> String {
    let mut engine = Engine::new();
    engine.eval(source).expect("eval");
    engine.run_jobs();
    let value = engine.eval(read).expect("read");
    engine.to_display_string(value)
}

#[test]
fn all_accepts_a_set_and_preserves_iteration_order() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.all(new Set([1, 2, 3])).then( \
               function (values) { globalThis.__out = values.join('-'); }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "1-2-3"
    );
}

#[test]
fn all_accepts_a_custom_iterable_and_iterates_exhaustively() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let calls = 0; \
             let iterable = { [Symbol.iterator]: function () { return { next: function () { \
               calls++; return calls > 3 ? { done: true } : { value: calls * 10, done: false }; \
             } }; } }; \
             Promise.all(iterable).then( \
               function (values) { globalThis.__out = values.join('-') + '/' + calls; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "10-20-30/4"
    );
}

#[test]
fn race_accepts_a_custom_iterable_and_settles_with_the_first_input() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let n = 0; \
             let iterable = { [Symbol.iterator]: function () { return { next: function () { \
               n++; return n > 2 ? { done: true } : { value: 'v' + n, done: false }; \
             } }; } }; \
             Promise.race(iterable).then( \
               function (v) { globalThis.__out = 'won:' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "won:v1"
    );
}

#[test]
fn combinators_accept_a_string_and_a_generator_iterable() {
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             Promise.all('ab').then(function (v) { globalThis.__out += v.join('-'); }); \
             function* gen() { yield 'g1'; yield 'g2'; } \
             Promise.all(gen()).then(function (v) { globalThis.__out += '|' + v.join('-'); });",
            "globalThis.__out"
        ),
        "a-b|g1-g2"
    );
}

#[test]
fn all_keeps_result_positions_for_a_sparse_input() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.all([1, , 3]).then( \
               function (v) { globalThis.__out = v.length + ':' + String(v[0]) + ',' + String(v[1]) + ',' + v[2]; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "3:1,undefined,3"
    );
}

#[test]
fn all_uses_an_overridden_array_iterator() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let original = Array.prototype[Symbol.iterator]; \
             Array.prototype[Symbol.iterator] = function () { \
               let n = 0; \
               return { next: function () { n++; return n > 2 ? { done: true } : { value: n, done: false }; } }; \
             }; \
             Promise.all([9, 9, 9]).then( \
               function (v) { globalThis.__out = v.join('-'); }, \
               function (e) { globalThis.__out = 'rejected:' + e; }); \
             Array.prototype[Symbol.iterator] = original;",
            "globalThis.__out"
        ),
        "1-2"
    );
}

#[test]
fn all_rejects_with_the_exact_rejection_value_of_each_input() {
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             Promise.reject('first').then(function () {}, function (e) { globalThis.__out += e; }); \
             Promise.reject('second').then(function () {}, function (e) { globalThis.__out += '|' + e; });",
            "globalThis.__out"
        ),
        "first|second"
    );
}

#[test]
fn all_propagates_a_rejection_object_identity() {
    assert_eq!(
        settled(
            "globalThis.__reason = { tag: 'why' }; globalThis.__out = 'unset'; \
             let reason = globalThis.__reason; \
             Promise.reject(reason).then(function () {}, function (e) { globalThis.__out = e === reason; });",
            "globalThis.__out"
        ),
        "true"
    );
}

#[test]
fn all_rejects_without_closing_when_next_throws() {
    // `PerformPromiseAll` rejects where the abrupt happened
    // (`IfAbruptRejectPromise` on the IteratorStepValue), so `return` is never
    // called — pinned by `Promise/all/iter-next-val-err-no-close.js`.
    assert_eq!(
        settled(
            "globalThis.__closed = 0; globalThis.__out = 'unset'; \
             let iterable = { [Symbol.iterator]: function () { return { \
               next: function () { throw new Error('next'); }, \
               return: function () { globalThis.__closed++; return {}; } \
             }; } }; \
             Promise.all(iterable).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e.message + '/' + globalThis.__closed; });",
            "globalThis.__out"
        ),
        "next/0"
    );
}

#[test]
fn combinators_reject_non_iterable_and_bad_iterators() {
    // Every one of these rejects rather than throwing out of the call: the
    // capability is created before `GetIterator`, so an abrupt there rejects
    // it — pinned by `Promise/all/iter-arg-is-undefined-reject.js`.
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             let label = function (tag, p) { p.then( \
               function () { globalThis.__out += tag + 'F'; }, \
               function (e) { globalThis.__out += tag + (e instanceof TypeError ? 'T' : 'X'); }); }; \
             label('u', Promise.all(undefined)); \
             label('n', Promise.all({})); \
             label('r', Promise.race(1)); \
             label('i', Promise.all({ [Symbol.iterator]: 1 })); \
             label('o', Promise.all({ [Symbol.iterator]: function () { return 1; } })); \
             label('x', Promise.all({ [Symbol.iterator]: function () { \
               return { next: function () { return 1; } }; } }));",
            "globalThis.__out"
        ),
        "uTnTrTiToTxT"
    );
}

#[test]
fn combinators_reject_a_non_constructor_receiver_before_touching_the_iterator() {
    // Spec 27.2.4.1 step 6 / 27.2.4.2 step 3: `NewPromiseCapability(C)` opens
    // with `If IsConstructor(C) is false, throw a TypeError`, so the throw
    // happens *before* `GetIterator` — the `next`/`return` counters below stay
    // at zero. Pinned by `Promise/{all,race}/ctx-non-ctor.js`.
    assert_eq!(
        display(
            "let pulled = 0; let closed = 0; \
             let iterable = { [Symbol.iterator]: function () { return { \
               next: function () { pulled++; return { done: false, value: 1 }; }, \
               return: function () { closed++; return {}; } }; } }; \
             let thrown = []; \
             for (let C of [eval, {}, 42, 'str', null]) { \
               try { Promise.all.call(C, iterable); } catch (e) { thrown.push(e instanceof TypeError); } \
               try { Promise.race.call(C, iterable); } catch (e) { thrown.push(e instanceof TypeError); } \
             } \
             if (!thrown.every(function (t) { return t; })) { throw new Error('non-TypeError: ' + thrown); } \
             if (pulled !== 0 || closed !== 0) { throw new Error('iterator touched: ' + pulled + '/' + closed); } \
             throw new TypeError('done');",
        ),
        "TypeError: done"
    );
}

#[test]
fn combinators_accept_constructor_receivers() {
    // The other half of the same gate: a real constructor must keep working —
    // the `Promise` constructor itself, a subclass, a plain function, a class,
    // and a proxy over one. Each carries the `resolve` that
    // `PerformPromiseAll` step 3 calls, and each fulfills with the results
    // array (or the race winner), so this also pins that the gate did not
    // start rejecting ordinary receivers.
    //
    // The two hand-written constructors return a real promise and pass it the
    // pair, which is what 27.5.1.5 wants: the pair must be callable (steps 8-9)
    // and the object is what script gets back, so a plain object here would
    // leave the `.then` below with nothing to call.
    //
    // `var`, not `let`, for the locals: constructing a derived class runs its
    // `super(…)`, and the default-derived lowering walks the env chain to find
    // the captured parent constructor. That walk miscounts when the enclosing
    // program is env-bearing, so `let` here makes `Construct(Sub, …)` read the
    // wrong env slot and fail with "callee is not a function". Pre-existing and
    // unrelated to promises (it reproduces with `class B {}` / `class D extends
    // B { constructor(f) { super(f); } }` and no promise in sight), so this test
    // steps around it rather than papering over it — a `let` in any of these
    // scopes would fail the whole case, not just the `Sub` line.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             class Sub extends Promise {} \
             function Fn(executor) { return new Promise(function (res, rej) { executor(res, rej); }); } \
             Fn.resolve = function (v) { return Promise.resolve(v); }; \
             class Cls { constructor(executor) { return new Promise(function (res, rej) { executor(res, rej); }); } } \
             Cls.resolve = Fn.resolve; \
             function one() { return { [Symbol.iterator]: function () { var n = 0; return { \
               next: function () { n++; return n > 1 ? { done: true } : { done: false, value: 'v' }; }, \
               return: function () { return {}; } }; } }; } \
             var seen = []; \
             var collect = function (p, tag) { p.then( \
               function (v) { seen.push(tag + '=' + (Array.isArray(v) ? v.join('-') : String(v))); }, \
               function (e) { seen.push(tag + '!rej:' + e); }); }; \
             collect(Promise.all.call(Promise, one()), 'Promise'); \
             collect(Promise.all.call(Sub, one()), 'Sub'); \
             collect(Promise.all.call(Fn, one()), 'Fn'); \
             collect(Promise.all.call(Cls, one()), 'Cls'); \
             collect(Promise.all.call(new Proxy(Promise, {}), one()), 'Proxy'); \
             collect(Promise.race.call(Sub, one()), 'race'); \
             Promise.resolve().then(function () { globalThis.__out = seen.join('|'); });",
            "globalThis.__out"
        ),
        "Promise=v|Sub=v|Fn=v|Cls=v|Proxy=v|race=v"
    );
}

#[test]
fn race_settles_with_the_first_element_in_reaction_order() {
    // `Promise.race([settled])` used to fulfill with the (never written)
    // results array the moment the iterator ran out, so the winner lost every
    // microtask race that the spec orders after the `then` invocation
    // (27.2.4.2 step 8-f returns the capability promise unchanged). The order
    // below is the spec's: the `then` reaction on the already-fulfilled input
    // runs first, and only then the handler the race result's own `then`
    // registered.
    let source = concat!(
        "globalThis.__out = 'unset'; let sequence = [];",
        "let a = Promise.resolve('a');",
        "let race = Promise.race([a]);",
        "race.then(function (winner) { sequence.push('handler:' + winner); });",
        "a.then(function () { sequence.push('input'); });",
        "Promise.resolve().then(function () { sequence.push('tick'); });",
        "Promise.resolve().then(function () { sequence.push('tick2'); })",
        ".then(function () { globalThis.__out = sequence.join(','); });",
    );
    assert_eq!(
        settled(source, "globalThis.__out"),
        "input,tick,tick2,handler:a"
    );
}

#[test]
fn a_throwing_done_or_value_getter_rejects_without_closing() {
    // Both reads are part of IteratorStepValue, so they reject in place and
    // `return` is never reached.
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             let closed = 0; \
             let doneThrow = { [Symbol.iterator]: function () { return { \
               next: function () { return Object.defineProperty({}, 'done', { get: function () { throw new Error('done'); } }); }, \
               return: function () { closed++; return {}; } \
             }; } }; \
             Promise.all(doneThrow).then( \
               function () {}, function (e) { globalThis.__out += e.message + '/' + closed; }); \
             let closed2 = 0; \
             let valueThrow = { [Symbol.iterator]: function () { return { \
               next: function () { return { get done() { return false; }, get value() { throw new Error('value'); } }; }, \
               return: function () { closed2++; return {}; } \
             }; } }; \
             Promise.race(valueThrow).then( \
               function () {}, function (e) { globalThis.__out += '|' + e.message + '/' + closed2; });",
            "globalThis.__out"
        ),
        "done/0|value/0"
    );
}

#[test]
fn a_normal_completion_does_not_call_return() {
    assert_eq!(
        display(
            "let closed = 0; \
             let iterable = { [Symbol.iterator]: function () { let n = 0; return { \
               next: function () { n++; return n > 2 ? { done: true } : { value: n, done: false }; }, \
               return: function () { closed++; return {}; } \
             }; } }; \
             Promise.all(iterable); \
             throw 'closed=' + closed;"
        ),
        "closed=0"
    );
}

#[test]
fn an_element_abort_closes_the_iterator_and_keeps_the_original_error() {
    // The element half of the loop closes: a `return()` that throws must not
    // mask the error that got us there (spec 7.4.11 — a throw completion wins).
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let iterable = { [Symbol.iterator]: function () { let n = 0; return { \
               next: function () { n++; return n > 1 ? { done: true } : { value: { then: function () { throw new Error('original'); } }, done: false }; }, \
               return: function () { throw new Error('from return'); } \
             }; } }; \
             Promise.all(iterable).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e.message; });",
            "globalThis.__out"
        ),
        "original"
    );
}

#[test]
fn a_two_element_infinite_iterator_closes_when_the_second_then_throws() {
    // The regression this streaming correction exists for: the loop pulls one
    // element at a time, so the second element's throwing `then` closes the
    // iterator and rejects — the iterator's end is never reached.
    let source = concat!(
        "globalThis.__out = 'unset'; globalThis.__closed = 0; globalThis.__pulled = 0;",
        "let iterable = { [Symbol.iterator]: function () { let n = 0; return {",
        "next: function () { n++; globalThis.__pulled = n;",
        "return { done: false, value: { then: function () {",
        "if (globalThis.__pulled > 1) { throw new Error('boom'); } } } }; },",
        "return: function () { globalThis.__closed++; return {}; } }; } };",
        "Promise.all(iterable).then(",
        "function () { globalThis.__out = 'fulfilled'; },",
        "function (e) { globalThis.__out = e.message + '/closed=' + globalThis.__closed",
        "+ '/pulled=' + globalThis.__pulled; });",
    );
    assert_eq!(
        settled(source, "globalThis.__out"),
        "boom/closed=1/pulled=2"
    );
}

#[test]
fn all_closes_when_a_promise_element_then_throws() {
    // Test262 `built-ins/Promise/all/invoke-then-error-close.js`: the element is
    // a *promise* carrying an own `then` that throws, over an iterator that
    // never ends. Two things must hold at once: the own `then` must be read
    // off the promise (an internal-slot object — its three
    // `[[State]]`/`[[Result]]`/`[[Reactions]]` entries used to be numbered
    // from 0, so the first user property aliased them and the read answered
    // the `[[State]]` Smi), and the pull loop must break out instead of
    // treating a non-callable `then` as "settled" and spinning forever.
    let source = concat!(
        "globalThis.__out = 'unset'; globalThis.__closed = 0; globalThis.__pulled = 0;",
        "let err = new Error('boom');",
        "let promise = new Promise(function () {});",
        "Object.defineProperty(promise, 'then', { value: function () { throw err; } });",
        "let iterable = { [Symbol.iterator]: function () { return {",
        "next: function () { globalThis.__pulled++;",
        "return { done: false, value: promise }; },",
        "return: function () { globalThis.__closed++; return {}; } }; } };",
        "Promise.all(iterable).then(",
        "function () { globalThis.__out = 'fulfilled'; },",
        "function (e) { globalThis.__out = (e === err ? 'boom' : 'other:' + e)",
        "+ '/closed=' + globalThis.__closed + '/pulled=' + globalThis.__pulled; });",
    );
    assert_eq!(
        settled(source, "globalThis.__out"),
        "boom/closed=1/pulled=1"
    );
}

#[test]
fn race_closes_when_an_element_then_throws() {
    let source = concat!(
        "globalThis.__out = 'unset'; globalThis.__closed = 0; globalThis.__pulled = 0;",
        "let iterable = { [Symbol.iterator]: function () { let n = 0; return {",
        "next: function () { n++; globalThis.__pulled = n;",
        "return { done: false, value: { then: function () {",
        "if (globalThis.__pulled > 1) { throw new Error('boom'); } } } }; },",
        "return: function () { globalThis.__closed++; return {}; } }; } };",
        "Promise.race(iterable).then(",
        "function () { globalThis.__out = 'fulfilled'; },",
        "function (e) { globalThis.__out = e.message + '/' + globalThis.__closed; });",
    );
    assert_eq!(settled(source, "globalThis.__out"), "boom/1");
}

#[test]
fn a_get_then_abrupt_closes_the_iterator() {
    // The other element-processing abrupt: reading `then` throws.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; globalThis.__closed = 0; \
             let iterable = { [Symbol.iterator]: function () { return { \
               next: function () { return { done: false, value: { \
                 get then() { throw new Error('get-then'); } } }; }, \
               return: function () { globalThis.__closed++; return {}; } \
             }; } }; \
             Promise.all(iterable).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e.message + '/' + globalThis.__closed; });",
            "globalThis.__out"
        ),
        "get-then/1"
    );
}

#[test]
// The spec names this receiver `C` (27.2.4.1 step 3); keep the spelling.
#[allow(non_snake_case)]
fn a_throwing_C_resolve_closes_the_iterator_and_rejects() {
    // `C.resolve` is the first element step (spec 27.5.4.1 step 11-b), so an
    // abrupt from it closes too and the aggregate rejects with it.
    //
    // `C` returns a real promise, because `[[Promise]]` is now whatever the
    // constructor built (27.5.1.5 step 6) — 27.5.1.5 checks only that the pair
    // is callable, so a constructor returning a plain object hands
    // `Promise.all` a non-promise, and the `.then` below would have nothing to
    // call. The pair it records is the inner promise's own, so the rejection
    // this pins is observable on it.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; globalThis.__closed = 0; \
             function C(executor) { return new Promise(function (res, rej) { executor(res, rej); }); } \
             C.resolve = function () { throw new Error('bad resolve'); }; \
             let iterable = { [Symbol.iterator]: function () { let n = 0; return { \
               next: function () { n++; return { done: false, value: 1 }; }, \
               return: function () { globalThis.__closed++; return {}; } \
             }; } }; \
             Promise.all.call(C, iterable).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e.message + '/' + globalThis.__closed; });",
            "globalThis.__out"
        ),
        "bad resolve/1"
    );
}

#[test]
fn all_of_an_empty_iterable_fulfills_with_an_empty_array() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.all([]).then( \
               function (v) { globalThis.__out = v.length + ':' + Object.prototype.toString.call(v); }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "0:[object Array]"
    );
}

#[test]
fn all_of_an_empty_string_fulfills_with_an_empty_array() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.all('').then( \
               function (v) { globalThis.__out = String(v.length); }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "0"
    );
}

#[test]
fn all_of_undefined_rejects_instead_of_throwing() {
    // The capability exists before `GetIterator` runs, so a bad argument
    // rejects it — pinned by `Promise/all/iter-arg-is-undefined-reject.js`.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.all(undefined).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = e instanceof TypeError ? 'TypeError' : 'other'; });",
            "globalThis.__out"
        ),
        "TypeError"
    );
}

#[test]
fn all_adopts_a_thenable_element_and_orders_the_results() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.all([1, { then: function (r) { r(2); } }, 3]).then(                function (v) { globalThis.__out = v.join('-'); },                function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "1-2-3"
    );
}

#[test]
fn combinator_results_are_promises_with_the_expected_shape() {
    assert_eq!(
        display(
            "let a = Promise.all([1]); let r = Promise.race([1]); \
             throw [a instanceof Promise, r instanceof Promise, \
             Object.getPrototypeOf(a) === Promise.prototype, \
             typeof a.then, typeof r.catch].join(',');"
        ),
        "true,true,true,function,function"
    );
}

// ---------------------------------------------------------------------------
// Settlement through the capability (spec 27.5.4.1 / 27.5.4.2)
// ---------------------------------------------------------------------------

/// The receiver every "did it go through the capability?" test below uses: a
/// constructor that hands its executor a pair of its *own* wrapping functions,
/// counting how often each half is called. `box` is a plain object the test
/// reads the counters out of — the aggregate is the promise the constructor
/// returns, so the counters cannot live on it (they are set while `new` is still
/// running) and they must not live on `this` either, which `Construct` discards
/// when the constructor returns an object.
const COUNTING_CTOR: &str = "var box = { resolveCalls: 0, rejectCalls: 0 }; \
  function Counting(executor) { \
    return new Promise(function (res, rej) { \
      executor( \
        function (v) { box.resolveCalls++; return res(v); }, \
        function (e) { box.rejectCalls++; return rej(e); }); \
    }); \
  } \
  Counting.resolve = function (v) { return Promise.resolve(v); };";

/// `source` with [`COUNTING_CTOR`] prepended.
fn with_counting(source: &str) -> String {
    format!("{COUNTING_CTOR}\n{source}")
}

#[test]
fn an_empty_all_settles_through_the_capability_resolve() {
    // Spec 27.5.4.1 steps 2 and 13: the aggregate is
    // `promiseCapability.[[Promise]]`, and the values array reaches it through
    // `Call(promiseCapability.[[Resolve]], undefined, « valuesArray »)`. An
    // implementation that wrote the promise's own slots would leave
    // `resolveCalls` at zero — which is what this pins against. The counters are
    // read off `agg` because the aggregate *is* the object the constructor made,
    // and that is where they live.
    assert_eq!(
        settled(
            &with_counting(
                "globalThis.__out = 'unset'; \
                 var agg = Promise.all.call(Counting, []); \
                 agg.then( \
                   function (v) { globalThis.__out = 'v=' + v.length + ' r=' + box.resolveCalls + ' j=' + box.rejectCalls; }, \
                   function (e) { globalThis.__out = 'rejected:' + e; });"
            ),
            "globalThis.__out"
        ),
        "v=0 r=1 j=0"
    );
}

#[test]
fn an_empty_race_leaves_the_capability_promise_alone() {
    // The same for 27.5.4.2: at exhaustion the spec returns the capability
    // promise *unchanged* (step 8-f) and no values array ever reaches it, so the
    // aggregate is still pending after the checkpoint. This is the
    // `KIND_RACE`-at-exhaustion rule `07612634` restored, seen from the
    // capability's side: a race that settled here would be the bug that fix
    // removed.
    assert_eq!(
        settled(
            &with_counting(
                "globalThis.__out = 'unset'; \
                 var settled = 'pending'; \
                 Promise.race.call(Counting, []).then( \
                   function () { settled = 'fulfilled'; }, \
                   function () { settled = 'rejected'; }); \
                 Promise.resolve().then(function () { globalThis.__out = settled; });"
            ),
            "globalThis.__out"
        ),
        "pending"
    );
}

#[test]
fn a_race_element_settles_through_the_capability_pair_verbatim() {
    // 27.5.4.2 step 8-e hands `resultCapability`'s own functions to `then`, so
    // the element's callbacks *are* the constructor's `resolve`/`reject`.
    // `resolveCalls` moving is what distinguishes that from a per-element
    // wrapper pair, which would settle the promise without ever calling them.
    assert_eq!(
        settled(
            &with_counting(
                "globalThis.__out = 'unset'; \
                 var agg = Promise.race.call(Counting, [Promise.resolve('w')]); \
                 agg.then( \
                   function (v) { globalThis.__out = v + ' r=' + box.resolveCalls + ' j=' + box.rejectCalls; }, \
                   function (e) { globalThis.__out = 'rejected:' + e; });"
            ),
            "globalThis.__out"
        ),
        "w r=1 j=0"
    );
}

#[test]
fn a_rejecting_element_settles_an_all_through_the_capability_reject() {
    // 27.5.4.1 step 11-b: the element's reject half is
    // `promiseCapability.[[Reject]]` verbatim, so an element rejection is a call
    // of the constructor's own reject function.
    assert_eq!(
        settled(
            &with_counting(
                "globalThis.__out = 'unset'; \
                 var agg = Promise.all.call(Counting, [Promise.reject('nope')]); \
                 agg.then( \
                   function () { globalThis.__out = 'fulfilled'; }, \
                   function (e) { globalThis.__out = e + ' r=' + box.resolveCalls + ' j=' + box.rejectCalls; });"
            ),
            "globalThis.__out"
        ),
        "nope r=0 j=1"
    );
}

#[test]
fn the_last_all_element_settles_through_the_capability_resolve() {
    // 27.5.4.1.1 step 4-b-iii-2: `onFulfilledElement` is the *only* element
    // handler left, and the one that calls `[[Resolve]]` with the values array
    // once the count reaches zero. So a non-empty `all` reaches the capability's
    // resolve exactly once, from the last element's reaction — not from the
    // loop, and not once per element.
    assert_eq!(
        settled(
            &with_counting(
                "globalThis.__out = 'unset'; \
                 var agg = Promise.all.call(Counting, [Promise.resolve(1), Promise.resolve(2)]); \
                 agg.then( \
                   function (v) { globalThis.__out = v.join('-') + ' r=' + box.resolveCalls + ' j=' + box.rejectCalls; }, \
                   function (e) { globalThis.__out = 'rejected:' + e; });"
            ),
            "globalThis.__out"
        ),
        "1-2 r=1 j=0"
    );
}

#[test]
fn a_throwing_capability_resolve_rejects_the_capability_promise() {
    // 27.5.4.1.1 step 4-b-iii-2 is `Perform ? Call([[Resolve]], …)`, and the `?`
    // sends the throw to `Promise.all`'s own `IfAbruptRejectPromise`, which
    // rejects through `[[Reject]]`. So a receiver whose resolve throws sees its
    // reject called with that very value — the identity check is the point, and
    // `rejectCalls` is what makes it a rejection *through the capability* rather
    // than a slot write. `Promise/all/capability-resolve-throws-reject.js` is
    // the test262 half.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             var thrown = { tag: 'why' }; \
             var box = { rejectCalls: 0 }; \
             function C(executor) { \
               return new Promise(function (res, rej) { \
                 var wrapped = function (e) { box.rejectCalls++; return rej(e); }; \
                 executor(function () { throw thrown; }, wrapped); \
               }); \
             } \
             C.resolve = function () { return Promise.resolve('x'); }; \
             Promise.all.call(C, []).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = (e === thrown) + '/' + box.rejectCalls; });",
            "globalThis.__out"
        ),
        "true/1"
    );
}

#[test]
fn a_throwing_or_uncallable_receiver_resolve_rejects_rather_than_throwing() {
    // 27.5.4.1 step 4 is `IfAbruptRejectPromise(promiseResolve, promiseCapability)`,
    // where step 3's `GetPromiseResolve(C)` is the `? Get` — so a throwing
    // `resolve` getter *and* a non-callable `resolve` both reject the capability
    // and the call still returns it. That is the split from step 2's
    // `NewPromiseCapability(C)`, which has no such step and therefore throws
    // (the test above). It is also why this runs before `GetIterator`: the
    // iterator's `@@iterator` getter below would throw if it were reached.
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             var thrown = { tag: 'getter' }; \
             function C(executor) { return new Promise(function (res, rej) { executor(res, rej); }); } \
             var poison = Object.defineProperty(C, 'resolve', { \
               configurable: true, \
               get: function () { throw thrown; } }); \
             var iterable = { [Symbol.iterator]: function () { \
               return { get next() { throw new Error('iterator reached'); } }; } }; \
             Promise.all.call(C, iterable).then( \
               function () { globalThis.__out += 'F'; }, \
               function (e) { globalThis.__out += (e === thrown ? 'getter' : 'other'); }); \
             Object.defineProperty(C, 'resolve', { value: null }); \
             Promise.race.call(C, iterable).then( \
               function () { globalThis.__out += 'F'; }, \
               function (e) { globalThis.__out += (e instanceof TypeError ? 'null' : 'other'); });",
            "globalThis.__out"
        ),
        "getternull"
    );
}

#[test]
fn a_constructor_that_ignores_its_executor_makes_the_combinator_throw() {
    // 27.5.1.5 steps 7-8: the recorded `[[Resolve]]` is still `undefined`, so
    // `IsCallable` fails and the `TypeError` is thrown — before `GetIterator`, so
    // the iterator is never touched. `Promise/{all,race}/capability-executor-not-callable.js`
    // pins this; its receivers additionally make `Get(C, "resolve")` throw,
    // which pins that the two `IsCallable` checks come *before* that read.
    assert_eq!(
        display(
            "var pulled = 0; \
             var kinds = []; \
             function Ignores(executor) { } \
             Object.defineProperty(Ignores, 'resolve', { \
               get: function () { throw new Error('resolve must not be read'); } }); \
             var iterable = { [Symbol.iterator]: function () { return { \
               next: function () { pulled++; return { done: false, value: 1 }; } }; } }; \
             var attempt = function (tag, f) { \
               try { f(); kinds.push(tag + '=none'); } \
               catch (e) { kinds.push(tag + '=' + (e instanceof TypeError ? 'TypeError' : 'other')); } }; \
             attempt('all', function () { Promise.all.call(Ignores, iterable); }); \
             attempt('race', function () { Promise.race.call(Ignores, iterable); }); \
             if (pulled !== 0) { throw new Error('iterator touched: ' + pulled); } \
             throw kinds.join(',');",
        ),
        "all=TypeError,race=TypeError"
    );
}

#[test]
fn a_constructor_whose_executor_is_called_twice_throws() {
    // 27.5.1.5.1 step 2 reads `[[AlreadyResolved]]`, not "are both halves still
    // `undefined`": the first call is `executor()` with *no arguments*, so under
    // the older reading of the same step a second call would be accepted and
    // record over the first. `Promise/{all,race}/capability-executor-called-twice.js`
    // pins this for `executor()`, `executor(undefined, undefined)` and the
    // half-filled cases; `marks` is that test's `checkPoint`.
    //
    // Only *that* it throws is asserted here. The value comes from
    // `v12_native::error_object`, which carries `name`/`message` in the internal
    // slot layout without linking `%TypeError.prototype%`, so neither
    // `instanceof TypeError` nor a readable `name` distinguishes it — a
    // pre-existing gap in that constructor, not this path, and left for the
    // error-object lane.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             var marks = ''; \
             function Twice(executor) { \
               marks += 'a'; \
               executor(); \
               marks += 'b'; \
               executor(function () {}, function () {}); \
               marks += 'c'; \
             } \
             Twice.resolve = function () {}; \
             try { \
               Promise.all.call(Twice, []); \
               globalThis.__out = marks + '/none'; \
             } catch (e) { \
               globalThis.__out = marks + '/threw'; \
             }",
            "globalThis.__out"
        ),
        "ab/threw"
    );
}

#[test]
fn the_receiver_resolve_is_read_once_for_an_empty_iterable() {
    // Spec 27.5.4.1 step 3: `promiseResolve = ? Get(C, "resolve")` happens once,
    // before `GetIterator` — so even an empty iterable pays for the read, while
    // `callCount` (how often the function itself ran) stays at zero because
    // there are no elements to resolve. The `get=1` half is what separates this
    // from the per-element read this used to do, and it is what
    // `Promise/all/invoke-resolve-get-once-no-calls.js` asserts.
    assert_eq!(
        display(
            "var getCount = 0; \
             var callCount = 0; \
             function C(executor) { return new Promise(function (res, rej) { executor(res, rej); }); } \
             Object.defineProperty(C, 'resolve', { \
               get: function () { getCount++; \
                 return function (v) { callCount++; return Promise.resolve(v); }; } }); \
             Promise.all.call(C, []); \
             throw 'get=' + getCount + ' call=' + callCount;",
        ),
        "get=1 call=0"
    );
}

#[test]
fn the_receiver_resolve_is_read_once_for_a_multi_element_iterable() {
    // The same read over three elements: one `Get`, three `Call`s (27.5.4.1
    // step 11-a, once per element), so a per-element `Get` would report 3.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             var getCount = 0; \
             var callCount = 0; \
             function C(executor) { return new Promise(function (res, rej) { executor(res, rej); }); } \
             Object.defineProperty(C, 'resolve', { \
               get: function () { getCount++; \
                 return function (v) { callCount++; return Promise.resolve(v); }; } }); \
             Promise.all.call(C, [1, 2, 3]).then( \
               function (v) { globalThis.__out = v.join('-') + ' get=' + getCount + ' call=' + callCount; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "1-2-3 get=1 call=3"
    );
}

#[test]
fn the_combinator_returns_the_object_the_receiver_constructed() {
    // 27.5.4.1 step 13 returns `promiseCapability.[[Promise]]` — step 6's
    // `Construct(C, « executor »)`, not a promise the engine allocated — so a
    // subclass's own instance is what script holds. That is what
    // `Promise/all/ctx-ctor.js` asserts. `var`, not `let`: see
    // `combinators_accept_constructor_receivers` for why a derived `super(…)`
    // cannot be constructed from an env-bearing program yet.
    assert_eq!(
        display(
            "var built = 0; \
             class Sub extends Promise { constructor(executor) { super(executor); built++; } } \
             var instance = Promise.all.call(Sub, []); \
             throw [built, instance.constructor === Sub, instance instanceof Sub].join(',');",
        ),
        "1,true,true"
    );
}
