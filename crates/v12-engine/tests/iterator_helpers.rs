//! The eight ES2025 iterator helpers with a callback (spec 27.1.4): `map`,
//! `filter`, `flatMap` (lazy, return iterators) and `reduce`, `forEach`,
//! `some`, `every`, `find` (consume, return a value).
//!
//! Regression: all eight existed but were **eager** and drained through the
//! registry `next`, so `map`/`filter`/`flatMap` answered with an *array*
//! (`it.map(f).toArray()` died on `Array.prototype.toArray` being undefined)
//! and no helper accepted anything but one of v12's own element-store
//! iterators — a generator or a plain `{next(){}}` was rejected outright.
//! The callbacks also got three arguments and a `thisArgument`, which the spec
//! does not have.
//!
//! Two ways of reaching a helper are exercised below, and the difference
//! matters: a v12 iterator (`[1,2,3].values()`) inherits the helpers from
//! `Iterator.prototype`, while a generator or a plain object does not, so those
//! go through `Iterator.prototype.X.call(source, …)` exactly as test262 does.

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

/// Evaluates `decl` (a source-iterator definition) followed by `body`, kept as
/// two pieces so no snippet needs brace escaping.
fn with(decl: &str, body: &str) -> String {
    let mut source = String::with_capacity(decl.len() + body.len() + 1);
    source.push_str(decl);
    source.push(' ');
    source.push_str(body);
    thrown(&source)
}

/// A three-value plain iterator.
const PLAIN: &str = "function plain() { return { i: 0, next: function() { \
     return this.i < 3 ? { value: this.i++, done: false } : { done: true }; } }; }";

/// A three-value generator.
const GEN: &str = "function* gen() { yield 10; yield 20; yield 30; }";

/// The eight callback helpers.
const CALLBACK_HELPERS: [&str; 8] = [
    "map", "filter", "flatMap", "reduce", "forEach", "some", "every", "find",
];

// ---------------------------------------------------------------------------
// Laziness: the three iterator-returning helpers must consume nothing until
// their result is pulled.
// ---------------------------------------------------------------------------

#[test]
fn map_filter_and_flatmap_pull_nothing_until_consumed() {
    for helper in ["map", "filter", "flatMap"] {
        let body = format!(
            "var pulled = 0; \
             var it = {{ next: function() {{ pulled++; return {{ done: true }}; }} }}; \
             Iterator.prototype.{helper}.call(it, function(v) {{ return v; }}); \
             throw pulled;"
        );
        assert_eq!(thrown(&body), "0", "{helper} must not pull at call time");
    }
}

#[test]
fn map_yields_the_callback_result_and_chains() {
    assert_eq!(
        thrown("throw [1,2,3].values().map(function(v) { return v * 2; }).toArray().join(',');"),
        "2,4,6"
    );
    assert_eq!(
        thrown(
            "throw [1,2].values().map(function(v) { return v; }) \
                 .map(function(v) { return v * 5; }).toArray().join(',');"
        ),
        "5,10"
    );
}

#[test]
fn map_result_is_an_iterator_not_an_array() {
    // The regression itself: the old body returned an Array, so `.toArray()`
    // on the result was `Array.prototype.toArray` — undefined.
    assert_eq!(
        thrown(
            "var m = [1,2].values().map(function(v) { return v; }); \
                 throw typeof m.next + '|' + typeof m.toArray;"
        ),
        "function|function"
    );
}

#[test]
fn filter_yields_the_source_value_not_the_predicate_result() {
    assert_eq!(
        thrown(
            "throw [1,2,3,4].values().filter(function(v) { return v % 2 === 0; }) \
                 .toArray().join(',');"
        ),
        "2,4"
    );
    // A truthy *object* is a truthy predicate result, so `filter` keeps both —
    // but it must yield the values, not the objects the predicate returned.
    assert_eq!(
        thrown(
            "var kept = [{a:1},{a:2}].values() \
                 .filter(function(v) { return {always: true}; }).toArray(); \
                 throw kept.length + '|' + kept[0].a + '|' + kept[1].a;"
        ),
        "2|1|2"
    );
}

