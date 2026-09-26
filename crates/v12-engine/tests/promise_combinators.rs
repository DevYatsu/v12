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
fn all_rejects_when_a_throwing_next_is_iterated() {
    assert_eq!(
        display(
            "let closed = 0; \
             let iterable = { [Symbol.iterator]: function () { return { \
               next: function () { throw new Error('next'); }, \
               return: function () { closed++; return {}; } \
             }; } }; \
             let caught = 'none'; \
             try { Promise.all(iterable); } catch (e) { caught = e.message; } \
             throw caught + '/' + closed;"
        ),
        "next/1"
    );
}

#[test]
fn combinators_reject_non_iterable_and_bad_iterators() {
    assert_eq!(
        display(
            "let out = ''; \
             try { Promise.all(undefined); } catch (e) { out += 'u'; } \
             try { Promise.all({}); } catch (e) { out += e instanceof TypeError ? 'n' : 'x'; } \
             try { Promise.race(1); } catch (e) { out += 'r'; } \
             try { Promise.all({ [Symbol.iterator]: 1 }); } catch (e) { out += 'i'; } \
             try { Promise.all({ [Symbol.iterator]: function () { return 1; } }); } catch (e) { out += 'o'; } \
             try { Promise.all({ [Symbol.iterator]: function () { \
               return { next: function () { return 1; } }; } }); } catch (e) { out += 'n2'; } \
             throw out;" // Six TypeErrors, in the order the probes append their labels
                         // (`n2` is two characters, so the string is seven long).
        ),
        "unrion2"
    );
}

#[test]
fn a_throwing_done_or_value_getter_closes_the_iterator() {
    assert_eq!(
        display(
            "let out = ''; \
             let closed = 0; \
             let doneThrow = { [Symbol.iterator]: function () { return { \
               next: function () { return Object.defineProperty({}, 'done', { get: function () { throw new Error('done'); } }); }, \
               return: function () { closed++; return {}; } \
             }; } }; \
             try { Promise.all(doneThrow); } catch (e) { out += e.message + '/' + closed; } \
             let closed2 = 0; \
             let valueThrow = { [Symbol.iterator]: function () { return { \
               next: function () { return { get done() { return false; }, get value() { throw new Error('value'); } }; }, \
               return: function () { closed2++; return {}; } \
             }; } }; \
             try { Promise.race(valueThrow); } catch (e) { out += '|' + e.message + '/' + closed2; } \
             throw out;"
        ),
        "done/1|value/1"
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
fn a_throwing_return_does_not_mask_the_original_error() {
    assert_eq!(
        display(
            "let iterable = { [Symbol.iterator]: function () { return { \
               next: function () { throw new Error('original'); }, \
               return: function () { throw new Error('from return'); } \
             }; } }; \
             let caught = 'none'; \
             try { Promise.all(iterable); } catch (e) { caught = e.message; } \
             throw caught;"
        ),
        "original"
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
