//! `Promise[@@species]` as a spec accessor, and the constructor selection
//! `Promise.prototype.then` / `Promise.prototype.finally` perform with it
//! (ECMA-262 27.5.4.10 and the `SpeciesConstructor` step-3 sequence inlined
//! in 27.5.5.4).
//!
//! Two things are pinned here. First the *shape* of the property: it is an
//! accessor whose set accessor is `undefined`, whose getter is a nullary
//! built-in named `"get [Symbol.species]"` returning its receiver — not the
//! data property the accessor used to stand in for. Second the *selection*:
//! `Get(promise, "constructor")`, then `Get(C, @@species)`, with the two
//! fallbacks (`undefined` constructor, `null`/`undefined` species → `%Promise%`)
//! and the two `TypeError`s (non-object constructor, non-constructor species)
//! in their spec places.
//!
//! Handler-result adoption is a separate task: these tests read what the
//! derived promise *is*, never what its reaction resolves it with.
//!
//! Two host-API conventions the sources below are written around, both already
//! established by `promise_capability.rs`:
//!
//! - Results travel on `globalThis`, never on a `let`/`var`. A top-level `let`
//!   in one `eval` is not visible to the next program — and a `let` in the
//!   *same* program as a `class` declaration followed by
//!   `Object.defineProperty` reads back clobbered (a pre-existing codegen bug
//!   this lane does not own), so the counters are read off `globalThis`.
//! - A value with a class body gets its `Symbol.species` through
//!   `Object.defineProperty`, never through assignment: `Sub[@@species] = C`
//!   is a *no-op*, because `Sub` inherits the setter-less `Promise[@@species]`
//!   accessor through its constructor chain (see
//!   `assigning_species_through_a_subclass_is_a_no_op`, which pins that).
//!
//! A note on how the receiver is built. The engine's `is_promise` test is
//! exact (a `Kind::Promise` whose property vector is still the three internal
//! slots), so an *own data* property installed on a promise object —
//! `p.constructor = null` — stops it answering as a promise and `then` rejects
//! it before the species read ever runs. The cases that need a poisoned
//! `constructor` therefore put it on a subclass prototype, where the instance
//! itself stays a pristine promise and the read reaches the spec's step 3.

use v12_engine::Engine;

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

/// Evaluates `source` and returns the display string of whatever it threw.
fn display(source: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(source).expect_err("source must throw");
    engine.to_display_string(value)
}

// ---------------------------------------------------------------------------
// The accessor itself
// ---------------------------------------------------------------------------

#[test]
fn species_is_an_accessor_with_no_set_accessor() {
    assert_eq!(
        display(
            "let d = Object.getOwnPropertyDescriptor(Promise, Symbol.species); \
             throw [typeof d.get, d.set === undefined, d.value === undefined, \
             d.enumerable, d.configurable].join(',');"
        ),
        "function,true,true,false,true"
    );
}

#[test]
fn species_getter_has_length_zero_and_the_spec_name() {
    assert_eq!(
        display(
            "let g = Object.getOwnPropertyDescriptor(Promise, Symbol.species).get; \
             let ld = Object.getOwnPropertyDescriptor(g, 'length'); \
             throw [g.length, ld.writable, ld.enumerable, ld.configurable, \
             g.name].join(',');"
        ),
        "0,false,false,true,get [Symbol.species]"
    );
}

#[test]
fn species_getter_returns_its_receiver() {
    // The spec's whole body is `1. Return the this value.`, so it must return
    // an arbitrary receiver unchanged — not `%Promise%` and not `undefined`.
    assert_eq!(
        display(
            "let g = Object.getOwnPropertyDescriptor(Promise, Symbol.species).get; \
             let plain = {}; throw g.call(plain) === plain;"
        ),
        "true"
    );
}

#[test]
fn reading_species_still_yields_the_receiver_constructor() {
    // `Promise[Symbol.species] === Promise` now holds through the getter.
    assert_eq!(
        display("throw Promise[Symbol.species] === Promise;"),
        "true"
    );
}

#[test]
fn sloppy_assignment_to_species_is_a_no_op() {
    // No set accessor: `Set` on an own accessor property with an `undefined`
    // setter fails (ES 9.1.9 step 1-c-iii) — silently in sloppy mode, and it
    // never converts the property into a data property.
    assert_eq!(
        after_jobs(
            "Promise[Symbol.species] = function () {};",
            "let d = Object.getOwnPropertyDescriptor(Promise, Symbol.species); \
             throw [typeof d.get, d.set === undefined, d.value === undefined, \
             Promise[Symbol.species] === Promise].join(',');"
        ),
        "function,true,true,true"
    );
}

#[test]
fn strict_assignment_to_species_throws() {
    assert_eq!(
        display(
            "'use strict'; \
             try { Promise[Symbol.species] = function () {}; } catch (e) { \
               throw e instanceof TypeError ? 'TypeError' : 'other'; } \
             throw 'no throw';"
        ),
        "TypeError"
    );
}

