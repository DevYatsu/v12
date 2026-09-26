//! `Promise.allSettled` (27.5.4.2.1), `Promise.withResolvers` (27.5.4.9) and
//! `Promise.try` (27.5.4.8) — the three statics added on top of the
//! `NewPromiseCapability` seam the `all`/`race` combinators run on.
//!
//! All three need the machine (`GetIterator`, `Construct`, `Call` of a
//! user callback), so they live on the interpreter side of the seam and the
//! engine only owns the data they mutate: the combinator state object and the
//! results array for `allSettled`.

use v12_engine::Engine;

/// Evaluates `source` and returns the thrown value's display string.
fn display(source: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(source).expect_err("source must throw");
    engine.to_display_string(value)
}

/// Evaluates `source` and returns the display string of its value.
fn evaluate(source: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(source).expect("eval");
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

// ---------------------------------------------------------------------------
// allSettled
// ---------------------------------------------------------------------------

/// The fulfilled half: `{ status: "fulfilled", value: x }`, with `value` first
/// visible after `status` (own-property order is the spec's `CreateDataProperty`
/// order, and `Object.keys` reports it).
#[test]
fn all_settled_reports_a_fulfilled_element() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.allSettled([Promise.resolve(1)]).then( \
               function (r) { \
                 globalThis.__out = r.length + '/' + r[0].status + '/' + r[0].value \
                   + '/' + Object.keys(r[0]).join(','); }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "1/fulfilled/1/status,value"
    );
}

/// The rejected half: `{ status: "rejected", reason: r }`.
#[test]
fn all_settled_reports_a_rejected_element_with_its_reason() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let error = { why: 'nope' }; \
             Promise.allSettled([Promise.reject(error)]).then( \
               function (r) { \
                 globalThis.__out = r[0].status + '/' + (r[0].reason === error) \
                   + '/' + Object.keys(r[0]).join(','); }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "rejected/true/status,reason"
    );
}

/// `CreateDataPropertyOrThrow` with `DEFAULT` attributes, so both properties
/// are writable, enumerable and configurable — `allSettled/result-array.js`
/// checks the same descriptors.
#[test]
fn all_settled_status_properties_are_writable_enumerable_configurable() {
    let attrs = settled(
        "globalThis.__out = 'unset'; \
         Promise.allSettled([Promise.resolve(1)]).then(function (r) { \
           let s = Object.getOwnPropertyDescriptor(r[0], 'status'); \
           let v = Object.getOwnPropertyDescriptor(r[0], 'value'); \
           globalThis.__out = [s.writable, s.enumerable, s.configurable] \
             .join(',') + '|' + [v.writable, v.enumerable, v.configurable].join(','); \
         });",
        "globalThis.__out",
    );
    assert_eq!(attrs, "true,true,true|true,true,true");
}

/// The aggregate never rejects because of a rejected element — that is the
/// whole point of the operation.
#[test]
fn all_settled_does_not_reject_on_a_rejected_element() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.allSettled([Promise.reject(1), Promise.resolve(2)]).then( \
               function (r) { globalThis.__out = 'fulfilled:' + r.length; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "fulfilled:2"
    );
}

/// Empty iterable: the exhaustion token drops the count to zero and the
/// aggregate fulfills with the empty values array.
#[test]
fn all_settled_of_an_empty_iterable_fulfills_with_an_empty_array() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.allSettled([]).then( \
               function (r) { globalThis.__out = Array.isArray(r) + '/' + r.length; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "true/0"
    );
}

/// Results are written at the element's own index, so they come back in input
/// order however the elements settle.
#[test]
fn all_settled_results_follow_input_order_not_settlement_order() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let a = { n: 'a' }, b = { n: 'b' }; \
             let pa = new Promise(function (res, rej) { res(a); rej(a); }); \
             let pb = new Promise(function (res, rej) { rej(b); res(b); }); \
             Promise.allSettled([pa, pb]).then(function (r) { \
               globalThis.__out = r.map(function (s) { \
                 return s.status + ':' + (s.value || s.reason).n; }).join(','); \
             });",
            "globalThis.__out"
        ),
        "fulfilled:a,rejected:b"
    );
}

