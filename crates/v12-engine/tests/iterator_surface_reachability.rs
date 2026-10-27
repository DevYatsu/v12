//! The iterator helpers are reachable as a **property** on the receivers whose
//! spec `[[Prototype]]` chain reaches `%Iterator.prototype%` (spec 27.1.4).
//!
//! Regression: `iterator_method_surface` (`property.rs`) gated on
//! `Kind::Iterator` only, so a generator — whose `prototype` field holds the
//! captured environment rather than `%GeneratorPrototype%`, so it has no chain
//! to consult — had no `.map`/`.filter`/`.reduce`/… at all. test262 calls the
//! helpers directly (`g().map(f)`), so 83 files failed at the *lookup*.
//!
//! The trap this file guards is the other direction: the same surface must NOT
//! leak onto plain objects. `Kind::Ordinary` is the kind of nearly every object,
//! so admitting it would put `.map` on `{}`. Every negative case below is
//! `undefined` today and must stay that way.

use v12_engine::Engine;

/// Evaluates `source` and returns the display string of the value or of the
/// thrown value. One behaviour per eval, so a throw can never mask a later
/// observation.
fn thrown(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

/// `typeof <expr>` for each expression, which is the whole assertion: a helper
/// is either reachable or it is not.
#[track_caller]
fn assert_undefined(expr: &str) {
    let src = format!("throw typeof {expr};");
    assert_eq!(thrown(&src), "undefined", "`{expr}` must stay undefined");
}

#[track_caller]
fn assert_function(expr: &str) {
    let src = format!("throw typeof {expr};");
    assert_eq!(thrown(&src), "function", "`{expr}` must be a function");
}

// ---------------------------------------------------------------------------
// The fix: a generator reaches the helpers through its own properties.
// ---------------------------------------------------------------------------

#[test]
fn a_generator_exposes_every_helper_as_a_property() {
    for helper in [
        "map", "filter", "take", "drop", "flatMap", "reduce", "toArray", "forEach", "some",
        "every", "find",
    ] {
        assert_function(&format!("(function* g(){{ yield 1; }})().{helper}"));
    }
}

#[test]
fn a_generator_helper_actually_runs() {
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; } \
                 throw g().map(function(v) { return v * 10; }).toArray().join(',');"
        ),
        "10,20"
    );
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; yield 3; } \
                 throw g().filter(function(v) { return v > 1; }).toArray().join(',');"
        ),
        "2,3"
    );
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; } \
                 throw g().reduce(function(a, v) { return a + v; });"
        ),
        "3"
    );
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; } \
                 throw g().take(1).toArray().join(',');"
        ),
        "1"
    );
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; } \
                 throw g().some(function(v) { return v === 2; });"
        ),
        "true"
    );
}

#[test]
fn a_generator_is_still_lazy_through_a_property_call() {
    // `take` over a lazy `map` on a generator: the mapper must not run for the
    // second source value.
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; yield 3; } \
                 var c = 0; \
                 var out = g().map(function(v) { c++; return v; }).take(1).toArray(); \
                 throw out.join(',') + '|' + c;"
        ),
        "1|1"
    )
}

#[test]
fn a_generator_for_of_still_works_before_and_after_a_helper() {
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; } var o = []; \
                 for (const v of g()) o.push(v); \
                 throw o.join(',');"
        ),
        "1,2"
    );
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; } var o = []; \
                 for (const v of g().map(function(v) { return v + 1; })) o.push(v); \
                 throw o.join(',');"
        ),
        "2,3"
    )
}

// ---------------------------------------------------------------------------
// The generator's own iterator protocol is untouched: `next` must keep being
// the *generator* method, never the element-store `iterator_next`.
// ---------------------------------------------------------------------------

#[test]
fn a_generator_next_still_drives_the_generator() {
    assert_eq!(
        thrown("function* g() { yield 7; } throw g().next().value;"),
        "7"
    );
    assert_eq!(
        thrown(
            "function* g() { yield 1; yield 2; } \
                 var it = g(); var a = it.next(); var b = it.next(); \
                 throw [a.value, a.done, b.value, b.done].join(',');"
        ),
        "1,false,2,false"
    );
}

#[test]
fn a_generator_return_still_closes_it() {
    assert_eq!(
        thrown(
            "function* g() { yield 1; } var it = g(); it.next(); \
                 var r = it.return(5); throw [r.value, r.done].join(',');"
        ),
        "5,true"
    )
}

