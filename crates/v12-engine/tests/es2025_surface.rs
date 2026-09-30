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
//! * A **set-like argument** is read through `GetSetRecord` (24.2.3.1), not
//!   iterated. That is a different and stricter contract than "anything
//!   iterable": the argument must be an **Object** carrying callable `has` and
//!   `keys` members, and a `Set`, a `Map` and a hand-built set-like all satisfy
//!   it while a bare array and a bare string do **not** — an array has no `has`,
//!   so `union([3])` is a `TypeError`, not a union with `[3]`. The tests below
//!   use [`SET_LIKE_JS`] wherever they mean "an array of candidates" and pin the
//!   rejection separately.
//! * The *values* still come from a structural drain (`Ctx` cannot call
//!   `keys()`), so a set-like whose `keys()` returns something other than its
//!   own values is not modelled here. That limitation is a conformance gap, not
//!   a licence for the weaker argument check the tests used to encode.
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

/// The JS preamble that defines `setLike`, the spec-shaped argument the Set
/// relational methods actually accept.
///
/// `GetSetRecord` (24.2.3.1) reads `size`, `has` and `keys` off the argument and
/// requires the last two to be callable, so a bare array is *not* a valid
/// argument even though it is iterable — an array has no `has`. Several tests
/// below were originally written against a plain-array argument, which pinned
/// the pre-ES2025 "any iterable" contract that the same lane's implementation
/// had taken a shortcut against; conformance pinned it the other way
/// (`Set/prototype/*/array-throws.js` and `called-with-object.js` are 14 tests
/// of it). These tests are corrected to the spec's shape, not weakened: the
/// ordering, dedupe, predicate and identity assertions they were written for are
/// all preserved verbatim, only the argument is now one the spec admits.
///
/// `setLike` is a real `Set` rather than a hand-built `{size, has, keys}`
/// object, and the reason is the documented deviation in
/// `map::argument_set`: the *validation* is spec-exact, but the values still
/// come from a structural drain of the argument, which only reaches the kinds
/// `drain_iterable` knows. A plain object is not one of them, so a
/// hand-built set-like would throw rather than union. One test does use a
/// hand-built set-like to pin that boundary from the outside.
const SET_LIKE_JS: &str = "function setLike(a) { return new Set(a); }";

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// [`eval_display`] with [`SET_LIKE_JS`] prepended, for the Set relational
/// methods.
fn eval_set(src: &str) -> String {
    eval_display(&format!("{SET_LIKE_JS}\n{src}"))
}

