//! Regression tests for the `Iterator.prototype` helper bodies that live in
//! `v12_engine::builtins::iterator`.
//!
//! Three defects are covered, each of which was a *wrong value* rather than a
//! missing method — so none of them shows up as a `TypeError: callee is not a
//! function` and each one silently answered the wrong thing:
//!
//! 1. **`take`/`drop` limit validation.** The spec (27.1.4.3 steps 6-9) has
//!    three distinct `RangeError` conditions before the integer limit exists:
//!    `NaN` (which is what `take()` and `take(undefined)` produce, because
//!    `ToNumber(undefined)` is `NaN`, *not* 0), a finite value above
//!    `2**53 - 1`, and a negative truncated limit. All three were clamped to 0
//!    instead, so `take(-1)` built an empty iterator where it must throw.
//!    `-0.5` and `Infinity` are the two counter-cases that must *not* throw and
//!    are pinned here, because the natural over-correction (`reject any
//!    negative`, `reject any non-finite`) breaks both.
//!
//! 2. **`Iterator.prototype.join` nullish contents.** A `null` or `undefined`
//!    yielded by the iterator contributes the *empty* string (step 7), so
//!    `[null, 2].join()` is `",2"`. It used to be stringified as
//!    `"null"`/`"undefined"`, which is the wrong *content*, not a wrong type.
//!
//! 3. **`CreateIteratorResultObject`'s prototype.** Every iterator's `next`
//!    result object is created by one shared helper in this file, so linking it
//!    to `%Object.prototype%` is a single edit that repairs the result shape for
//!    the Array, Map, Set and Iterator iterators alike. It was allocated with no
//!    prototype at all, which is why `Object.getPrototypeOf(it.next())` answered
//!    `null` everywhere.
//!
//! The `zip` strict-mode tests are the fourth area: `"strict"` is a valid
//! `options.mode` that the engine rejected outright. Accepting the string
//! without implementing the check would have been worse than the rejection
//! (unequal input lengths must *throw*, not quietly produce a short zip), so
//! the fault is encoded as its own iterator kind and asserted here directly.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// The error *class name* a snippet threw, or `Err("no-throw")` when it did not
/// throw.
///
/// `to_display_string` renders a thrown value as `"<Class>: <message>"`, so the
/// class is the text before the first `": "`. Separating "threw the right
/// thing" from "threw something" is the whole point of the `RangeError` cases:
/// both a correct `RangeError` and a stray `TypeError` are "it threw", and only
/// the class name tells them apart.
fn eval_throw_kind(src: &str) -> Result<String, String> {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(_) => Err("no-throw".to_string()),
        Err(thrown) => {
            let text = engine.to_display_string(thrown);
            let class = text.split(": ").next().unwrap_or("").to_string();
            Ok(class)
        }
    }
}

// -- 1. take/drop limit validation ---------------------------------------------

#[test]
fn take_rejects_nan_limit() {
    // `take()` and `take(undefined)` are the NaN arm: `ToNumber(undefined)` is
    // NaN, so an absent argument is an error rather than a take-nothing.
    assert_eq!(eval_throw_kind("[1,2,3].values().take()"), Ok("RangeError".into()));
    assert_eq!(
        eval_throw_kind("[1,2,3].values().take(undefined)"),
        Ok("RangeError".into())
    );
    assert_eq!(
        eval_throw_kind("[1,2,3].values().take(NaN)"),
        Ok("RangeError".into())
    );
}

#[test]
fn take_rejects_negative_limit() {
    assert_eq!(
        eval_throw_kind("[1,2,3].values().take(-1)"),
        Ok("RangeError".into())
    );
    assert_eq!(
        eval_throw_kind("[1,2,3].values().drop(-1)"),
        Ok("RangeError".into())
    );
}

#[test]
fn take_rejects_limit_above_max_safe_integer() {
    assert_eq!(
        eval_throw_kind("[1,2,3].values().take(Number.MAX_SAFE_INTEGER + 1)"),
        Ok("RangeError".into())
    );
    // The boundary itself is valid: step 7 rejects only *strictly* above it.
    assert_eq!(
        eval_display("[1,2,3].values().take(Number.MAX_SAFE_INTEGER).toArray()"),
        "1,2,3"
    );
}

