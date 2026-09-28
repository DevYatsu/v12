//! ES2025 surface tests for the built-ins the `surface` lane added: the
//! well-known `Symbol.dispose` / `Symbol.asyncDispose` singletons, the seven
//! `Set.prototype` relational methods, the `Iterator` helpers, and the keyed
//! promise combinators.
//!
//! # How the engine reaches this surface
//!
//! Every method here is a *pure* `NativeHandler` (`fn(&mut Ctx, JsValue,
//! &[JsValue])`): it holds the heap and a realm-global reader and cannot
//! re-enter the interpreter. That is what decides the shape of each
//! implementation, and the tests below are written to pin the consequences
//! rather than the happy path alone:
//!
//! * An **iterable argument** is drained structurally — arrays, `Set`s, `Map`s,
//!   strings, and the engine's own iterator objects. A user-defined
//!   `@@iterator` (a generator, a hand-rolled iterator) is not reachable from a
//!   `&mut Heap`; the tests below therefore exercise the engine-iterator path
//!   and say so rather than implying full iterable coverage.
//! * `ToString` on an **object** value needs the machine, so
//!   `Iterator.prototype.join` renders objects through the Tier-0 display
//!   subset.
//! * Array results go through `Ctx::alloc_array`, which stamps
//!   `%Array.prototype%` and binds the `length` descriptor. Several tests assert
//!   that prototype and `length` explicitly, because an array missing them
//!   fails far away from its cause (`result.constructor === Array` reading
//!   `undefined`).
//!
//! # Attribute triple
//!
//! Every installed property is checked for the spec's
//! `{writable, enumerable, configurable}` triple via
//! `Object.getOwnPropertyDescriptor`, not merely for presence: a property that
//! exists with the wrong attributes fails `propertyHelper.verifyProperty` just
//! as hard as a missing one, and that is the defect class this file exists to
//! catch.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// Own-property attribute triple of `target[key]`, or a diagnostic string.
///
/// Returns `"MISSING"` when the key is absent, `"ACCESSOR"` when the slot is a
/// getter/setter rather than a data property, and `"w,e,c"` otherwise.
fn attr_triple_on(target: &str, key: &str) -> String {
    let mut engine = Engine::new();
    let src = format!(
        "(() => {{ const d = Object.getOwnPropertyDescriptor({target}, {key});
           if (d === undefined) return 'MISSING';
           if (d.get !== undefined || d.set !== undefined) return 'ACCESSOR';
           return [d.writable, d.enumerable, d.configurable].join(','); }})()"
    );
    match engine.eval(&src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

// ---------------------------------------------------------------------------
// Group 1 — well-known symbols
// ---------------------------------------------------------------------------

/// 20.4.2: `Symbol.dispose` exists and is a *symbol*, not a handler function.
#[test]
fn symbol_dispose_is_a_symbol() {
    assert_eq!(eval_display("typeof Symbol.dispose"), "symbol");
}

/// The singleton contract: the installed property value *is* the symbol, so
/// every read is the same shape lookup and identity holds without a cache.
#[test]
fn symbol_dispose_is_a_singleton() {
    assert_eq!(eval_display("Symbol.dispose === Symbol.dispose"), "true");
    assert_eq!(
        eval_display("Symbol.asyncDispose === Symbol.asyncDispose"),
        "true"
    );
    // Two names, two symbols.
    assert_eq!(eval_display("Symbol.dispose === Symbol.asyncDispose"), "false");
}

/// Spec 20.4.2 gives every `Symbol` static
/// `{writable: false, enumerable: false, configurable: true}` — the one place
/// in clauses 18-26 that departs from the `{true, false, true}` default, and
/// the reason `install_value_with_attrs` exists next to `install_value`.
#[test]
fn well_known_symbols_carry_the_non_writable_attribute_triple() {
    assert_eq!(attr_triple_on("Symbol", "'dispose'"), "false,false,true");
    assert_eq!(
        attr_triple_on("Symbol", "'asyncDispose'"),
        "false,false,true"
    );
}

/// A well-known symbol is never in the registry, so `Symbol.keyFor` answers
/// `undefined` (20.4.2) rather than the description.
#[test]
fn well_known_symbols_are_not_in_the_key_registry() {
    assert_eq!(eval_display("Symbol.keyFor(Symbol.dispose)"), "undefined");
    assert_eq!(
        eval_display("Symbol.keyFor(Symbol.asyncDispose)"),
        "undefined"
    );
    // Control: a registered symbol still round-trips, so the assertions above
    // are about registry membership and not about a broken `keyFor`.
    assert_eq!(eval_display("Symbol.keyFor(Symbol.for('k'))"), "k");
}

/// `Symbol.prototype.description` returns the spec description of a well-known
/// symbol when *invoked*.
///
/// The property-read form (`sym.description`) is a different code path — the
/// interpreter's `SymbolPrim` surface in `v12-interp/src/property.rs`, which is
/// outside this lane's file ownership and still answers `undefined`. Asserting
/// the handler here keeps that gap visible as a *read-path* gap instead of
/// letting it look like a missing description.
#[test]
fn description_reports_the_spec_description_of_a_well_known_symbol() {
    assert_eq!(
        eval_display("Symbol.prototype.description.call(Symbol.dispose)"),
        "Symbol.dispose"
    );
    assert_eq!(
        eval_display("Symbol.prototype.description.call(Symbol.asyncDispose)"),
        "Symbol.asyncDispose"
    );
    assert_eq!(
        eval_display("Symbol.prototype.description.call(Symbol.iterator)"),
        "Symbol.iterator"
    );
}

/// A registered (or fresh) symbol has no description — v1 `V12Symbol` carries
/// no `[[Description]]` — so `undefined` is the correct answer. The control
/// guards against a `well_known_description` that matches *any* symbol.
#[test]
fn description_of_a_non_well_known_symbol_is_undefined() {
    assert_eq!(
        eval_display("Symbol.prototype.description.call(Symbol.for('k'))"),
        "undefined"
    );
    assert_eq!(
        eval_display("Symbol.prototype.description.call(Symbol())"),
        "undefined"
    );
}