/// Runs `setup`, drains the microtask queue, then evaluates `read`.
///
/// `setup` must declare its recording slot with `var`, not `let`: a
/// script-level `let` lives in a declarative environment that a second
/// `Engine::eval` cannot see, so the read would fail with a `ReferenceError`
/// rather than observing the settled value.
///
/// Promise assertions are meaningless without the drain: `allKeyed(…).then(…)`
/// runs its callback in a job, so reading the recorded value in the *same* turn
/// would see `pending` and pass vacuously — exactly the hazard this file exists
/// to catch. Splitting setup from read is what makes the assertion about the
/// settled value rather than about the synchronous return.
fn eval_then(setup: &str, read: &str) -> String {
    let mut engine = Engine::new();
    if let Err(thrown) = engine.eval(setup) {
        return format!("setup threw: {}", engine.to_display_string(thrown));
    }
    engine.run_jobs();
    match engine.eval(read) {
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
    assert_eq!(
        eval_display("Symbol.dispose === Symbol.asyncDispose"),
        "false"
    );
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
    assert_eq!(
        eval_display("new Set([1,2]).union(new Set([2,3])).size"),
        "3"
    );
}

/// The argument goes through `GetSetRecord`, so its duplicates collapse — the
/// *set* of the argument is what every one of the seven algorithms measures.
/// SameValueZero: a duplicated `NaN` is still one element.
///
/// The argument is a set-like rather than a bare array, because an array has no
/// `has` and is rejected; see `set_relational_rejects_a_non_object_argument`.
#[test]
fn union_accepts_a_set_like_argument_and_dedupes_it() {
    assert_eq!(
        eval_set("new Set([1,2]).union(setLike([2,3,3])).values().toArray().join('-')"),
        "1-2-3"
    );
    // SameValueZero: a duplicated `NaN` is still one element.
    assert_eq!(eval_set("new Set([1]).union(setLike([NaN,NaN])).size"), "2");
}

/// `GetSetRecord` step 1 is `If obj is not an Object, throw a TypeError`, and
/// steps 6 and 8 require `has` and `keys` to be callable. Neither an iterable
/// nor a set-like-without-`has` is enough — the distinction the conformance
/// corpus draws in `array-throws.js` (a TypeError) against
/// `set-like-array.js` (a union, because the array was given `has`/`keys`).
#[test]
fn set_relational_rejects_a_non_object_argument() {
    for arg in ["1", "'x'", "null", "undefined", "Symbol()", "1n", "true"] {
        assert!(
            eval_set(&format!(
                "(() => {{ try {{ new Set([1]).union({arg}); \
                               return 'NO THROW'; }} \
                               catch (e) {{ return e.constructor.name; }} }})()"
            )) == "TypeError",
            "union({arg}) must be a TypeError"
        );
    }
    // An array has no `has`, so it fails step 6 rather than being consumed.
    assert_eq!(
        eval_set(
            "(() => { try { new Set([1,2]).union([3]); return 'NO THROW'; } \
                  catch (e) { return e.constructor.name; } })()"
        ),
        "TypeError"
    );
    // A `Map` is a perfectly good set-like: it has all three members.
    assert_eq!(
        eval_display("new Set([1]).union(new Map([[2, 'x']])).size"),
        "2"
    );
}

/// The documented deviation in `map::argument_set`, pinned from the outside so
/// it cannot be mistaken for full `GetSetRecord` coverage: a hand-built set-like
/// *validates* (its `size`, `has` and `keys` are all read and all callable) but
/// its `keys()` is never called, because calling user code is what a `&mut Ctx`
/// native cannot do. So the members it reports are accepted and the values come
/// from a structural drain of the object instead, which a plain object cannot
/// satisfy — the method rejects it at the drain.
///
/// The spec accepts this argument and unions `[2, 3]`. `Set/prototype/*/
/// set-like-array.js` and `set-like-object.js` are the conformance half.
#[test]
fn a_hand_built_set_like_validates_but_is_not_iterated() {
    let out = eval_display(
        "(() => { var o = { size: 2, has: function (v) { return true; }, \
                     keys: function () { return [2, 3].values(); } }; \
           try { return 'size ' + new Set([1]).union(o).size; } \
           catch (e) { return e.constructor.name; } })()",
    );
    assert_eq!(
        out, "TypeError",
        "a plain-object set-like passes validation and fails the structural \
         drain — the deviation is in the values, not the argument check"
    );
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
    assert_eq!(eval_set("new Set().intersection(setLike([1])).size"), "0");
    assert_eq!(eval_set("new Set([1]).intersection(setLike([])).size"), "0");
}

/// `difference` yields the receiver's elements absent from the argument
/// (24.2.3.6).
#[test]
fn difference_keeps_receiver_order_and_drops_argument_members() {
    assert_eq!(
        eval_set("new Set([1,2,3]).difference(setLike([2])).values().toArray().join('-')"),
        "1-3"
    );
    assert_eq!(
        eval_set("new Set([1,2]).difference(setLike([1,2])).size"),
        "0"
    );
}

/// `symmetricDifference` yields the receiver-only elements and then the
/// argument-only ones, in that order (24.2.3.7).
#[test]
fn symmetric_difference_yields_this_then_argument() {
    assert_eq!(
        eval_set("new Set([1,2]).symmetricDifference(setLike([2,3])).values().toArray().join('-')"),
        "1-3"
    );
}

/// The three predicates return *primitive* booleans, so `=== true` holds, and
/// each has a true and a false case.
#[test]
fn set_predicates_return_primitive_booleans() {
    for (expr, want) in [
        ("new Set([1]).isSubsetOf(setLike([1,2]))", "true"),
        ("new Set([1,9]).isSubsetOf(setLike([1,2]))", "false"),
        ("new Set([1,2]).isSupersetOf(setLike([1]))", "true"),
        ("new Set([1]).isSupersetOf(setLike([1,2]))", "false"),
        ("new Set([1]).isDisjointFrom(setLike([2]))", "true"),
        ("new Set([1]).isDisjointFrom(setLike([1]))", "false"),
    ] {
        assert_eq!(eval_set(expr), want, "{expr}");
    }
    assert_eq!(
        eval_set("typeof new Set([1]).isSubsetOf(setLike([1]))"),
        "boolean"
    );
}

/// A result Set is an ordinary `Set`: `%Set.prototype%`, a live `size`, and the
/// inherited methods.
#[test]
fn a_result_set_is_a_real_set() {
    assert_eq!(
        eval_set("Object.getPrototypeOf(new Set([1]).union(setLike([2]))) === Set.prototype"),
        "true"
    );
    assert_eq!(eval_set("new Set([1,2]).union(setLike([])).size"), "2");
    assert_eq!(
        eval_set("typeof new Set([1]).difference(setLike([1])).has"),
        "function"
    );
    // The inherited method really is reachable through the linked prototype,
    // so the check above is not a detached method object.
    assert_eq!(eval_set("new Set([1]).union(setLike([2])).has(2)"), "true");
    assert_eq!(eval_set("new Set([1]).union(setLike([2])).size"), "2");
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

/// A string is not a valid set-relational argument at all.
///
/// This was originally `a_string_argument_iterates_by_code_point`, asserting
/// that `Set.prototype.union('c')` unions the string's characters and that a
/// surrogate pair counts as one. `GetSetRecord` step 1 requires an **Object**,
/// so both halves of that are false: a string argument is a `TypeError`
/// (`called-with-object.js`, seven method directories). The engine's *actual*
/// string iteration — `for (const c of str)` and `new Set(str)` — is
/// separately **not** code-point based; it yields code units, so a surrogate
/// pair is two elements. That is a pre-existing gap, not something this change
/// introduced (`StringIteratorPrototype` is 0/7 in conformance), and it is
/// asserted here at its real value so the deviation is named rather than
/// inherited silently from a test that was asserting the wrong thing about the
/// wrong code path.
#[test]
fn a_string_is_not_a_set_relational_argument() {
    for arg in ["'c'", "'\\u{1F600}'", "''"] {
        assert_eq!(
            eval_set(&format!(
                "(() => {{ try {{ new Set(['ab']).union({arg}); return 'NO THROW'; }} \
                   catch (e) {{ return e.constructor.name; }} }})()"
            )),
            "TypeError",
            "union({arg}) must be a TypeError"
        );
    }
    // The engine's real string iteration is by code unit, not code point. Named
    // here as a known deviation; see `StringIteratorPrototype` conformance.
    assert_eq!(
        eval_display(
            "(() => { var n = 0; for (const c of '\\u{1F600}') n++; \
                              return n; })()"
        ),
        "2",
        "KNOWN DEVIATION: for-of over a string yields code units, not code points"
    );
}

// ---------------------------------------------------------------------------
// Group 3 — Iterator helpers
// ---------------------------------------------------------------------------

/// `includes`/`join`/`windows`/`chunks` are prototype methods; `zip`/`zipKeyed`/
/// `concat` are *statics on the `Iterator` constructor*. The second half is the
/// observable that catches a misreading of the spec — putting them on the
/// prototype passes every behavioural test that only uses the prototype.
///
/// The converse ("and not also on the constructor") is deliberately not
/// asserted: this realm links `Iterator`'s `[[Prototype]]` to
/// `%Iterator.prototype%` rather than `%Function.prototype%`, so the prototype's
/// members read off the constructor. That is a pre-existing link defect in
/// `realm.rs`, not a property of this install.
#[test]
fn iterator_helpers_are_installed_on_the_right_objects() {
    for name in ["includes", "join", "windows", "chunks"] {
        assert_eq!(
            attr_triple_on("Iterator.prototype", &format!("'{name}'")),
            "true,false,true",
            "Iterator.prototype.{name}"
        );
        assert_eq!(
            eval_display(&format!("typeof Iterator.prototype.{name}")),
            "function"
        );
    }
    for name in ["zip", "zipKeyed", "concat"] {
        assert_eq!(
            attr_triple_on("Iterator", &format!("'{name}'")),
            "true,false,true",
            "Iterator.{name}"
        );
        assert_eq!(eval_display(&format!("typeof Iterator.{name}")), "function");
        assert_eq!(
            eval_display(&format!("typeof Iterator.prototype.{name}")),
            "undefined",
            "Iterator.{name} must not be a prototype method"
        );
    }
}

/// `zip`/`zipKeyed` take `(iterables, options)` and so report `length` 1;
/// `concat` is a rest parameter and reports 0. Getting this backwards still
/// *works* for `concat(a, b)`-shaped calls and only shows up in the arity.
#[test]
fn iterator_statics_declare_the_spec_arity() {
    assert_eq!(eval_display("Iterator.zip.length"), "1");
    assert_eq!(eval_display("Iterator.zipKeyed.length"), "1");
    assert_eq!(eval_display("Iterator.concat.length"), "0");
    assert_eq!(eval_display("Iterator.prototype.includes.length"), "1");
    assert_eq!(eval_display("Iterator.prototype.join.length"), "1");
    assert_eq!(eval_display("Iterator.prototype.windows.length"), "1");
    assert_eq!(eval_display("Iterator.prototype.chunks.length"), "1");
}

/// An iterator object inherits `%Iterator.prototype%`, so a helper reached only
/// by ordinary property lookup resolves. The interpreter's `Kind::Iterator`
/// method table synthesizes the older helpers by name, which masked this: a
/// method missing from that table read `undefined` on a real iterator while
/// being perfectly callable off the prototype.
#[test]
fn iterator_objects_inherit_the_iterator_prototype() {
    assert_eq!(
        eval_display("Object.getPrototypeOf([1,2].values()) === Iterator.prototype"),
        "true"
    );
    assert_eq!(
        eval_display("Object.getPrototypeOf(new Set([1]).values()) === Iterator.prototype"),
        "true"
    );
    assert_eq!(eval_display("typeof [1,2].values().includes"), "function");
    assert_eq!(eval_display("[1,2,3].values().includes(2)"), "true");
}

/// `includes` uses SameValueZero and short-circuits: the iterator is left
/// partially consumed at the match, not drained.
#[test]
fn includes_matches_same_value_zero_and_short_circuits() {
    assert_eq!(eval_display("[1,2,3].values().includes(2)"), "true");
    assert_eq!(eval_display("[1,2,3].values().includes(9)"), "false");
    // SameValueZero: `NaN` matches `NaN`.
    assert_eq!(eval_display("[NaN].values().includes(NaN)"), "true");
    // SameValueZero: `-0` matches `+0`.
    assert_eq!(eval_display("[-0].values().includes(0)"), "true");
    assert_eq!(eval_display("typeof [1].values().includes(1)"), "boolean");
    // The short circuit is observable: the values after the match are still
    // there for a later pull.
    assert_eq!(
        eval_display(
            "(() => { const it = [1,2,3,4,5].values(); it.includes(2);
               return it.toArray().join('-'); })()"
        ),
        "3-4-5"
    );
}