#[test]
fn assigning_species_through_a_subclass_is_a_no_op() {
    // `class Sub extends Promise` puts `Promise` on `Sub`'s constructor chain,
    // so `Sub[@@species]` *reaches* the setter-less accessor: `Set` walks to
    // it, finds no set accessor, and fails. Script has to use
    // `Object.defineProperty` (which is what every case below does).
    assert_eq!(
        after_jobs(
            "class Sub extends Promise {} Sub[Symbol.species] = function Other() {}; \
             globalThis.d = Object.getOwnPropertyDescriptor(Sub, Symbol.species);",
            "throw [globalThis.d === undefined, \
             Sub[Symbol.species] === Sub].join(',');"
        ),
        "true,true"
    );
}

// ---------------------------------------------------------------------------
// Selection in `then`
// ---------------------------------------------------------------------------

/// A subclass whose `Symbol.species` names a second constructor, so both the
/// selected constructor and the derived promise's identity are observable.
const SPECIES_CLASS: &str = "globalThis.selected = 0; \
    class Derived extends Promise { \
      constructor(executor) { super(executor); globalThis.selected += 1; } \
    } \
    class Sub extends Promise {} \
    Object.defineProperty(Sub, Symbol.species, { value: Derived }); \
    globalThis.sub = new Sub(function (resolve) { resolve(1); });";

/// The same, with `value: null` — the step-5 fallback.
const NULL_SPECIES_CLASS: &str = "class Sub extends Promise {} \
    Object.defineProperty(Sub, Symbol.species, { value: null }); \
    globalThis.sub = new Sub(function (resolve) { resolve(1); });";

/// The same, with `value: undefined`.
const UNDEFINED_SPECIES_CLASS: &str = "class Sub extends Promise {} \
    Object.defineProperty(Sub, Symbol.species, { value: undefined }); \
    globalThis.sub = new Sub(function (resolve) { resolve(1); });";

#[test]
fn then_uses_the_subclass_species_constructor() {
    assert_eq!(
        after_jobs(
            &format!(
                "{SPECIES_CLASS} \
                 globalThis.p = Promise.resolve(1).then.call(globalThis.sub, function () {{ return 2; }});"
            ),
            "throw [globalThis.selected, globalThis.p instanceof Derived, \
             globalThis.p instanceof Sub].join(',');"
        ),
        "1,true,false"
    );
}

#[test]
fn then_on_a_plain_promise_still_yields_a_plain_promise() {
    // The default path: `Get(p, "constructor")` is `%Promise%` and
    // `Get(%Promise%, @@species)` is `%Promise%` again, so nothing changes for
    // ordinary script.
    assert_eq!(
        after_jobs(
            "globalThis.p = Promise.resolve(1).then(function () { return 2; });",
            "throw [globalThis.p instanceof Promise, \
             globalThis.p.constructor === Promise].join(',');"
        ),
        "true,true"
    );
}

#[test]
fn then_propagates_a_constructor_getter_that_throws() {
    // `SpeciesConstructor` runs before `NewPromiseCapability`, so the abrupt
    // `Get` is the completion of `then` itself.
    assert_eq!(
        display(
            "function Boom() {} \
             class Sub extends Promise {} \
             Object.defineProperty(Sub.prototype, 'constructor', { \
               get: function () { throw new Boom(); } }); \
             let p = new Sub(function (resolve) { resolve(1); }); \
             try { p.then(function () {}); } catch (e) { throw e instanceof Boom ? 'Boom' : 'other'; } \
             throw 'no throw';"
        ),
        "Boom"
    );
}

#[test]
fn then_reads_the_constructor_property_exactly_once() {
    assert_eq!(
        after_jobs(
            "globalThis.reads = 0; \
             class Sub extends Promise {} \
             Object.defineProperty(Sub.prototype, 'constructor', { \
               get: function () { globalThis.reads += 1; return Promise; } }); \
             new Sub(function (resolve) { resolve(1); }).then(function () {});",
            "throw globalThis.reads;"
        ),
        "1"
    );
}

#[test]
fn then_throws_when_constructor_is_null() {
    // `C` is `null`: not `undefined`, so the default does not apply, and a
    // non-object `C` is a `TypeError`.
    assert_eq!(
        display(
            "class Sub extends Promise {} \
             Object.defineProperty(Sub.prototype, 'constructor', { value: null }); \
             let p = new Sub(function (resolve) { resolve(1); }); \
             try { p.then(function () {}); } catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; } \
             throw 'no throw';"
        ),
        "TypeError"
    );
}

#[test]
fn then_throws_when_constructor_is_a_primitive() {
    assert_eq!(
        display(
            "class Sub extends Promise {} \
             Object.defineProperty(Sub.prototype, 'constructor', { value: 42 }); \
             let p = new Sub(function (resolve) { resolve(1); }); \
             try { p.then(function () {}); } catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; } \
             throw 'no throw';"
        ),
        "TypeError"
    );
}