#[test]
fn filter_pulls_past_rejected_values_within_one_next() {
    // Spec: the Iterator Helper Closure loops until the predicate accepts, so
    // a single `next()` on a filter may consume several source values.
    assert_eq!(
        thrown(
            "var f = [1,2,3].values().filter(function(v) { return v === 3; }); \
                 var r = f.next(); throw r.value + '|' + r.done;"
        ),
        "3|false"
    )
}

#[test]
fn flatmap_flattens_one_level_of_arrays_only() {
    assert_eq!(
        thrown("throw [1,2].values().flatMap(function(v) { return [v, v]; }).toArray().join(',');"),
        "1,1,2,2"
    );
    assert_eq!(
        thrown("throw [1,2].values().flatMap(function(v) { return v; }).toArray().join(',');"),
        "1,2"
    );
    // Depth 1: a nested array is yielded whole rather than flattened again.
    assert_eq!(
        thrown(
            "var r = [[1,2]].values().flatMap(function(v) { return [v]; }).toArray(); \
                 throw [r.length, Array.isArray(r[0]), r[0].length].join(',');"
        ),
        "1,true,2"
    )
}

// ---------------------------------------------------------------------------
// Callback shape: two arguments and no thisArgument, per spec 27.1.4.5
// step 5.b.iv. The old body passed three arguments and a `thisArgument`.
// ---------------------------------------------------------------------------

#[test]
fn callbacks_get_two_arguments_and_an_undefined_this() {
    assert_eq!(
        with(
            GEN,
            "var argc = -1; var counter = -1; \
             Iterator.prototype.map.call(gen(), function(v, i) { \
               argc = arguments.length; counter = i; return v; }).toArray(); \
             throw argc + '|' + counter;"
        ),
        "2|2"
    );
    assert_eq!(
        with(
            GEN,
            "var argc = -1; \
             Iterator.prototype.reduce.call(gen(), function(m, v, i) { \
               argc = arguments.length; return m; }); \
             throw argc;"
        ),
        "3"
    );
}

#[test]
fn flatmap_yields_one_flattened_element_per_next() {
    // The distinguishing laziness check for `flatMap`: two source values, each
    // mapping to a two-element array, is four yielded values — and the mapper
    // must not run for the second source value until the third `next`.
    assert_eq!(
        thrown(
            "var calls = 0; \
                 var f = [1,2].values().flatMap(function(v) { calls++; return [v, v]; }); \
                 var a = f.next(); var after1 = calls; \
                 var b = f.next(); var after2 = calls; \
                 var c = f.next(); var after3 = calls; \
                 throw [a.value, after1, b.value, after2, c.value, after3].join(',');"
        ),
        "1,1,1,1,2,2"
    )
}

#[test]
fn the_counter_advances_once_per_source_value() {
    assert_eq!(
        with(
            GEN,
            "var seen = []; \
             Iterator.prototype.map.call(gen(), function(v, i) { seen.push(i); return v; }).toArray(); \
             throw seen.join(',');"
        ),
        "0,1,2"
    )
}

// ---------------------------------------------------------------------------
// The five value-returning helpers.
// ---------------------------------------------------------------------------

#[test]
fn reduce_seeds_from_the_first_value_when_no_initial_is_given() {
    // The callback is not called for the seed, so it runs twice for three
    // values.
    assert_eq!(
        with(
            GEN,
            "var calls = 0; \
             var r = Iterator.prototype.reduce.call(gen(), function(m, v) { calls++; return m + v; }); \
             throw r + '|' + calls;"
        ),
        "60|2"
    );
    assert_eq!(
        thrown("throw [1,2,3].values().reduce(function(a, v) { return a + v; }, 10);"),
        "16"
    )
}

#[test]
fn reduce_distinguishes_an_explicit_undefined_seed_from_no_seed() {
    // Spec 27.1.4.9 step 6 tests whether `initialValue` is *present*. With an
    // explicit `undefined` an empty source yields `undefined`; with no
    // argument at all it is a TypeError.
    assert_eq!(
        thrown("throw [].values().reduce(function(a, v) { return v; }, undefined);"),
        "undefined"
    );
    assert_eq!(
        thrown(
            "try { [].values().reduce(function(a, v) { return v; }); throw 'no-throw'; } \
               catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"
        ),
        "TypeError"
    )
}