/// `join` applies ToString to each value and defaults the separator to `","`.
#[test]
fn join_stringifies_values_and_defaults_the_separator() {
    assert_eq!(eval_display("[1,2,3].values().join()"), "1,2,3");
    assert_eq!(eval_display("[1,2,3].values().join('-')"), "1-2-3");
    // An explicit `undefined` takes the default, per the optional parameter.
    assert_eq!(eval_display("[1,2].values().join(undefined)"), "1,2");
    assert_eq!(eval_display("[1,'a',true].values().join()"), "1,a,true");
    assert_eq!(eval_display("new Set([1,2]).values().join('|')"), "1|2");
    assert_eq!(eval_display("[].values().join()"), "");
    // An empty separator concatenates with nothing between.
    assert_eq!(eval_display("[1,2].values().join('')"), "12");
}

/// `windows(n)` yields overlapping groups of exactly `n`; `chunks(n)` yields
/// consecutive non-overlapping ones, and `chunks` keeps a short final group
/// while the default `windows` mode drops it.
#[test]
fn windows_overlaps_and_chunks_partitions() {
    assert_eq!(
        eval_display("JSON.stringify([1,2,3,4].values().windows(2).toArray())"),
        "[[1,2],[2,3],[3,4]]"
    );
    assert_eq!(
        eval_display("JSON.stringify([1,2,3,4,5].values().windows(3).toArray())"),
        "[[1,2,3],[2,3,4],[3,4,5]]"
    );
    assert_eq!(
        eval_display("JSON.stringify([1,2,3,4].values().chunks(2).toArray())"),
        "[[1,2],[3,4]]"
    );
    // A short final chunk is kept by `chunks`…
    assert_eq!(
        eval_display("JSON.stringify([1,2,3].values().chunks(2).toArray())"),
        "[[1,2],[3]]"
    );
    // …and dropped by the default `windows` undersized mode.
    assert_eq!(
        eval_display("JSON.stringify([1,2,3].values().windows(5).toArray())"),
        "[]"
    );
    // …unless `"allow-partial"` asks for it.
    assert_eq!(
        eval_display("JSON.stringify([1,2,3].values().windows(5, 'allow-partial').toArray())"),
        "[[1,2,3]]"
    );
    // A group size of 1 makes `windows` the identity, one fresh array each.
    assert_eq!(
        eval_display("JSON.stringify([1,2,3].values().windows(1).toArray())"),
        "[[1],[2],[3]]"
    );
}