/// A throwing `return` on `IteratorClose` does **not** become the rejection
/// reason: `IteratorClose(iteratorRecord, completion)` returns the original
/// `throw` completion when there is one (the spec's `Promise.allSettled` step
/// 6-b), so the element-processing `TypeError` is what the aggregate rejects
/// with — and the `return` still ran.
#[test]
fn all_settled_rejects_with_the_element_error_not_the_close_error() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             globalThis.__closed = false; \
             class Weird extends Promise { static resolve() { return { then: 1 }; } } \
             let iterable = { [Symbol.iterator]: function () { return { \
               next: function () { return { value: 1, done: false }; }, \
               return: function () { globalThis.__closed = true; \
                 throw new Test262Error('close'); } }; } }; \
             Weird.allSettled(iterable).then( \
               function (r) { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "rejected:TypeError: then is not a function"
    );
}

/// 27.5.4.2.1 has no `IsCallable(nextPromise.then)` branch: it goes straight
/// from `Call(promiseResolve, …)` to `Invoke(nextPromise, "then", « onFulfilled,
/// onRejected »)`, so a non-callable `then` is the `Invoke`'s `TypeError` and
/// rejects the aggregate. (Contrast `KIND_ALL`, which settles such an element
/// in place.) The element's own `then` is never what is read — only
/// `nextPromise`'s — so a receiver whose `resolve` hands back a `then`-less
/// object is what reaches it.
#[test]
fn all_settled_rejects_when_the_resolved_promise_has_a_non_callable_then() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             class Weird extends Promise { \
               static resolve() { return { then: 1 }; } \
             } \
             Weird.allSettled([1]).then( \
               function (r) { globalThis.__out = 'fulfilled:' + r.length; }, \
               function (e) { globalThis.__out = 'rejected'; });",
            "globalThis.__out"
        ),
        "rejected"
    );
}

/// A plain element with a non-callable `then` is not that case: it goes through
/// `promiseResolve`, which builds a real promise for it, and that promise's
/// `then` is `Promise.prototype.then`.
#[test]
fn all_settled_treats_a_then_less_element_as_fulfilled() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.allSettled([{ then: 1 }]).then( \
               function (r) { globalThis.__out = r.length + '/' + r[0].status; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "1/fulfilled"
    );
}

/// A non-promise element fulfills with its own value.
#[test]
fn all_settled_treats_a_plain_value_as_fulfilled() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.allSettled([1, 'two']).then(function (r) { \
               globalThis.__out = r.map(function (s) { \
                 return s.status + '=' + s.value; }).join(','); });",
            "globalThis.__out"
        ),
        "fulfilled=1,fulfilled=two"
    );
}

/// The spec's `alreadyCalled` record is shared by the pair, so a `then` free to
/// call both halves records only the first outcome and the aggregate never sees
/// a double decrement.
#[test]
fn all_settled_ignores_the_second_call_of_a_misbehaving_then() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let element = { then: function (res, rej) { res('first'); rej('second'); } }; \
             Promise.allSettled([element]).then(function (r) { \
               globalThis.__out = r.length + '/' + r[0].status + '/' + r[0].value; });",
            "globalThis.__out"
        ),
        "1/fulfilled/first"
    );
}

/// A subclassed receiver gets its own promise, from `NewPromiseCapability(ctor)`.
#[test]
fn all_settled_produces_instances_of_the_receiver() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             class SubPromise extends Promise {} \
             Promise.allSettled.call(SubPromise, [1]).then(function (r) { \
               globalThis.__out = r[0].status; }, function () { globalThis.__out = 'rejected'; });",
            "globalThis.__out"
        ),
        "fulfilled"
    );
}

// ---------------------------------------------------------------------------
// any
// ---------------------------------------------------------------------------

/// The fulfilled half of 27.5.4.3.1 is `promiseCapability.[[Resolve]]`
/// *verbatim*, so the first element to fulfil settles the aggregate — and a
/// rejection arriving later is the rejected half's own business.
#[test]
fn any_resolves_with_the_first_fulfilled_element() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.any([Promise.reject('a'), Promise.resolve('b'), \
                          Promise.reject('c')]).then( \
               function (v) { globalThis.__out = 'fulfilled:' + v; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "fulfilled:b"
    );
}

