//! The `Iterator` global (spec 27.1) and `%Iterator.prototype%` (spec 27.1.4).
//!
//! Regression: neither existed, so 410 test262 files raised
//! `ReferenceError: Iterator is not defined` and 125 more called a helper that
//! was not there. The surface is what this file pins — the global, the
//! prototype's eleven methods, their `length`/`name`/descriptors, the brand
//! check, and the two helpers whose body is pure heap (`take`/`drop`/`toArray`).
//!
//! Known boundary, asserted nowhere here: the eight *callback-taking* helpers
//! (`map`/`filter`/`flatMap`/`reduce`/`forEach`/`some`/`every`/`find`) are
//! implemented on the interpreter seam
//! (`Interp::run_iterator_callback`, `crates/v12-interp/src/call_setup.rs`),
//! which drains eagerly and returns an array from `map`/`filter`/`flatMap`.
//! So `it.map(f).toArray()` is a `TypeError` today; only their *surface* is
//! covered here.

use v12_engine::Engine;

/// Evaluates `source` and returns the display string of the value or of the
/// thrown value. Every snippet ends in `throw` so the assertion channel is a
/// plain string comparison.
fn thrown(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

/// The eleven spec helpers, in `Iterator.prototype`'s own-key order.
const HELPERS: [&str; 11] = [
    "map", "filter", "take", "drop", "flatMap", "reduce", "toArray", "forEach", "some", "every",
    "find",
];

#[test]
fn iterator_is_a_global_with_a_prototype_chained_to_object_prototype() {
    // `Object.getPrototypeOf(Iterator) === Function.prototype` is *not*
    // asserted: in v12 a constructor's `prototype` field is both the instance
    // prototype (`new Iterator()`) and the chain link, so the two cannot both
    // hold. Every other built-in constructor has the same limitation.
    assert_eq!(
        thrown(
            "throw [typeof Iterator, typeof Iterator.prototype, \
                 Object.getPrototypeOf(Iterator.prototype) === Object.prototype].join(',');"
        ),
        "function,object,true"
    );
}

#[test]
fn every_helper_is_installed_on_the_prototype() {
    let probe = format!(
        "throw [{}].join(',');",
        HELPERS
            .iter()
            .map(|n| format!("typeof Iterator.prototype.{n}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(
        thrown(&probe),
        "function,function,function,function,function,function,function,\
         function,function,function,function"
    );
}

#[test]
fn helper_length_and_name_are_spec_values() {
    // `toArray` is the only arity-0 helper; `reduce`'s `initialValue` is
    // optional, so it does not count toward `length`.
    assert_eq!(
        thrown(
            "throw [Iterator.prototype.map.length, Iterator.prototype.take.length, \
                 Iterator.prototype.reduce.length, Iterator.prototype.toArray.length, \
                 Iterator.prototype.filter.name, Iterator.prototype.flatMap.name, \
                 Iterator.prototype.find.name].join(',');"
        ),
        "1,1,1,0,filter,flatMap,find"
    );
}

#[test]
fn the_constructor_stamps_its_own_length_and_name() {
    assert_eq!(
        thrown(
            "throw [Iterator.length, Iterator.name, \
                 Object.getOwnPropertyDescriptor(Iterator, 'prototype').writable, \
                 Object.getOwnPropertyDescriptor(Iterator, 'prototype').configurable, \
                 Object.getOwnPropertyDescriptor(Iterator, 'prototype').enumerable].join(',');"
        ),
        "0,Iterator,false,false,false"
    );
}

#[test]
fn a_plain_call_throws_but_new_produces_an_object() {
    // Spec 27.1.2.1 step 1: `Iterator()` (no `new`) is a TypeError; `new
    // Iterator()` returns a fresh object inheriting `Iterator.prototype`.
    assert_eq!(
        thrown("try { Iterator(); throw 'no-throw'; } catch (e) { throw 'plain-call-threw'; }"),
        "plain-call-threw"
    );
    assert_eq!(
        thrown(
            "var it = new Iterator(); \
                 throw [typeof it, Object.getPrototypeOf(it) === Iterator.prototype].join(',');"
        ),
        "object,true"
    );
}

#[test]
fn the_prototype_carries_a_to_string_tag_and_the_iterator_protocol() {
    // Two symbol-keyed own properties: `@@toStringTag` (spec 27.1.4) and
    // `@@iterator` (spec 27.1.4, whose body is `return this`).
    assert_eq!(
        thrown(
            "throw [Iterator.prototype[Symbol.toStringTag], \
                 Object.getOwnPropertySymbols(Iterator.prototype).length].join(',');"
        ),
        "Iterator,2"
    );
    assert_eq!(
        thrown("var self = Iterator.prototype; throw self[Symbol.iterator]() === self;"),
        "true"
    );
}

#[test]
fn to_array_drains_the_source() {
    assert_eq!(
        thrown("throw [1,2,3].values().toArray().join(',');"),
        "1,2,3"
    );
}

#[test]
fn take_and_drop_bound_the_iteration() {
    assert_eq!(
        thrown(
            "throw [[1,2,3,4,5].values().take(2).toArray().join(','), \
                   [1,2,3,4,5].values().drop(3).toArray().join(',')].join('|');"
        ),
        "1,2|4,5"
    );
}

#[test]
fn take_does_not_over_pull_its_source() {
    // Laziness, observed through the source: an eager `take` would drain the
    // iterator and leave nothing behind. `take` pulls exactly `limit` values,
    // so the rest of the source is still there afterwards.
    assert_eq!(
        thrown(
            "var it = [1,2,3,4].values(); \
                 var taken = it.take(2).toArray().join(','); \
                 throw [taken, it.toArray().join(',')].join('|');"
        ),
        "1,2|3,4"
    );
}

#[test]
fn drop_does_not_pull_until_it_is_consumed() {
    // The mirror image: an eager `drop` would snapshot the tail at call time,
    // so a value appended in between would be lost.
    assert_eq!(
        thrown(
            "var arr = [1,2,3]; \
                 var rest = arr.values().drop(2); \
                 arr.push(4); \
                 throw rest.toArray().join(',');"
        ),
        "3,4"
    );
}

#[test]
fn take_zero_consumes_nothing() {
    assert_eq!(
        thrown(
            "var it = [1,2].values(); \
                 var none = it.take(0).toArray(); \
                 throw [none.length, it.toArray().join(',')].join(',');"
        ),
        "0,1,2"
    );
}

#[test]
fn helpers_compose_in_both_orders() {
    // Built through locals rather than a nested array literal: `throw [f(x),
    // g(y)].join(s)` with two method chains trips a v12 parser bug ("Expected
    // a semicolon or an implicit semicolon"), unrelated to this surface.
    assert_eq!(
        thrown(
            "var a = [1,2,3,4,5].values().take(3).drop(1).toArray().join(','); \
                var b = [1,2,3,4,5].values().drop(4).take(1).toArray().join(','); \
                throw a + '|' + b;"
        ),
        "2,3|5"
    );
}

#[test]
fn a_completed_helper_stays_completed() {
    // Spec 27.1.4.6: once `next` has reported `done`, every later `next` on
    // the same helper reports `done` again without touching the source.
    assert_eq!(
        thrown(
            "var t = [1,2,3].values().take(1); \
                 var a = t.next(), b = t.next(), c = t.next(); \
                 throw [a.value, a.done, b.done, c.done, t.toArray().length].join(',');"
        ),
        "1,false,true,true,0"
    );
}

#[test]
fn helpers_are_brand_checked() {
    for helper in HELPERS {
        let src = format!(
            "try {{ Iterator.prototype.{helper}.call({{}}, function(){{}}); throw 'no-throw'; }} \
             catch (e) {{ throw e instanceof TypeError ? 'TypeError' : 'other'; }}"
        );
        assert_eq!(thrown(&src), "TypeError", "{helper} must brand-check");
    }
}

#[test]
fn helpers_reject_a_non_object_receiver() {
    assert_eq!(
        thrown(
            "try { Iterator.prototype.take.call(null, 1); throw 'no-throw'; } \
               catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"
        ),
        "TypeError"
    );
}

#[test]
fn take_clamps_a_non_positive_or_non_finite_limit_to_zero() {
    // Corrected against spec 27.1.4.3 steps 6-9 (quoted in test262's
    // `take/limit-rangeerror.js`): a `NaN` limit, a finite limit above
    // `2**53 - 1`, and a negative truncated limit each throw a `RangeError`.
    // They used to be clamped to "no values", which is why this test had to
    // change — the old comment cited a step that never said that. `0` and a
    // fractional limit are still clamped, and `2.9` truncates down to 2.
    assert_eq!(
        thrown(
            "throw [[1,2,3].values().take(0).toArray().length, \
                   [1,2,3].values().take(2.9).toArray().join(',')].join(',');"
        ),
        "0,1,2"
    );
    // `thrown` reports "<Class>: <message>", so the class prefix is what the
    // assertion below is really checking — the message text is not part of the
    // spec.
    assert!(thrown("[1,2,3].values().take(-1);").starts_with("RangeError"));
    assert!(thrown("[1,2,3].values().take(NaN);").starts_with("RangeError"));
    assert!(thrown("[1,2,3].values().take();").starts_with("RangeError"));
    assert!(
        thrown("[1,2,3].values().take(Number.MAX_SAFE_INTEGER + 1);").starts_with("RangeError")
    );
    // The two counter-cases the spec deliberately exempts: `-0.5` truncates to
    // `-0` (and `-0 < 0` is false) and `Infinity` is not finite.
    assert_eq!(thrown("[1,2,3].values().take(-0.5).toArray().length;"), "0");
    assert_eq!(
        thrown("[1,2,3].values().take(Infinity).toArray().join(',');"),
        "1,2,3"
    );
}

#[test]
fn iterator_from_passes_an_iterator_through_and_wraps_an_array() {
    assert_eq!(
        thrown(
            "var it = [1,2].values(); \
                 throw [Iterator.from(it) === it, Iterator.from([7,8]).toArray().join(',')].join(',');"
        ),
        "true,7,8"
    );
}