/// Each window/chunk is a *distinct* array, not a view over a shared buffer —
/// mutating one must not be visible in the next.
#[test]
fn window_results_are_distinct_arrays() {
    assert_eq!(
        eval_display(
            "(() => { const w = [1,2,3].values().windows(2).toArray();
               w[0][0] = 99; return w[1][0]; })()"
        ),
        "2"
    );
}

/// Spec 27.1.4.8 steps 4-7: the size must *already* be a Number (never coerced),
/// integral, and inside `[1, 2^32-1]`. A non-Number or non-integral value is a
/// `TypeError`; an out-of-range Number is a `RangeError`. Using `ToNumber` here
/// — the obvious implementation — would make `'1'` succeed and `NaN` a
/// `RangeError`, both wrong.
#[test]
fn windows_and_chunks_validate_the_size_argument() {
    for name in ["windows", "chunks"] {
        for arg in [
            "undefined",
            "'1'",
            "true",
            "null",
            "{}",
            "NaN",
            "1.5",
            "Infinity",
        ] {
            assert_eq!(
                eval_display(&format!(
                    "(() => {{ try {{ [1,2].values().{name}({arg}); return 'NO THROW'; }}
                       catch (e) {{ return e.constructor.name; }} }})()"
                )),
                "TypeError",
                "{name}({arg})"
            );
        }
        for arg in ["0", "-0", "-1", "2 ** 32", "2 ** 53"] {
            assert_eq!(
                eval_display(&format!(
                    "(() => {{ try {{ [1,2].values().{name}({arg}); return 'NO THROW'; }}
                       catch (e) {{ return e.constructor.name; }} }})()"
                )),
                "RangeError",
                "{name}({arg})"
            );
        }
        // The boundary values are valid and must not throw.
        assert_eq!(
            eval_display(&format!("[1,2,3].values().{name}(1).toArray().length > 0")),
            "true",
            "{name}(1)"
        );
    }
}