#[test]
fn a_generator_is_not_mistaken_for_an_array_or_a_v12_iterator() {
    assert_eq!(
        thrown("function* g() { yield 1; } throw Array.isArray(g());"),
        "false"
    );
    // The helper surface must not have replaced the generator with something
    // the engine can step positionally: draining it must still consult `next`.
    assert_eq!(
        thrown("function* g() { yield 1; yield 2; } throw g().toArray().join(',');"),
        "1,2"
    )
}

// ---------------------------------------------------------------------------
// Negative: the surface must not leak. `Kind::Ordinary` is nearly every object.
// ---------------------------------------------------------------------------

#[test]
fn a_plain_object_has_no_iterator_helpers() {
    for helper in ["map", "filter", "take", "drop", "toArray", "reduce", "find"] {
        assert_undefined(&format!("({{}}).{helper}"));
        assert_undefined(&format!("Object.prototype.{helper}"));
    }
}

#[test]
fn a_plain_class_instance_has_no_iterator_helpers() {
    for helper in ["map", "filter", "reduce", "take", "toArray"] {
        assert_undefined(&format!(
            "(function(){{ var o = new (class A {{}})(); return o.{helper}; }})()"
        ));
        assert_undefined(&format!("(class A {{}}).prototype.{helper}"));
    }
}

#[test]
fn other_builtin_receivers_have_no_iterator_helpers() {
    for expr in [
        "new Map()",
        "new Set()",
        "Promise.resolve(1)",
        "/x/",
        "new Date(0)",
        "new Error('x')",
        "Math",
        "JSON",
        "Symbol('s')",
        "(function f(){})",
        "(() => 1)",
    ] {
        assert_undefined(&format!("{expr}.map"));
    }
}

#[test]
fn a_primitive_receiver_has_no_iterator_helpers() {
    for expr in ["('s')", "(42)", "(true)"] {
        assert_undefined(&format!("{expr}.map"));
    }
}

#[test]
fn a_null_or_undefined_receiver_still_throws() {
    for expr in ["null", "undefined"] {
        let src = format!("try {{ {expr}.map; throw 'no-throw'; }} catch (e) {{ throw 'threw'; }}");
        assert_eq!(thrown(&src), "threw", "`{expr}.map` must throw");
    }
}

#[test]
fn an_arguments_object_has_no_iterator_helpers() {
    // `arguments` is `Kind::Arguments`, not `Kind::Iterator`; it must not gain
    // the surface.
    assert_undefined("(function(){ return arguments; })().map");
}

#[test]
fn array_prototype_methods_are_untouched() {
    // The negative control for the negative control: `[].reduce` is defined by
    // `Array.prototype`, and a regression that shadowed it with the iterator
    // helper would show up here as a *different* function, so assert the
    // behaviour too, not just the type.
    assert_function("[].reduce");
    assert_function("[].filter");
    assert_eq!(
        thrown("throw [1,2,3].reduce(function(a, v) { return a + v; });"),
        "6"
    );
    assert_eq!(
        thrown("throw [1,2,3,4].filter(function(v) { return v % 2 === 0; }).join(',');"),
        "2,4"
    );
}

// ---------------------------------------------------------------------------
// A `class X extends Iterator` instance needs no surface at all: the realm
// materializes `%Iterator.prototype%` as a real object and `iterator_construct`
// links the instance to it, so the ordinary prototype lookup finds the eleven
// installed helpers. This is the case the kind check deliberately does NOT
// cover, and the reason is recorded so nobody widens it to `Kind::Ordinary`.
// ---------------------------------------------------------------------------

#[test]
fn a_subclass_of_iterator_reaches_the_helpers_through_the_real_prototype() {
    for helper in ["map", "filter", "take", "reduce", "toArray", "find"] {
        assert_function(&format!(
            "(new (class X extends Iterator {{ next() {{ \
             return {{ done: true }}; }} }})()).{helper}"
        ));
    }
    assert_eq!(
        thrown(
            "class X extends Iterator { \
                 constructor() { super(); this.n = 0; } \
                 next() { this.n += 1; return this.n === 1 \
                          ? { value: 1, done: false } : { value: undefined, done: true }; } \
               } \
               throw new X().map(function(v) { return v * 5; }).toArray().join(',');"
        ),
        "5"
    );
}

#[test]
fn set_prototype_of_iterator_prototype_confers_the_helpers() {
    // The same reason as above, from the other direction: the helpers are real
    // installed properties, so a user can wire them onto any object.
    assert_function(
        "(function(){ var o = {}; Object.setPrototypeOf(o, Iterator.prototype); \
                            return o.map; })()",
    );
}