/// A rejection that lands *after* the aggregate is already settled changes
/// nothing: `[[Resolve]]` and `[[Reject]]` are first-wins, so the second
/// element's `rejectElement` can only write to a reason list nobody reads.
#[test]
fn any_ignores_a_rejection_arriving_after_a_fulfilment() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let seen = []; \
             Promise.any([Promise.resolve('win'), \
                           new Promise(function (res, rej) { rej('late'); })]) \
               .then( \
                 function (v) { globalThis.__out = 'fulfilled:' + v; }, \
                 function (e) { globalThis.__out = 'rejected'; }); \
             Promise.resolve().then(function () {}).then(function () {}).then( \
               function () { globalThis.__out += '/still:' + seen.length; });",
            "globalThis.__out"
        ),
        "fulfilled:win/still:0"
    );
}

/// Every rejected element's reason is recorded at the element's own index, so
/// the `AggregateError`'s `errors` array comes back in input order
/// (`Promise.any/reject-all-mixed.js` asserts the same `join`).
#[test]
fn any_rejects_with_an_aggregate_error_holding_every_reason_in_input_order() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.any([Promise.reject('a'), Promise.reject('b'), \
                          Promise.reject('c')]).then( \
               function (v) { globalThis.__out = 'fulfilled:' + v; }, \
               function (e) { globalThis.__out = [e instanceof AggregateError, \
                 e instanceof Error, e.errors.length, e.errors.join('')] \
                 .join('/'); });",
            "globalThis.__out"
        ),
        "true/true/3/abc"
    );
}

/// 27.5.4.3.1 step 6: an empty iterable rejects with `AggregateError(« »)` —
/// the errors list is built empty, *not* from a first rejection.
#[test]
fn any_of_an_empty_iterable_rejects_with_an_aggregate_error_with_no_errors() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.any([]).then( \
               function (v) { globalThis.__out = 'fulfilled:' + v; }, \
               function (e) { globalThis.__out = [e instanceof AggregateError, \
                 Array.isArray(e.errors), e.errors.length, e.message] \
                 .join('/'); });",
            "globalThis.__out"
        ),
        "true/true/0/"
    );
}

/// A non-iterable argument is an abrupt `GetIterator`, so the capability
/// rejects with the `TypeError` rather than the call throwing — the same
/// `IfAbruptRejectPromise` the other combinators route through.
#[test]
fn any_rejects_rather_than_throws_for_a_non_iterable_argument() {
    for source in [
        "Promise.any(42)",
        "Promise.any(undefined)",
        "Promise.any(null)",
        "Promise.any(true)",
        "Promise.any(Symbol())",
        "Promise.any()",
    ] {
        let out = settled(
            &format!(
                "globalThis.__out = 'unset'; {source}.then( \
                   function () {{ globalThis.__out = 'fulfilled'; }}, \
                   function (e) {{ globalThis.__out = 'rejected:' + e; }});"
            ),
            "globalThis.__out",
        );
        assert!(
            out.starts_with("rejected:TypeError"),
            "{source} must reject with a TypeError, got {out}"
        );
    }
}

/// The `errors` array is snapshotted at the moment the count reaches zero: a
/// `then` free to call its reject function a second time after the aggregate
/// has already rejected cannot extend it (its `alreadyCalled` record is set,
/// and the count is already spent).
#[test]
fn any_does_not_extend_the_errors_array_with_a_late_rejection() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let element = { then: function (res, rej) { \
               rej('first'); \
               Promise.resolve().then(function () { rej('second'); }); } }; \
             Promise.any([element]).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { Promise.resolve().then(function () { \
                 globalThis.__out = e.errors.join(',') + '/' + e.errors.length; }); });",
            "globalThis.__out"
        ),
        "first/1"
    );
}

/// A reason is stored as-is, never adopted: a rejected promise used as a
/// rejection reason stays that promise in the `errors` array rather than being
/// unwrapped to its own fulfilment value.
#[test]
fn any_stores_a_promise_rejection_reason_without_adopting_it() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let reason = Promise.resolve('inner'); \
             Promise.any([Promise.reject(reason)]).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = (e.errors[0] === reason) + '/' \
                 + (e.errors[0] instanceof Promise) + '/' \
                 + typeof e.errors[0].then; });",
            "globalThis.__out"
        ),
        "true/true/function"
    );
}