/// The `undersized` option of `windows` is a closed set of two strings
/// (step 5-6); anything else is a `TypeError`.
#[test]
fn windows_rejects_an_invalid_undersized_option() {
    for arg in ["null", "''", "'something else'", "0", "true", "{}"] {
        assert_eq!(
            eval_display(&format!(
                "(() => {{ try {{ [1,2].values().windows(1, {arg}); return 'NO THROW'; }}
                   catch (e) {{ return e.constructor.name; }} }})()"
            )),
            "TypeError",
            "windows(1, {arg})"
        );
    }
    for arg in ["undefined", "'only-full'", "'allow-partial'"] {
        assert_eq!(
            eval_display(&format!(
                "[1,2,3].values().windows(1, {arg}).toArray().length"
            )),
            "3",
            "windows(1, {arg})"
        );
    }
}

/// A window/chunk result array carries `%Array.prototype%` and a correct
/// `length` — the `Ctx::alloc_array` contract, asserted because a hand-rolled
/// array here fails `result.constructor` far from its cause.
#[test]
fn window_results_are_real_arrays() {
    assert_eq!(
        eval_display(
            "Object.getPrototypeOf([1,2,3].values().windows(2).toArray()[0]) === Array.prototype"
        ),
        "true"
    );
    assert_eq!(
        eval_display("[1,2,3].values().windows(2).toArray()[0].length"),
        "2"
    );
    assert_eq!(
        eval_display("[1,2,3].values().chunks(2).toArray()[1].length"),
        "1"
    );
    assert_eq!(
        eval_display("[1,2,3,4,5].values().windows(3).toArray()[0].length"),
        "3"
    );
}