#[test]
fn for_each_returns_undefined_and_visits_every_value() {
    assert_eq!(
        thrown(
            "var seen = []; var r = [1,2,3].values().forEach(function(v) { seen.push(v); }); \
                 throw r === undefined ? seen.join(',') : 'not-undefined';"
        ),
        "1,2,3"
    )
}

#[test]
fn some_every_and_find_short_circuit() {
    // Each assertion also counts the calls, so a helper that keeps pulling
    // after its answer is caught.
    assert_eq!(
        thrown(
            "var c = 0; var r = [1,2,3,4].values().some(function(v) { c++; return v === 1; }); \
                 throw r + '|' + c;"
        ),
        "true|1"
    );
    assert_eq!(
        thrown(
            "var c = 0; var r = [1,2,3,4].values().every(function(v) { c++; return v !== 2; }); \
                 throw r + '|' + c;"
        ),
        "false|2"
    );
    assert_eq!(
        thrown(
            "var c = 0; var r = [1,2,3,4].values().find(function(v) { c++; return v === 2; }); \
                 throw r + '|' + c;"
        ),
        "2|2"
    )
}

#[test]
fn an_empty_source_gives_the_spec_neutral_value() {
    assert_eq!(
        thrown(
            "throw [].values().every(function() { return false; }) + '|' \
                 + [].values().find(function() { return true; }) + '|' \
                 + [].values().some(function() { return true; });"
        ),
        "true|undefined|false"
    )
}

// ---------------------------------------------------------------------------
// Sources: the helpers accept any object with a callable `next` (spec 27.1.4
// steps 1-3), not only v12's own element-store iterators.
// ---------------------------------------------------------------------------

#[test]
fn the_helpers_accept_a_generator_source() {
    assert_eq!(
        with(
            GEN,
            "throw Iterator.prototype.map.call(gen(), function(v) { return v + 1; }) \
               .toArray().join(',');"
        ),
        "11,21,31"
    );
    assert_eq!(
        with(
            GEN,
            "throw Iterator.prototype.toArray.call(gen()).join(',');"
        ),
        "10,20,30"
    );
    assert_eq!(
        with(
            GEN,
            "throw Iterator.prototype.reduce.call(gen(), function(a, v) { return a + v; });"
        ),
        "60"
    );
    assert_eq!(
        with(
            GEN,
            "throw Iterator.prototype.some.call(gen(), function(v) { return v === 20; });"
        ),
        "true"
    )
}

#[test]
fn the_helpers_accept_a_plain_object_source() {
    assert_eq!(
        with(
            PLAIN,
            "throw Iterator.prototype.map.call(plain(), function(v) { return v * 10; }) \
               .toArray().join(',');"
        ),
        "0,10,20"
    );
    assert_eq!(
        with(
            PLAIN,
            "throw Iterator.prototype.take.call(plain(), 2).toArray().join(',');"
        ),
        "0,1"
    );
    assert_eq!(
        with(
            PLAIN,
            "throw Iterator.prototype.find.call(plain(), function(v) { return v === 1; });"
        ),
        "1"
    )
}

// ---------------------------------------------------------------------------
// Iterator-protocol details the old eager drain got wrong.
// ---------------------------------------------------------------------------

#[test]
fn the_next_method_is_resolved_exactly_once() {
    assert_eq!(
        thrown(
            "var reads = 0; var it = { get next() { reads++; \
                 return function() { return { done: true }; }; } }; \
                 Iterator.prototype.toArray.call(it); throw reads;"
        ),
        "1"
    )
}

#[test]
fn a_throwing_next_getter_is_observed_once_and_propagates() {
    assert_eq!(
        thrown(
            "var reads = 0; var it = { get next() { reads++; throw new Error('x'); } }; \
                 try { Iterator.prototype.toArray.call(it); } catch (e) {} \
                 throw reads;"
        ),
        "1"
    )
}

#[test]
fn a_non_object_next_result_is_a_type_error() {
    assert_eq!(
        thrown(
            "try { Iterator.prototype.toArray.call({ next: function() { return 1; } }); \
                 throw 'no-throw'; } \
               catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"
        ),
        "TypeError"
    )
}

