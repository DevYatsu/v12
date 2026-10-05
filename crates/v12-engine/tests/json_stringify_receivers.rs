//! `JSON.stringify` over array-like receivers: `LengthOfArrayLike` (J1) and
//! the element-store key rung (J2).
//!
//! # What these pin, and why they were invisible
//!
//! Both defects are in `crates/v12-engine/src/builtins/json.rs` and both are
//! **bypasses of machinery that already existed and was already correct**:
//!
//! * **J1** — [`write_array`] walked `elements_snapshot()`, the element
//!   *lattice*, instead of `LengthOfArrayLike(value)`. A holey array whose
//!   `length` is 5 with four present entries therefore serialised four entries.
//!   The bound is the spec's step 3 of `SerializeJSONArray`; the store is not.
//! * **J2** — [`enumerable_own_keys`] consulted the shape descriptors and the
//!   dictionary overflow and **never the element store**, so a
//!   `Kind::Arguments` object — whose indices live in the store, not in the
//!   shape — serialised as `{}`. The correct reader is
//!   `collect_enumerable_string_keys` (`builtins/object.rs`), which branches on
//!   `Kind::Array || Kind::Arguments` for exactly this reason. `json.rs` had
//!   its own, narrower copy of "the own enumerable string keys".
//!
//! That is also why neither defect is visible in the ADR-0004 generated table:
//! `array_generic_receivers.rs` names J1 as the reason `JSON.stringify` is
//! **deliberately not the observer** there (its "Deliberately NOT covered here"
//! section) — the table is a `Array.prototype` table, so it cannot see a
//! serializer bug. These tests are the instrument for the serializer.
//!
//! # The oracle
//!
//! Every expected value below is `node`'s own output for the same program
//! (node v24.21.0), captured by generating them, not by hand. `node` is the
//! authority on the two spec questions here:
//!
//! * `JSON.stringify(arguments)` with 2+ populated indices is an **OBJECT**,
//!   `{"0":1,"1":2}` — not an array and not `{}`. `arguments` is not an
//!   Array, so `SerializeJSONProperty` never takes the array branch; the
//!   indices are ordinary own enumerable data properties
//!   (`Object.keys` reports them, and reports them in v12 too), which is
//!   precisely why `{}` was wrong.
//! * `JSON.stringify([1,,3])` is `[1,null,3]`: holes become `null` inside an
//!   array, so a holey array is *not* shorter, only null-filled.

use v12_engine::Engine;