#[test]
fn take_accepts_the_two_negative_and_infinite_counter_cases() {
    // `-0.5` truncates to `-0`, and `-0 < 0` is false, so this is *not* the
    // negative arm: it takes nothing rather than throwing.
    assert_eq!(eval_display("[1,2,3].values().take(-0.5).toArray()"), "");
    // `Infinity` is not finite, so it escapes step 7, and it is not negative, so
    // it escapes step 9: an unbounded take.
    assert_eq!(
        eval_display("[1,2,3].values().take(Infinity).toArray()"),
        "1,2,3"
    );
    // `take(null)` is `take(0)`, not an unbounded take: `ToNumber(null)` is 0.
    assert_eq!(eval_display("[1,2,3].values().take(null).toArray()"), "");
    assert_eq!(eval_display("[1,2,3].values().take(0).toArray()"), "");
}

#[test]
fn take_still_clamps_normal_limits() {
    // The passing behaviour the validation must not disturb.
    assert_eq!(eval_display("[1,2,3,4].values().take(2).toArray()"), "1,2");
    assert_eq!(eval_display("[1,2,3,4].values().drop(2).toArray()"), "3,4");
    assert_eq!(eval_display("[1,2,3].values().take(99).toArray()"), "1,2,3");
    assert_eq!(eval_display("[].values().take(1).toArray()"), "");
}

// -- 2. join nullish contents --------------------------------------------------

#[test]
fn join_renders_nullish_contents_as_empty_fields() {
    // The whole point: an empty *field*, not the text "null".
    assert_eq!(
        eval_display("['one', null, 'two', undefined].values().join()"),
        "one,,two,"
    );
    assert_eq!(
        eval_display("[null, undefined, 2].values().join()"),
        ",,2"
    );
    // A non-default separator still applies between the empty fields.
    assert_eq!(eval_display("[null, 'a', undefined].values().join('-')"), "-a-");
    // nullish contents do not shorten the result, unlike the pre-fix behaviour.
    assert_eq!(eval_display("[null].values().join()"), "");
}

#[test]
fn join_still_stringifies_ordinary_contents() {
    assert_eq!(eval_display("['a','b'].values().join()"), "a,b");
    assert_eq!(eval_display("[1,2].values().join('-')"), "1-2");
    assert_eq!(eval_display("[true,null].values().join()"), "true,");
}

// -- 3. iterator result prototype ----------------------------------------------

#[test]
fn iterator_result_inherits_object_prototype() {
    // Every `next` result flows through one shared constructor, so this covers
    // the Array, Iterator and Set iterators at once.
    assert_eq!(
        eval_display(
            "Object.getPrototypeOf([1].values().next()) === Object.prototype"
        ),
        "true"
    );
    assert_eq!(
        eval_display(
            "Object.getPrototypeOf([1,2].entries().next()) === Object.prototype"
        ),
        "true"
    );
    assert_eq!(
        eval_display(
            "Object.getPrototypeOf(new Set([1]).values().next()) === Object.prototype"
        ),
        "true"
    );
    // The done result is built by the same constructor, so it is not a special
    // case that could have kept the missing link.
    let exhausted = "var it = [1].values(); it.next();\
                     Object.getPrototypeOf(it.next()) === Object.prototype";
    assert_eq!(eval_display(exhausted), "true");
}

#[test]
fn iterator_result_shape_is_value_then_done() {
    // `CreateIteratorResultObject` installs exactly these two keys, in order,
    // and nothing else.
    assert_eq!(
        eval_display("Reflect.ownKeys([1].values().next()).join()"),
        "value,done"
    );
    assert_eq!(eval_display("[1].values().next().value"), "1");
    assert_eq!(eval_display("[1].values().next().done"), "false");
    assert_eq!(
        eval_display("Object.isExtensible([1].values().next())"),
        "true"
    );
}

// -- 4. zip strict mode --------------------------------------------------------

#[test]
fn zip_accepts_every_valid_mode() {
    // `"strict"` used to be rejected as an invalid mode, which is what
    // `options-mode.js` pins. Empty inputs mean no column is unequal, so there
    // is no fault to observe here.
    for mode in ["undefined", "'shortest'", "'longest'", "'strict'"] {
        let src = format!("Iterator.zip([], {{ mode: {mode} }}) instanceof Iterator");
        assert_eq!(eval_display(&src), "true", "mode {mode} must be accepted");
    }
}