/// `toArray` (and therefore everything built on it) must return a real Array.
#[test]
fn to_array_results_are_real_arrays() {
    assert_eq!(
        eval_display("Object.getPrototypeOf([1,2].values().toArray()) === Array.prototype"),
        "true"
    );
    assert_eq!(eval_display("[1,2].values().toArray().length"), "2");
    assert_eq!(
        eval_display("typeof [1,2].values().toArray().map"),
        "function"
    );
}

/// `Iterator.zip` takes a *list* of iterables, yields one array per position,
/// and stops at the shortest input by default (spec 27.1.3.2).
#[test]
fn zip_stops_at_the_shortest_input() {
    assert_eq!(
        eval_display("JSON.stringify(Iterator.zip([[1,2,3],[10,20]]).toArray())"),
        "[[1,10],[2,20]]"
    );
    assert_eq!(
        eval_display("JSON.stringify(Iterator.zip([[1],[2,3]]).toArray())"),
        "[[1,2]]"
    );
    // `mode: "longest"` pads the short row with `undefined`, which
    // `JSON.stringify` renders as `null` inside an array.
    assert_eq!(
        eval_display("JSON.stringify(Iterator.zip([[1,2],[10]], {mode:'longest'}).toArray())"),
        "[[1,10],[2,null]]"
    );
    // Each row is a fresh, packed, real Array.
    assert_eq!(
        eval_display("Array.isArray(Iterator.zip([[1],[2]]).toArray()[0])"),
        "true"
    );
    assert_eq!(
        eval_display("Iterator.zip([[1],[2]]).toArray()[0].length"),
        "2"
    );
}

/// `zipKeyed` takes an *object* of iterables and yields one null-prototype
/// object per position, keyed by the input's own enumerable keys.
#[test]
fn zip_keyed_yields_null_prototype_objects_per_position() {
    assert_eq!(
        eval_display("JSON.stringify(Iterator.zipKeyed({a:[1,2], b:[3,4]}).toArray())"),
        "[{\"a\":1,\"b\":3},{\"a\":2,\"b\":4}]"
    );
    assert_eq!(
        eval_display("Object.getPrototypeOf(Iterator.zipKeyed({a:[1]}).toArray()[0]) === null"),
        "true"
    );
    // A non-enumerable own key is excluded (the spec's `[[GetOwnProperty]]`
    // filter), and an inherited key never appears.
    assert_eq!(
        eval_display(
            "(() => { const input = Object.create({inherited:[9]});
               input.own = [1];
               Object.defineProperty(input, 'hidden', {value:[2], enumerable:false});
               return Object.keys(Iterator.zipKeyed(input).toArray()[0]).join(','); })()"
        ),
        "own"
    );
    // `undefined` values are skipped entirely.
    assert_eq!(
        eval_display("Object.keys(Iterator.zipKeyed({a:[1], b:undefined}).toArray()[0]).length"),
        "1"
    );
}

/// The three statics return real iterators, not bare objects.
#[test]
fn iterator_statics_return_real_iterators() {
    for expr in [
        "Iterator.zip([[1,2],[3,4]])",
        "Iterator.zipKeyed({a:[1]})",
        "Iterator.concat([1],[2])",
    ] {
        assert_eq!(
            eval_display(&format!(
                "Object.getPrototypeOf({expr}) === Iterator.prototype"
            )),
            "true",
            "{expr}"
        );
        assert_eq!(
            eval_display(&format!("typeof {expr}.next")),
            "function",
            "{expr}"
        );
    }
}

/// `Iterator.concat(...items)` is the rest-parameter one: every input, in
/// argument order.
#[test]
fn concat_chains_inputs_in_argument_order() {
    assert_eq!(
        eval_display("Iterator.concat([1,2],[3],[4,5]).toArray().join('-')"),
        "1-2-3-4-5"
    );
    assert_eq!(eval_display("Iterator.concat().toArray().length"), "0");
    assert_eq!(
        eval_display("Iterator.concat(...[[1],[2,3]]).toArray().join('-')"),
        "1-2-3"
    );
}

// ---------------------------------------------------------------------------
// Group 4 — keyed promise combinators
// ---------------------------------------------------------------------------