/// `CreateBuiltinFunction`: `length: 1`, `name: "any"`, and the
/// `@@toStringTag` every `%Promise%`-family static inherits.
#[test]
fn any_static_shape_is_length_one_named_any_and_to_string_tagged() {
    assert_eq!(
        evaluate(
            "[Promise.any.length, Promise.any.name, \
              Promise[Symbol.toStringTag], \
              typeof Promise.any].join(',')"
        ),
        "1,any,Promise,function"
    );
}

/// Nothing is special-cased to arrays: `GetIterator` accepts any iterable, so a
/// generator, a `Set` and a hand-rolled `@@iterator` all work — and each reaches
/// one of the two terminal steps.
///
/// The leading `anyR0` is 27.5.4.3.1 step 5-6 and not an ordering accident: a
/// first `IteratorStepValue` that is already done short-circuits the whole
/// algorithm, so that aggregate's `[[Reject]]` runs *synchronously* and its
/// reaction is queued before any of the three below settle.
#[test]
fn any_accepts_a_generator_a_set_and_a_generic_iterable() {
    assert_eq!(
        settled(
            "globalThis.__out = ''; \
             let empty = { [Symbol.iterator]: function () { \
               return { next: function () { return { value: 'never', done: true }; } }; } }; \
             Promise.any(empty).then( \
               function (v) { globalThis.__out += '/any:' + v; }, \
               function (e) { globalThis.__out += '/anyR0:' + e.errors.length; }); \
             function* gen() { yield Promise.reject('g1'); yield 'g2'; } \
             Promise.any(gen()).then( \
               function (v) { globalThis.__out += '/gen:' + v; }, \
               function (e) { globalThis.__out += '/genR:' + e.errors.join(''); }); \
             Promise.any(new Set([Promise.reject('s')])).then( \
               function (v) { globalThis.__out += '/set:' + v; }, \
               function (e) { globalThis.__out += '/setR:' + e.errors.join(''); }); \
             let generic = { [Symbol.iterator]: function () { \
               let n = 0; \
               return { next: function () { \
                 return n++ === 0 ? { value: Promise.reject('x'), done: false } \
                                  : { value: undefined, done: true }; } }; } }; \
             Promise.any(generic).then( \
               function (v) { globalThis.__out += '/generic:' + v; }, \
               function (e) { globalThis.__out += '/genericR:' + e.errors.join(''); });",
            "globalThis.__out"
        ),
        "/anyR0:0/gen:g2/setR:s/genericR:x"
    );
}

// ---------------------------------------------------------------------------
// withResolvers
// ---------------------------------------------------------------------------

/// 27.5.4.9's three `CreateDataPropertyOrThrow` calls use `DEFAULT`
/// attributes, and `withResolvers/result.js` checks exactly that.
#[test]
fn with_resolvers_properties_have_default_attributes() {
    let mut engine = Engine::new();
    let out = engine
        .eval(
            "let r = Promise.withResolvers(); \
             ['promise', 'resolve', 'reject'].map(function (name) { \
               let d = Object.getOwnPropertyDescriptor(r, name); \
               return [d.writable, d.enumerable, d.configurable].join(','); \
             }).join('|');",
        )
        .expect("eval");
    assert_eq!(
        engine.to_display_string(out),
        "true,true,true|true,true,true|true,true,true"
    );
}

#[test]
fn with_resolvers_promise_property_is_a_promise_of_the_receiver() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             class SubPromise extends Promise {} \
             let r = Promise.withResolvers.call(SubPromise); \
             globalThis.__ok = r.promise.constructor === SubPromise \
               && r.promise instanceof SubPromise; \
             r.resolve(7); \
             r.promise.then(function (v) { globalThis.__out = globalThis.__ok + '/' + v; });",
            "globalThis.__out"
        ),
        "true/7"
    );
}

/// `CreateBuiltinFunction`'s pair: `length: 1`, `name: ""`.
#[test]
fn with_resolvers_resolvers_are_arity_one_nameless_functions() {
    assert_eq!(
        evaluate(
            "let r = Promise.withResolvers(); \
             [r.resolve.length, r.resolve.name, r.reject.length, r.reject.name] \
               .join(',');"
        ),
        "1,,1,"
    );
}

#[test]
fn with_resolvers_reject_settles_the_promise() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let r = Promise.withResolvers(); \
             r.reject('because'); \
             r.promise.then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "rejected:because"
    );
}