fn eval_display(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

// -- J1: arrays serialise from `length`, not from the element lattice ----------

/// The defect itself: `length` is 5, only two entries are present, and the
/// answer must still have five slots.
#[test]
fn holey_array_serialises_every_index_up_to_length() {
    assert_eq!(
        eval_display(
            "var a = ['a']; a[3] = 'd'; a.length = 5; \
             a.length + ' ' + JSON.stringify(a)"
        ),
        "5 [\"a\",null,null,\"d\",null]"
    );
}

/// A hole *below* the highest present index — the case a lattice walk gets
/// right by accident and a `length` walk must too.
#[test]
fn hole_below_the_last_present_index_is_null_not_a_shortened_array() {
    assert_eq!(
        eval_display("var a = ['a']; a[2] = 'c'; a.length = 4; JSON.stringify(a)"),
        "[\"a\",null,\"c\",null]"
    );
}

/// `new Array(5)` has an empty lattice and a `length` of 5: zero present
/// entries, five serialised slots. This is the sharpest form of the defect,
/// because a lattice walk returns `[]` and an extent-based walk returns `[null]`
/// — only the declared `length` gives all five.
#[test]
fn a_constructor_sized_array_serialises_length_n_not_the_empty_lattice() {
    assert_eq!(
        eval_display("JSON.stringify(new Array(5))"),
        "[null,null,null,null,null]"
    );
}

/// One entry written into a `new Array(3)`: `length` stays 3.
#[test]
fn a_partially_filled_constructor_array_keeps_its_declared_length() {
    assert_eq!(
        eval_display("var a = new Array(3); a[0] = 'z'; JSON.stringify(a)"),
        "[\"z\",null,null]"
    );
}

/// A trailing write extends `length`, and the serialised form follows it.
#[test]
fn a_trailing_index_write_extends_the_serialised_form() {
    assert_eq!(
        eval_display("var a = [1]; a[3] = 4; JSON.stringify(a)"),
        "[1,null,null,4]"
    );
}

/// Lowering `length` must *shorten* the serialised form. This is the mirror of
/// the defect: a lattice walk ignores `length` in both directions, so a fix
/// that only ever pads upward would pass the tests above and fail this one.
#[test]
fn lowering_length_shortens_the_serialised_form() {
    assert_eq!(
        eval_display("var a = [1,2,3]; a.length = 1; JSON.stringify(a)"),
        "[1]"
    );
}

// -- J2: `arguments` is serialised as an object with its own indices ----------

/// The defect itself, and the spec answer it is asserting: an **object**, keyed
/// by stringified index, in ascending numeric order.
#[test]
fn a_populated_arguments_object_serialises_its_indices_as_an_object() {
    assert_eq!(
        eval_display(
            "function f(a, b, c) { return JSON.stringify(arguments); } \
             f(1, 'b', true)"
        ),
        "{\"0\":1,\"1\":\"b\",\"2\":true}"
    );
}

/// Two indices, so the answer cannot be confused with a one-entry coincidence.
#[test]
fn two_populated_arguments_indices_are_both_serialised() {
    assert_eq!(
        eval_display("function f(a, b) { return JSON.stringify(arguments); } f(1, 2)"),
        "{\"0\":1,\"1\":2}"
    );
}

/// An empty `arguments` is legitimately `{}` — pinned so the J2 fix cannot be
/// satisfied by making the object path emit something unconditionally.
#[test]
fn an_empty_arguments_object_is_still_an_empty_object() {
    assert_eq!(
        eval_display("function f() { return JSON.stringify(arguments); } f()"),
        "{}"
    );
}

/// `length` and `callee` are own but **non-enumerable**, so they must not
/// appear. This is the guard against a fix that walks *all* own keys instead
/// of own *enumerable* ones.
#[test]
fn arguments_length_and_callee_are_not_serialised() {
    assert_eq!(
        eval_display(
            "function f(a, b) { return JSON.stringify(arguments); } \
             f(1, 2).indexOf('length') + ' ' + \
             f(1, 2).indexOf('callee')"
        ),
        "-1 -1"
    );
}

// -- regression guards: plain arrays and objects must not move ---------------

#[test]
fn plain_arrays_and_objects_are_unchanged() {
    assert_eq!(eval_display("JSON.stringify([1,2,3])"), "[1,2,3]");
    assert_eq!(eval_display("JSON.stringify([1,,3])"), "[1,null,3]");
    assert_eq!(
        eval_display("JSON.stringify({a:1,b:[true,null]})"),
        "{\"a\":1,\"b\":[true,null]}"
    );
    assert_eq!(
        eval_display("JSON.stringify({a:[1,{b:[2]}]})"),
        "{\"a\":[1,{\"b\":[2]}]}"
    );
    assert_eq!(
        eval_display("JSON.stringify({a:undefined,b:function(){},c:1})"),
        "{\"c\":1}"
    );
    assert_eq!(
        eval_display("JSON.stringify([1,undefined,function(){},2])"),
        "[1,null,null,2]"
    );
    assert_eq!(
        eval_display("JSON.stringify([]) + '|' + JSON.stringify({})"),
        "[]|{}"
    );
    assert_eq!(
        eval_display("JSON.stringify([NaN,Infinity,-Infinity])"),
        "[null,null,null]"
    );
}

/// A hole at depth, i.e. the J1 fix applied at a non-top level, and inside a
/// nested object. Catches a fix that only repaired the top-level call.
#[test]
fn holes_are_null_at_every_depth() {
    assert_eq!(
        eval_display("JSON.stringify({a:[1,,3]})"),
        "{\"a\":[1,null,3]}"
    );
    assert_eq!(
        eval_display("JSON.stringify([[1,,2],{b:[3,,4]}])"),
        "[[1,null,2],{\"b\":[3,null,4]}]"
    );
}

/// `undefined` / function / symbol are not representable at the top level
/// either, and still yield `undefined` rather than a string.
#[test]
fn unrepresentable_tops_still_yield_undefined() {
    assert_eq!(
        eval_display("String(JSON.stringify(undefined))"),
        "undefined"
    );
    assert_eq!(
        eval_display("String(JSON.stringify(function(){}))"),
        "undefined"
    );
    assert_eq!(
        eval_display("String(JSON.stringify(Symbol('s')))"),
        "undefined"
    );
}

/// A **plain object** carrying a numeric `length` is not an Array and must
/// serialise as an object including its `length` key. This is the nearest
/// neighbour to the J1 fix and the row most likely to regress from it: if
/// `write_array` were reached by "has a numeric length" instead of
/// "IsArray(value)", this row would become `["a","c"]`.
#[test]
fn a_plain_object_with_a_numeric_length_is_still_an_object() {
    assert_eq!(
        eval_display("var o = {0:'a',2:'c',length:4}; JSON.stringify(o)"),
        "{\"0\":\"a\",\"2\":\"c\",\"length\":4}"
    );
}

/// Same for a plain object whose `length` is a *number* the JSON layer would
/// otherwise happily iterate: the array branch is `IsArray`, not
/// "looks array-like".
#[test]
fn a_plain_array_like_object_is_not_serialised_as_an_array() {
    assert_eq!(
        eval_display("var o = {0:'a',1:'b',length:2}; JSON.stringify(o)"),
        "{\"0\":\"a\",\"1\":\"b\",\"length\":2}"
    );
}

/// The `space` / indent path shares `write_array`, so a holey array under
/// indentation must still have `length` slots.
#[test]
fn the_gap_parameter_does_not_restore_the_lattice_length() {
    assert_eq!(
        eval_display("var a = ['a']; a.length = 3; JSON.stringify(a, null, 1)"),
        "[\n \"a\",\n null,\n null\n]"
    );
}

/// The array `replacer` (`PropertyList`) governs *nested objects*, not an
/// array's own indices — `SerializeJSONArray` steps 5-7 take every index from
/// `0` to `len`. So a holey array under a replacer keeps its `length`-derived
/// shape, and a dense one is unaffected. (Verified against node, which answers
/// `["a",null,null]` and `["a","b"]` respectively.)
#[test]
fn the_replacer_array_form_does_not_shorten_an_array() {
    assert_eq!(
        eval_display(
            "var a = ['a']; a.length = 3; \
             JSON.stringify(a, ['0']) + '|' + JSON.stringify(['a','b'], ['0'])"
        ),
        "[\"a\",null,null]|[\"a\",\"b\"]"
    );
}

/// The `PropertyList` branch of `write_object` is a **third** copy of the own-
/// key lookup, separate from `enumerable_own_keys`, and it was bypassed by the
/// element store too: node answers `{"1":2}` for an `arguments` under a
/// one-name replacer, and `{}` is what the shape-only lookup produced.
#[test]
fn the_replacer_array_form_reaches_arguments_indices() {
    assert_eq!(
        eval_display("function f(a, b) { return JSON.stringify(arguments, ['1']); } f(1, 2)"),
        "{\"1\":2}"
    );
}