#[test]
fn zip_strict_yields_rows_then_throws_on_unequal_columns() {
    // The unequal case must *throw*, not quietly produce the short zip. Accepting
    // the mode string without this check is the failure mode this pins.
    // One row is produced (the shorter column has one element), and the call
    // after that faults. Written as explicit steps rather than a drain loop
    // because draining to completion *is* the throw in this mode. The row's
    // elements are indexed rather than concatenated: `String(row)` is a
    // `ToPrimitive` on an object, which is the engine's known re-entrancy
    // boundary, not something this mode decides.
    let src = "var it = Iterator.zip([[1,2],[3]], { mode: 'strict' });\
               var first = it.next();\
               first.done + ':' + first.value.length + ':' + first.value[0];";
    assert_eq!(eval_display(src), "false:2:1");
    let throw_src = "var it = Iterator.zip([[1,2],[3]], { mode: 'strict' });\
                     it.next(); it.next();";
    assert_eq!(eval_throw_kind(throw_src), Ok("TypeError".into()));
    // And it keeps faulting: a strict zip is closed by the throw, so a later
    // `next` must not report a clean `done`.
    let again = "var it = Iterator.zip([[1,2],[3]], { mode: 'strict' });\
                 it.next(); it.next();";
    assert_eq!(eval_throw_kind(again), Ok("TypeError".into()));
}

#[test]
fn zip_strict_completes_normally_on_equal_columns() {
    // Equal columns have nothing to fault on, so the iterator completes instead
    // of throwing — the other half of the same mode.
    assert_eq!(
        eval_display(
            "Iterator.zip([[1,2],[3,4]], { mode: 'strict' }).next().done"
        ),
        "false"
    );
    assert_eq!(
        eval_display(
            "Iterator.zip([[1,2],[3,4]], { mode: 'strict' }).toArray().length"
        ),
        "2"
    );
    let done_src = "var it = Iterator.zip([[1,2],[3,4]], { mode: 'strict' });\
                    it.next(); it.next(); it.next().done";
    assert_eq!(eval_display(done_src), "true");
}

#[test]
fn zip_longest_pads_with_the_padding_option() {
    // `padding` is a separate option consulted only in `"longest"` mode, and it
    // defaults to `undefined` — which is *not* the same as the pre-fix `""`.
    assert_eq!(
        eval_display("Iterator.zip([[1,2],[3]], { mode: 'longest' }).toArray()"),
        "1,3,2,"
    );
    assert_eq!(
        eval_display(
            "Iterator.zip([[1,2],[3]], { mode: 'longest', padding: 0 }).toArray()"
        ),
        "1,3,2,0"
    );
    // Shortest is unaffected by `padding` entirely.
    assert_eq!(
        eval_display("Iterator.zip([[1,2],[3]], { padding: 0 }).toArray()"),
        "1,3"
    );
}



// -- JSON raw JSON: the stringify half ----------------------------------------
//
// `JSON.rawJSON` / `JSON.isRawJSON` themselves are NOT installed — both need a
// `NativeId` in `v12-native/src/id.rs` and an entry in the `JSON` block of
// `builtins/mod.rs`, both of which are outside this lane's file ownership. The
// bodies (`json::json_raw_json`, `json::json_is_raw_json`) are written and
// compile; only the wiring is missing.
//
// What IS live without any wiring is the *consuming* half: the
// `SerializeJSONProperty` splice, because it is recognised structurally. These
// tests therefore build a backdoor by hand — a lone `rawJSON` own property —
// which is exactly the shape `JSON.rawJSON` produces and exactly the shape
// `json::raw_json_of` looks for.

#[test]
fn a_lone_rawjson_property_is_spliced_verbatim() {
    // Not quoted, not escaped: the text goes into the result as-is.
    assert_eq!(
        eval_display("var o = {}; o.rawJSON = '{\"x\":1}'; JSON.stringify(o)"),
        "{\"x\":1}"
    );
    assert_eq!(
        eval_display("JSON.stringify([{ rawJSON: '[1,2]' }, 3])"),
        "[[1,2],3]"
    );
}

#[test]
fn an_ordinary_object_with_a_rawjson_key_is_not_treated_as_raw() {
    // The safety property, and the reason the check is "exactly one own
    // enumerable property" rather than "has a rawJSON property": a second key
    // makes it an ordinary object that gets escaped like any other.
    assert_eq!(
        eval_display("JSON.stringify({ rawJSON: '{\"x\":1}', other: 2 })"),
        "{\"rawJSON\":\"{\\\"x\\\":1}\",\"other\":2}"
    );
    // A `rawJSON` key whose value is not a String is likewise ordinary.
    assert_eq!(
        eval_display("JSON.stringify({ rawJSON: 1 })"),
        "{\"rawJSON\":1}"
    );
}

#[test]
fn raw_json_text_is_not_validated_on_the_way_out() {
    // `JSON.rawJSON` only rejects control characters; everything else is the
    // caller's responsibility. Emitting `nope` verbatim and then failing to
    // parse it is the documented round-trip behaviour, not a bug — and it is
    // what shows the splice really is verbatim rather than a re-serialization.
    assert_eq!(
        eval_throw_kind("JSON.parse(JSON.stringify({ rawJSON: 'nope' }))"),
        Ok("SyntaxError".into())
    );
}