#[test]
fn with_resolvers_throws_for_a_non_constructor_receiver() {
    assert!(
        display("Promise.withResolvers.call(eval);").starts_with("TypeError"),
        "expected a TypeError, got: {}",
        display("Promise.withResolvers.call(eval);")
    );
}

#[test]
fn with_resolvers_throws_for_a_non_object_receiver() {
    assert!(
        display("Promise.withResolvers.call(undefined);").starts_with("TypeError"),
        "expected a TypeError"
    );
}

// ---------------------------------------------------------------------------
// try
// ---------------------------------------------------------------------------

#[test]
fn try_fulfills_with_the_callbacks_return_value() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let sentinel = { sentinel: true }; \
             Promise.try(function () { return sentinel; }).then(function (v) { \
               globalThis.__out = (v === sentinel) + '/' + v.sentinel; });",
            "globalThis.__out"
        ),
        "true/true"
    );
}

#[test]
fn try_rejects_with_the_thrown_value() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let error = { why: 'nope' }; \
             Promise.try(function () { throw error; }).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = (e === error) + '/' + e.why; });",
            "globalThis.__out"
        ),
        "true/nope"
    );
}

#[test]
fn try_forwards_every_argument_after_the_callback() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.try(function (a, b, c) { \
               globalThis.__out = [a, b, c].join(','); }, 1, 'two', 3).then(function () {});",
            "globalThis.__out"
        ),
        "1,two,3"
    );
}

/// 27.5.4.8 step 5 is `PromiseResolve(ctor, result)`, so a promise the callback
/// returns is adopted by identity rather than wrapped.
#[test]
fn try_does_not_wrap_a_promise_the_callback_returns() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             let sentinel = Promise.resolve('inner'); \
             let returned = Promise.try(function () { return sentinel; }); \
             globalThis.__out = (returned === sentinel) + '/' + (returned instanceof Promise);",
            "globalThis.__out"
        ),
        "true/true"
    );
}

/// The capability comes from the receiver, so a subclass instance comes back —
/// for a non-promise return value.
#[test]
fn try_produces_instances_of_the_receiver() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             class SubPromise extends Promise {} \
             let p = Promise.try.call(SubPromise, function () { return 1; }); \
             globalThis.__out = (p.constructor === SubPromise) + '/' \
               + (p instanceof SubPromise); \
             p.then(function (v) { globalThis.__out += '/' + v; });",
            "globalThis.__out"
        ),
        "true/true/1"
    );
}

/// 27.5.4.8 step 1: `If ctor is not an Object, throw a TypeError exception` —
/// ahead of the callback call.
#[test]
fn try_throws_for_a_non_object_receiver() {
    for source in [
        "Promise.try.call(undefined, function () {});",
        "Promise.try.call(null, function () {});",
        "Promise.try.call(86, function () {});",
        "Promise.try.call('string', function () {});",
    ] {
        assert!(
            display(source).starts_with("TypeError"),
            "{source} must throw a TypeError"
        );
    }
}

/// A non-constructor object receiver is not rejected either: step 1 only asks
/// for an Object, and `NewPromiseCapability`'s own `IsConstructor` gate throws.
#[test]
fn try_throws_for_a_non_constructor_receiver() {
    assert!(
        display("Promise.try.call(eval, function () {});").starts_with("TypeError"),
        "expected a TypeError"
    );
}

/// `Promise.try` is a plain static, not a constructor: `new Promise.try(…)`
/// throws and `isConstructor` is false.
#[test]
fn try_is_not_a_constructor() {
    assert!(
        display("new Promise.try(function () {});").starts_with("TypeError"),
        "expected `new Promise.try` to throw"
    );
}

/// A non-callable callback: `Call` throws and the abrupt branch builds the
/// capability, so the *promise* rejects with the `TypeError` rather than the
/// call throwing.
#[test]
fn try_rejects_when_the_callback_is_not_callable() {
    assert_eq!(
        settled(
            "globalThis.__out = 'unset'; \
             Promise.try(undefined).then( \
               function () { globalThis.__out = 'fulfilled'; }, \
               function (e) { globalThis.__out = 'rejected:' + e; });",
            "globalThis.__out"
        ),
        "rejected:TypeError: callee is not a function"
    );
}