#[test]
fn then_falls_back_to_the_default_when_constructor_is_undefined() {
    assert_eq!(
        after_jobs(
            "class Sub extends Promise {} \
             Object.defineProperty(Sub.prototype, 'constructor', { value: undefined }); \
             globalThis.p = new Sub(function (resolve) { resolve(1); }).then(function () { return 2; });",
            "throw [globalThis.p instanceof Promise, \
             globalThis.p.constructor === Promise].join(',');"
        ),
        "true,true"
    );
}

#[test]
fn a_null_species_falls_back_to_the_default_constructor() {
    assert_eq!(
        after_jobs(
            &format!(
                "{NULL_SPECIES_CLASS} \
                 globalThis.p = Promise.resolve(1).then.call(globalThis.sub, function () {{ return 2; }});"
            ),
            "throw [globalThis.p instanceof Promise, \
             globalThis.p.constructor === Promise].join(',');"
        ),
        "true,true"
    );
}

#[test]
fn an_undefined_species_falls_back_to_the_default_constructor() {
    assert_eq!(
        after_jobs(
            &format!(
                "{UNDEFINED_SPECIES_CLASS} \
                 globalThis.p = Promise.resolve(1).then.call(globalThis.sub, function () {{ return 2; }});"
            ),
            "throw [globalThis.p instanceof Promise, \
             globalThis.p.constructor === Promise].join(',');"
        ),
        "true,true"
    );
}

#[test]
fn then_throws_when_species_is_not_a_constructor() {
    assert_eq!(
        display(
            "class Sub extends Promise {} \
             Object.defineProperty(Sub, Symbol.species, { value: {} }); \
             let p = new Sub(function (resolve) { resolve(1); }); \
             try { p.then(function () {}); } catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; } \
             throw 'no throw';"
        ),
        "TypeError"
    );
}

#[test]
fn then_propagates_a_species_getter_that_throws() {
    assert_eq!(
        display(
            "function Boom() {} \
             class Sub extends Promise {} \
             Object.defineProperty(Sub, Symbol.species, { \
               get: function () { throw new Boom(); } }); \
             let p = new Sub(function (resolve) { resolve(1); }); \
             try { p.then(function () {}); } catch (e) { throw e instanceof Boom ? 'Boom' : 'other'; } \
             throw 'no throw';"
        ),
        "Boom"
    );
}

#[test]
fn then_settles_a_species_constructed_derived_promise() {
    // The derived promise is the species constructor's own, so the reaction
    // still runs and delivers the watched value to the handler: the
    // capability's promise, not a copy.
    assert_eq!(
        after_jobs(
            &format!(
                "{SPECIES_CLASS} \
                 globalThis.log = []; \
                 globalThis.sub.then(function (v) {{ globalThis.log.push('h:' + v); }});"
            ),
            "throw [globalThis.log.join(','), globalThis.selected].join('|');"
        ),
        "h:1|1"
    );
}

// ---------------------------------------------------------------------------
// Selection in `finally`
// ---------------------------------------------------------------------------

#[test]
fn finally_uses_the_subclass_species_constructor() {
    assert_eq!(
        after_jobs(
            &format!(
                "{SPECIES_CLASS} \
                 globalThis.p = Promise.resolve(1).finally.call(globalThis.sub, function () {{}});"
            ),
            "throw [globalThis.selected, globalThis.p instanceof Derived].join(',');"
        ),
        "1,true"
    );
}

#[test]
fn finally_throws_when_constructor_is_not_an_object() {
    assert_eq!(
        display(
            "class Sub extends Promise {} \
             Object.defineProperty(Sub.prototype, 'constructor', { value: null }); \
             let p = new Sub(function (resolve) { resolve(1); }); \
             try { p.finally(function () {}); } catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; } \
             throw 'no throw';"
        ),
        "TypeError"
    );
}

#[test]
fn finally_still_runs_its_callback_through_the_species_derived_promise() {
    assert_eq!(
        after_jobs(
            &format!(
                "{SPECIES_CLASS} \
                 globalThis.log = []; \
                 Promise.resolve(7).finally.call(globalThis.sub, function () {{ \
                   globalThis.log.push('finally'); }});"
            ),
            "throw globalThis.log.join(',');"
        ),
        "finally"
    );
}

// ---------------------------------------------------------------------------
// The statics do NOT select a species constructor
// ---------------------------------------------------------------------------

#[test]
fn the_static_methods_never_read_species() {
    // 27.5.4.x gives every static `NewPromiseCapability(this)` — no species
    // read anywhere on that path — so a receiver whose `Symbol.species` getter
    // throws must still work, and the resulting promise is an instance of that
    // receiver, not of `%Promise%`.
    assert_eq!(
        display(
            "globalThis.reads = 0; \
             function Boom() {} \
             class C extends Promise {} \
             Object.defineProperty(C, Symbol.species, { \
               get: function () { globalThis.reads += 1; throw new Boom(); } }); \
             throw [C.all([]) instanceof C, C.race([]) instanceof C, \
             C.allSettled([]) instanceof C, C.any([]) instanceof C, \
             C.withResolvers().promise instanceof C, \
             C.try(function () { return 1; }) instanceof C, \
             globalThis.reads].join(',');"
        ),
        "true,true,true,true,true,true,0"
    );
}
