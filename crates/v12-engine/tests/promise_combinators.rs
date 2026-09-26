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
    // `PerformPromiseAll` step 6 calls, and each fulfills with the results
    // array (or the race winner), so this also pins that the gate did not
    // start rejecting ordinary receivers.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             class Sub extends Promise {} \
             function Fn() {} \
             Fn.resolve = function (v) { return Promise.resolve(v); }; \
             class Cls {} \
             Cls.resolve = Fn.resolve; \
             function one() { return { [Symbol.iterator]: function () { let n = 0; return { \
               next: function () { n++; return n > 1 ? { done: true } : { done: false, value: 'v' }; }, \
               return: function () { return {}; } }; } }; } \
             let seen = []; \
             let collect = function (p, tag) { p.then( \
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
    // `C.resolve` is the first element step (spec 27.2.4.1 step 6-g), so an
    // abrupt from it closes too and the aggregate rejects with it.
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; globalThis.__closed = 0; \
             function C() {} \
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