#[test]
fn value_is_not_read_from_a_done_result() {
    // Spec 7.4.2 `IteratorStepValue` reads `done` first and only then `value`,
    // so a throwing `value` getter on a `done` result is never observed.
    assert_eq!(
        thrown(
            "var touched = false; var it = { next: function() { \
                 return { done: true, get value() { touched = true; return 1; } }; } }; \
                 Iterator.prototype.toArray.call(it); throw touched;"
        ),
        "false"
    )
}

#[test]
fn a_throwing_callback_propagates_out_of_the_helper() {
    assert_eq!(
        thrown(
            "try { [1].values().map(function() { throw new Error('boom'); }).toArray(); \
                 throw 'no-throw'; } catch (e) { throw e.message; }"
        ),
        "boom"
    )
}

// ---------------------------------------------------------------------------
// Brand checks: spec 27.1.4 steps 1-3 (an Object receiver with a callable
// `next`) and step 4 (a callable callback).
// ---------------------------------------------------------------------------

#[test]
fn a_receiver_without_a_callable_next_is_rejected() {
    for helper in [
        "map", "filter", "flatMap", "reduce", "forEach", "some", "every", "find",
    ] {
        let body = format!(
            "try {{ Iterator.prototype.{helper}.call({{}}, function() {{}}); throw 'no-throw'; }} \
             catch (e) {{ throw e instanceof TypeError ? 'TypeError' : 'other'; }}"
        );
        assert_eq!(thrown(&body), "TypeError", "{helper} must reject {{}}");
    }
}

#[test]
fn a_non_object_receiver_is_rejected() {
    for helper in CALLBACK_HELPERS {
        let body = format!(
            "try {{ Iterator.prototype.{helper}.call(null, function() {{}}); throw 'no-throw'; }} \
             catch (e) {{ throw e instanceof TypeError ? 'TypeError' : 'other'; }}"
        );
        assert_eq!(thrown(&body), "TypeError", "{helper} must reject null");
    }
}

#[test]
fn a_non_callable_callback_is_rejected() {
    for helper in CALLBACK_HELPERS {
        let body = format!(
            "try {{ [1].values().{helper}(1); throw 'no-throw'; }} \
                     catch (e) {{ throw e instanceof TypeError ? 'TypeError' : 'other'; }}"
        );
        assert_eq!(thrown(&body), "TypeError", "{helper} must reject a number");
    }
}

// ---------------------------------------------------------------------------
// Composition across the machine-free / machine-driven boundary. A v12
// element-store source is stepped by the engine's `iterator_next`; anything
// else by the interpreter. The split must be invisible from JS.
// ---------------------------------------------------------------------------

#[test]
fn lazy_helpers_compose_in_both_orders() {
    assert_eq!(
        thrown(
            "throw [1,2,3,4].values().map(function(v) { return v; }) \
                 .take(2).toArray().join(',');"
        ),
        "1,2"
    );
    assert_eq!(
        thrown(
            "throw [1,2,3,4].values().drop(2).map(function(v) { return v * 3; }) \
                 .toArray().join(',');"
        ),
        "9,12"
    );
}

#[test]
fn take_over_a_lazy_helper_pulls_only_what_it_needs() {
    // The laziness assertion that actually distinguishes pull-through from a
    // drain: `take(1)` must not advance the mapper past its first value.
    assert_eq!(
        thrown(
            "var c = 0; var out = [1,2,3].values() \
                 .map(function(v) { c++; return v; }).take(1).toArray(); \
                 throw out.join(',') + '|' + c;"
        ),
        "1|1"
    )
}

#[test]
fn a_lazy_helper_over_a_generator_composes_with_take() {
    assert_eq!(
        with(
            GEN,
            "throw Iterator.prototype.map.call(gen(), function(v) { return v; }) \
               .take(2).toArray().join(',');"
        ),
        "10,20"
    )
}

#[test]
fn a_helper_inside_a_helper_callback_re_enters_cleanly() {
    assert_eq!(
        thrown(
            "throw [1,2].values().map(function(v) { \
                 return [v].values().map(function(w) { return w + 1; }).toArray()[0]; \
               }).toArray().join(',');"
        ),
        "2,3"
    )
}