/// Both statics install with the spec attributes and `length` 1.
#[test]
fn keyed_promise_combinators_are_installed() {
    for name in ["allKeyed", "allSettledKeyed"] {
        assert_eq!(
            attr_triple_on("Promise", &format!("'{name}'")),
            "true,false,true",
            "Promise.{name}"
        );
        assert_eq!(eval_display(&format!("typeof Promise.{name}")), "function");
    }
    assert_eq!(eval_display("Promise.allKeyed.length"), "1");
    assert_eq!(eval_display("Promise.allSettledKeyed.length"), "1");
}

/// The argument is an **object**, and the fulfillment value is a *null-prototype*
/// object keyed by its own enumerable string keys (27.2.4.4 /
/// `CreateKeyedPromiseCombinatorResultObject`).
///
/// `eval_then` reads the settled value: without the job drain the
/// callback would not have run and the assertion would pass on `pending`.
#[test]
fn all_keyed_resolves_with_a_null_prototype_object() {
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allKeyed({first: 1, second: 2}).then(r => {
               out = (Object.getPrototypeOf(r) === null ? 'null-proto' : 'has-proto')
                     + '|' + Object.keys(r).join('-') + '|' + r.first + ',' + r.second; });",
            "out",
        ),
        "null-proto|first-second|1,2"
    );
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allKeyed({}).then(r => { out = String(Object.keys(r).length); });",
            "out",
        ),
        "0"
    );
}

/// Each result property is a data property with default attributes, and the
/// whole object is extensible.
#[test]
fn all_keyed_result_properties_are_plain_data_properties() {
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allKeyed({a: 1}).then(r => {
               const d = Object.getOwnPropertyDescriptor(r, 'a');
               out = [d.writable, d.enumerable, d.configurable].join(',')
                     + '|' + Object.isExtensible(r); });",
            "out",
        ),
        "true,true,true|true"
    );
}

/// An inherited key is not an element and a non-enumerable own key is not
/// either: the spec filters on `[[OwnPropertyKeys]]` then
/// `[[GetOwnProperty]].[[Enumerable]]`, in that order.
#[test]
fn all_keyed_ignores_inherited_and_non_enumerable_keys() {
    assert_eq!(
        eval_then(
            "var out = 'pending';
             const input = Object.create({inherited: 1});
             input.own = 2;
             Object.defineProperty(input, 'hidden', {value: 3, enumerable: false});
             Promise.allKeyed(input).then(r => { out = Object.keys(r).join(','); });",
            "out",
        ),
        "own"
    );
}

/// A promise value is unwrapped and its payload lands under the key.
#[test]
fn all_keyed_unwraps_promise_values() {
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allKeyed({a: Promise.resolve(5)}).then(r => { out = String(r.a); });",
            "out",
        ),
        "5"
    );
}

/// A non-object argument **rejects** the returned promise; it does not throw
/// synchronously (`arg-not-object-reject.js`). Both halves are asserted, since
/// an implementation that threw would leave `out` at `pending` and the
/// throw-half would show it.
#[test]
fn all_keyed_rejects_a_non_object_argument_instead_of_throwing() {
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allKeyed(5).then(() => { out = 'fulfilled'; },
                                () => { out = 'rejected'; });",
            "out",
        ),
        "rejected"
    );
    // And the call itself did not throw — a synchronous throw would surface
    // here instead.
    assert_eq!(
        eval_display(
            "(() => { try { Promise.allKeyed(undefined); return 'no sync throw'; } catch (e) { return 'threw'; } })()"
        ),
        "no sync throw"
    );
}

/// `allSettledKeyed` wraps each element in a `{status, value}` record and never
/// rejects.
#[test]
fn all_settled_keyed_wraps_each_element_in_a_status_record() {
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allSettledKeyed({a: 1}).then(r => { out = r.a.status + ':' + r.a.value; });",
            "out",
        ),
        "fulfilled:1"
    );
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allSettledKeyed({}).then(r => {
               out = (Object.getPrototypeOf(r) === null ? 'null-proto' : 'has-proto'); });",
            "out",
        ),
        "null-proto"
    );
}

/// A rejected element is a `rejected` record, and the aggregate still fulfils.
#[test]
fn all_settled_keyed_records_a_rejection_without_rejecting() {
    assert_eq!(
        eval_then(
            "var out = 'pending';
             Promise.allSettledKeyed({a: Promise.reject(new Error('x'))}).then(
               r => { out = r.a.status; },
               () => { out = 'AGGREGATE REJECTED'; });",
            "out",
        ),
        "rejected"
    );
}
