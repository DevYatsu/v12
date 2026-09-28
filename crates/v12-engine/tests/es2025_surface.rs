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

// ---------------------------------------------------------------------------
// Group 2 — Set.prototype relational methods (24.2.3)
// ---------------------------------------------------------------------------

/// The seven methods, as a list, so a missing one fails with its name.
const SET_RELATIONAL: [&str; 7] = [
    "union",
    "intersection",
    "difference",
    "symmetricDifference",
    "isSubsetOf",
    "isSupersetOf",
    "isDisjointFrom",
];

/// Every one is installed as a function with the spec method attributes
/// (`{writable: true, enumerable: false, configurable: true}`).
#[test]
fn set_relational_methods_are_installed_with_spec_attributes() {
    for name in SET_RELATIONAL {
        assert_eq!(
            attr_triple_on("Set.prototype", &format!("'{name}'")),
            "true,false,true",
            "Set.prototype.{name} attribute triple"
        );
        assert_eq!(
            eval_display(&format!("typeof Set.prototype.{name}")),
            "function",
            "Set.prototype.{name} is callable"
        );
    }
}

/// `union` keeps `this`'s order and appends the argument's elements that are
/// not already present (24.2.3.4 step 3-4).
#[test]
fn union_concatenates_in_spec_order_without_duplicates() {
    assert_eq!(
        eval_display("new Set([3,1]).union(new Set([2,1])).values().toArray().join('-')"),
        "3-1-2"
    );
    assert_eq!(eval_display("new Set([1,2]).union(new Set([2,3])).size"), "3");
}

/// The argument goes through `GetSetFromIterable`, so an array is accepted and
/// its duplicates collapse — the *set* of the argument is what every one of the
/// seven algorithms measures.
#[test]
fn union_accepts_an_array_argument_and_dedupes_it() {
    assert_eq!(
        eval_display("new Set([1,2]).union([2,3,3]).values().toArray().join('-')"),
        "1-2-3"
    );
    // SameValueZero: a duplicated `NaN` is still one element.
    assert_eq!(eval_display("new Set([1]).union([NaN,NaN]).size"), "2");
}

/// `intersection` builds its table from the argument and then walks the
/// receiver, so the result order is `this`'s (24.2.3.5 step 7) — the observable
/// that distinguishes it from a naive "iterate the argument" implementation.
#[test]
fn intersection_iterates_the_receiver_in_spec_order() {
    assert_eq!(
        eval_display(
            "new Set([3,1,2]).intersection(new Set([1,2,9])).values().toArray().join('-')"
        ),
        "1-2"
    );
    // An empty operand short-circuits to the empty result (24.2.3.5 step 4).
    assert_eq!(eval_display("new Set().intersection([1]).size"), "0");
    assert_eq!(eval_display("new Set([1]).intersection([]).size"), "0");
}

/// `difference` yields the receiver's elements absent from the argument
/// (24.2.3.6).
#[test]
fn difference_keeps_receiver_order_and_drops_argument_members() {
    assert_eq!(
        eval_display("new Set([1,2,3]).difference([2]).values().toArray().join('-')"),
        "1-3"
    );
    assert_eq!(eval_display("new Set([1,2]).difference([1,2]).size"), "0");
}

/// `symmetricDifference` yields the receiver-only elements and then the
/// argument-only ones, in that order (24.2.3.7).
#[test]
fn symmetric_difference_yields_this_then_argument() {
    assert_eq!(
        eval_display(
            "new Set([1,2]).symmetricDifference([2,3]).values().toArray().join('-')"
        ),
        "1-3"
    );
}

/// The three predicates return *primitive* booleans, so `=== true` holds, and
/// each has a true and a false case.
#[test]
fn set_predicates_return_primitive_booleans() {
    for (expr, want) in [
        ("new Set([1]).isSubsetOf([1,2])", "true"),
        ("new Set([1,9]).isSubsetOf([1,2])", "false"),
        ("new Set([1,2]).isSupersetOf([1])", "true"),
        ("new Set([1]).isSupersetOf([1,2])", "false"),
        ("new Set([1]).isDisjointFrom([2])", "true"),
        ("new Set([1]).isDisjointFrom([1])", "false"),
    ] {
        assert_eq!(eval_display(expr), want, "{expr}");
    }
    assert_eq!(
        eval_display("typeof new Set([1]).isSubsetOf([1])"),
        "boolean"
    );
}

/// A result Set is an ordinary `Set`: `%Set.prototype%`, a live `size`, and the
/// inherited methods.
#[test]
fn a_result_set_is_a_real_set() {
    assert_eq!(
        eval_display(
            "Object.getPrototypeOf(new Set([1]).union([2])) === Set.prototype"
        ),
        "true"
    );
    assert_eq!(eval_display("new Set([1,2]).union([]).size"), "2");
    assert_eq!(
        eval_display("typeof new Set([1]).difference([1]).has"),
        "function"
    );
    // The inherited method really is reachable through the linked prototype,
    // so the check above is not a detached method object.
    assert_eq!(eval_display("new Set([1]).union([2]).has(2)"), "true");
    assert_eq!(eval_display("new Set([1]).union([2]).size"), "2");
}

/// Every method requires the `[[SetData]]` internal slot (24.2.3 step 1), so a
/// plain object receiver is a `TypeError` before the argument is examined.
#[test]
fn set_relational_methods_reject_a_non_set_receiver() {
    for name in SET_RELATIONAL {
        assert_eq!(
            eval_display(&format!(
                "(() => {{ try {{ Set.prototype.{name}.call({{}}, []); return 'NO THROW'; }}
                   catch (e) {{ return e.constructor.name; }} }})()"
            )),
            "TypeError",
            "Set.prototype.{name} receiver brand check"
        );
    }
}

/// The argument is `RequireObjectCoercible`'d and then iterated, so `undefined`,
/// a missing argument, and a non-iterable primitive are all `TypeError`s.
#[test]
fn set_relational_methods_reject_a_non_iterable_argument() {
    for arg in ["", "undefined", "null", "5", "{}"] {
        assert_eq!(
            eval_display(&format!(
                "(() => {{ try {{ new Set([1]).union({arg}); return 'NO THROW'; }}
                   catch (e) {{ return e.constructor.name; }} }})()"
            )),
            "TypeError",
            "Set.prototype.union({arg})"
        );
    }
}

/// A `Map` argument iterates `[key, value]` pairs, so `union` on one adds the
/// pair arrays themselves — the same observable every engine produces.
#[test]
fn a_map_argument_iterates_entry_pairs() {
    assert_eq!(
        eval_display(
            "(() => { const s = new Set(['a']).union(new Map([['b', 1]]));
               const first = s.values().toArray()[1];
               return Array.isArray(first) && first[0] === 'b' && first[1] === 1; })()"
        ),
        "true"
    );
}

/// A string argument iterates by code point, not by code unit.
#[test]
fn a_string_argument_iterates_by_code_point() {
    assert_eq!(
        eval_display("new Set(['ab']).union('c').values().toArray().join('-')"),
        "ab-c"
    );
    assert_eq!(eval_display("new Set().union('\u{1F600}').size"), "1");
}
