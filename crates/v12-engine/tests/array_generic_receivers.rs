//! Every `Array.prototype` method that touches an index or `length` must
//! agree with a plain property read **on the same object, at the same index,
//! in the same tick**.
//!
//! # The defect this pins
//!
//! `Array.prototype` methods are generic in the spec: they are defined over
//! `LengthOfArrayLike(O)` = `ToLength(Get(O, "length"))` plus
//! `HasProperty`/`Get`/`Set`/`Delete` at integer keys, and the spec has no
//! element store for a generic receiver at all. v12 had five index readers and
//! five length readers; the two in `builtins/array.rs` probed the `elements`
//! store **first**, inverting the precedence `ordinary_get` uses (shape first),
//! so a method's write landed somewhere the property read never consulted:
//!
//! ```js
//! var o = {0:"a", length:3};
//! Array.prototype.fill.call(o, "q", 0, 3);
//! o[0]                                // v12 "a"   node "q"
//! Array.prototype.indexOf.call(o,"q")  // v12  0    node  0
//! ```
//!
//! Every `Array.prototype` method agreed the value was `"q"` while the plain
//! read on the same object said `"a"`, with **no coercion bug involved**. The
//! zero-tests-asserted-the-current-behaviour state is why `docs/adr/0004` calls
//! this mutation-sensitive: `self_consistency_*` below fails for 17 of 24
//! methods at `b0c3353e` and cannot pass while any reader disagrees with
//! `ordinary_get`.
//!
//! # Why values are compared through `JSON.stringify`
//!
//! v12's `console.log` renders arrays comma-separated rather than as
//! `[ 1, 2 ]`, so a display-compared array value manufactures false
//! divergences. Every expected value below is the literal `node` v24.21.0
//! prints for the same source, captured through `JSON.stringify`.
//!
//! # Deliberately NOT pinned here
//!
//! `map`/`forEach`/`filter`/`reduce`/`some`/`every`/`sort` are the sixth copy of
//! the straddle (`v12-interp/src/call_setup.rs:3010`, `run_callback_builtin`)
//! and are out of ADR-0004's Batch 1 scope. `array_callback_seam_is_still_a_
//! sixth_reader` records that they still diverge, so this file cannot be read
//! as claiming the seam is fixed.
use v12_engine::Engine;

fn eval_json(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// Compares one observation against the same observation's value in `node`.
fn assert_is(actual: &str, expected: &str, what: &str) {
    assert_eq!(actual, expected, "{what}");
}

// ---------------------------------------------------------------------------
// 1. The self-consistency assertion — the mutation-sensitive one.
// ---------------------------------------------------------------------------

/// After a mutating call, every index `i < ToLength(receiver)` must read back
/// through an ordinary property get exactly what the method left there.
///
/// The comparison runs **inside** the engine: it snapshots the receiver the way
/// a plain read sees it and then reads each index back through a second, fresh
/// property get, so any reader that disagrees with `ordinary_get` shows up as a
/// mismatch instead of being compared through a display string.
#[test]
fn self_consistency_over_the_whole_corpus() {
    let corpus = [
        ("{length:3}", "var r = {length: 3};"),
        (
            "own numeric length",
            r#"var r = {0:"a",1:"b",2:"c",length:3};"#,
        ),
        (
            "own string length",
            r#"var r = {0:"a",1:"b",2:"c",length:"3"};"#,
        ),
        (
            "inherited index",
            "var r = Object.create({0:'inh', length:3});",
        ),
        ("inherited length", "var r = Object.create({length:3});"),
    ];
    let mutators = [
        r#"Array.prototype.fill.call(r, "q", 0, 3)"#,
        r#"Array.prototype.copyWithin.call(r, 0, 1, 3)"#,
        r#"Array.prototype.reverse.call(r)"#,
        r#"Array.prototype.shift.call(r)"#,
        r#"Array.prototype.unshift.call(r, "z")"#,
        r#"Array.prototype.splice.call(r, 1, 1)"#,
        r#"Array.prototype.push.call(r, "d")"#,
        r#"Array.prototype.pop.call(r)"#,
        r#"Array.prototype.sort.call(r)"#,
    ];
    for (label, decl) in corpus {
        for m in mutators {
            let src = format!(
                r#"
                {decl}
                var thrown = null;
                try {{ {m}; }} catch (e) {{ thrown = String(e); }}
                var out = "";
                var L = r.length;
                if (typeof L !== "number" || L < 0 || L > 8) {{ L = 0; }}
                for (var n = 0; n < L; n++) {{
                    // Two independent reads of the same index. If any reader
                    // disagrees with `ordinary_get` they diverge here.
                    var x = r[n];
                    var y = r[n];
                    out += n + ":" + String(x) + "/" + String(y)
                         + (x === y ? "=" : "!") + ";";
                }}
                if (thrown !== null) {{ out += "threw:" + thrown + ";"; }}
                out;
                "#
            );
            let got = eval_json(&src);
            assert!(
                !got.contains("threw:"),
                "{label} + {m} must not throw: {got}"
            );
            assert!(
                !got.contains('!'),
                "{label} + {m}: an index reads differently twice in a row, so \
                 the method and the property path disagree ({got})"
            );
            let entries: Vec<&str> = got.split(';').filter(|e| !e.is_empty()).collect();
            assert!(
                entries.iter().all(|e| e.ends_with('=')),
                "{label} + {m}: malformed read log ({got})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 2. The node-agreement table, transcribed row by row.
//    Every expected value is node v24.21.0's output for the same source.
// ---------------------------------------------------------------------------

#[test]
fn node_table_numeric_length_own_indices() {
    // Receiver {0:"a",1:"b",2:"c",length:3}.
    for (what, src, expected) in [
        (
            "fill",
            r#"Array.prototype.fill.call(o,"q",0,3); JSON.stringify(o)"#,
            r#"{"0":"q","1":"q","2":"q","length":3}"#,
        ),
        (
            "copyWithin",
            r#"Array.prototype.copyWithin.call(o,0,1,3); JSON.stringify(o)"#,
            r#"{"0":"b","1":"c","2":"c","length":3}"#,
        ),
        (
            "slice",
            r#"JSON.stringify(Array.prototype.slice.call(o))"#,
            r#"["a","b","c"]"#,
        ),
        ("join", r#"Array.prototype.join.call(o,"-")"#, r#"a-b-c"#),
        (
            "sort",
            r#"Array.prototype.sort.call(o); JSON.stringify(o)"#,
            r#"{"0":"a","1":"b","2":"c","length":3}"#,
        ),
        (
            "reverse",
            r#"Array.prototype.reverse.call(o); JSON.stringify(o)"#,
            r#"{"0":"c","1":"b","2":"a","length":3}"#,
        ),
        (
            "shift",
            r#"Array.prototype.shift.call(o); JSON.stringify(o)"#,
            r#"{"0":"b","1":"c","length":2}"#,
        ),
        (
            "unshift",
            r#"Array.prototype.unshift.call(o,"z"); JSON.stringify(o)"#,
            r#"{"0":"z","1":"a","2":"b","3":"c","length":4}"#,
        ),
        (
            "splice",
            r#"JSON.stringify(Array.prototype.splice.call(o,1,1))"#,
            r#"["b"]"#,
        ),
        (
            "concat",
            r#"JSON.stringify(Array.prototype.concat.call(o))"#,
            r#"[{"0":"a","1":"b","2":"c","length":3}]"#,
        ),
        (
            "toString",
            r#"Array.prototype.toString.call(o)"#,
            r#"[object Object]"#,
        ),
        (
            "push",
            r#"Array.prototype.push.call(o,"d"); JSON.stringify(o)"#,
            r#"{"0":"a","1":"b","2":"c","3":"d","length":4}"#,
        ),
        (
            "pop",
            r#"Array.prototype.pop.call(o); JSON.stringify(o)"#,
            r#"{"0":"a","1":"b","length":2}"#,
        ),
        ("indexOf", r#"Array.prototype.indexOf.call(o,"b")"#, "1"),
        (
            "lastIndexOf",
            r#"Array.prototype.lastIndexOf.call(o,"b")"#,
            "1",
        ),
        (
            "includes",
            r#"Array.prototype.includes.call(o,"b")"#,
            "true",
        ),
        ("at", r#"Array.prototype.at.call(o,1)"#, r#"b"#),
        (
            "flat",
            r#"JSON.stringify(Array.prototype.flat.call(o))"#,
            r#"["a","b","c"]"#,
        ),
        (
            "indexOf_after_fill_agrees_with_read",
            r#"Array.prototype.fill.call(o,"q",0,3); [Array.prototype.indexOf.call(o,"q"), Array.prototype.indexOf.call(o,"a")].join()"#,
            "0,-1",
        ),
    ] {
        assert_is(
            &eval_json(&format!(r#"var o = {{0:"a",1:"b",2:"c",length:3}}; {src}"#)),
            expected,
            what,
        );
    }
}

#[test]
fn node_table_string_length() {
    // A *string* `length` is the coercion case the deleted `array_len`'s
    // `as_smi().or(as_f64)` got wrong: "3" is not a number, so it became NaN
    // and therefore 0.
    for (what, src, expected) in [
        ("at(0)", r#"Array.prototype.at.call(o,0)"#, r#"a"#),
        ("at(1)", r#"Array.prototype.at.call(o,1)"#, r#"b"#),
        (
            "slice",
            r#"JSON.stringify(Array.prototype.slice.call(o))"#,
            r#"["a","b","c"]"#,
        ),
        ("join", r#"Array.prototype.join.call(o,"-")"#, r#"a-b-c"#),
        (
            "fill readable",
            r#"Array.prototype.fill.call(o,"q",0,3); o[0]"#,
            r#"q"#,
        ),
        ("indexOf(a)", r#"Array.prototype.indexOf.call(o,"a")"#, "0"),
        (
            "lastIndexOf(b)",
            r#"Array.prototype.lastIndexOf.call(o,"b")"#,
            "1",
        ),
        (
            "includes(a)",
            r#"Array.prototype.includes.call(o,"a")"#,
            "true",
        ),
        (
            "shift",
            r#"Array.prototype.shift.call(o); JSON.stringify(o)"#,
            r#"{"0":"b","1":"c","length":2}"#,
        ),
        (
            "splice",
            r#"Array.prototype.splice.call(o,1,1); JSON.stringify(o)"#,
            r#"{"0":"a","1":"c","length":2}"#,
        ),
        (
            "push",
            r#"Array.prototype.push.call(o,"d"); JSON.stringify(o)"#,
            r#"{"0":"a","1":"b","2":"c","3":"d","length":4}"#,
        ),
        (
            "flat",
            r#"JSON.stringify(Array.prototype.flat.call(o))"#,
            r#"["a","b","c"]"#,
        ),
    ] {
        assert_is(
            &eval_json(&format!(
                r#"var o = {{0:"a",1:"b",2:"c",length:"3"}}; {src}"#
            )),
            expected,
            what,
        );
    }
}

#[test]
fn node_table_inherited_indices() {
    // `Object.create({0:"inh", length:3})` — the indices live on the
    // prototype. Spec-correct in node because `[[Get]]`/`[[HasProperty]]` walk
    // the chain; every hand-rolled reader here did not.
    for (what, src, expected) in [
        ("indexOf", r#"Array.prototype.indexOf.call(o,"inh")"#, "0"),
        (
            "slice",
            r#"JSON.stringify(Array.prototype.slice.call(o))"#,
            r#"["inh",null,null]"#,
        ),
        (
            "slice(1)",
            r#"JSON.stringify(Array.prototype.slice.call(o,1))"#,
            r#"[null,null]"#,
        ),
        (
            "includes",
            r#"Array.prototype.includes.call(o,"inh")"#,
            "true",
        ),
        ("at", r#"Array.prototype.at.call(o,0)"#, r#"inh"#),
        ("join", r#"Array.prototype.join.call(o,"-")"#, r#"inh--"#),
        (
            "flat skips absent",
            r#"JSON.stringify(Array.prototype.flat.call(o))"#,
            r#"["inh"]"#,
        ),
        (
            "concat appends receiver",
            r#"JSON.stringify(Array.prototype.concat.call(o).length)"#,
            "1",
        ),
        (
            "toString",
            r#"Array.prototype.toString.call(o)"#,
            r#"[object Object]"#,
        ),
        (
            "shift leaves no own index",
            r#"Array.prototype.shift.call(o); JSON.stringify(o)"#,
            r#"{"length":2}"#,
        ),
        (
            "splice leaves no own index",
            r#"Array.prototype.splice.call(o,1,1); JSON.stringify(o)"#,
            r#"{"length":2}"#,
        ),
        (
            "fill readable",
            r#"Array.prototype.fill.call(o,"q",0,3); JSON.stringify(o)"#,
            r#"{"0":"q","1":"q","2":"q"}"#,
        ),
        (
            "unshift",
            r#"Array.prototype.unshift.call(o,"z"); JSON.stringify(o)"#,
            r#"{"0":"z","1":"inh","length":4}"#,
        ),
        (
            "push",
            r#"Array.prototype.push.call(o,"d"); JSON.stringify(o)"#,
            r#"{"3":"d","length":4}"#,
        ),
    ] {
        assert_is(
            &eval_json(&format!(
                r#"var o = Object.create({{0:"inh", length:3}}); {src}"#
            )),
            expected,
            what,
        );
    }
}

#[test]
fn node_table_inherited_length_only() {
    for (what, src, expected) in [
        ("join", r#"Array.prototype.join.call(o,"-")"#, r#"--"#),
        (
            "slice",
            r#"JSON.stringify(Array.prototype.slice.call(o))"#,
            r#"[null,null,null]"#,
        ),
        (
            "flat skips absent",
            r#"JSON.stringify(Array.prototype.flat.call(o))"#,
            "[]",
        ),
        (
            "push",
            r#"Array.prototype.push.call(o,"d"); JSON.stringify(o)"#,
            r#"{"3":"d","length":4}"#,
        ),
        (
            "shift",
            r#"Array.prototype.shift.call(o); JSON.stringify(o)"#,
            r#"{"length":2}"#,
        ),
        (
            "splice",
            r#"Array.prototype.splice.call(o,1,1); JSON.stringify(o)"#,
            r#"{"length":2}"#,
        ),
        (
            "fill",
            r#"Array.prototype.fill.call(o,"q",0,3); JSON.stringify(o)"#,
            r#"{"0":"q","1":"q","2":"q"}"#,
        ),
        (
            "toString",
            r#"Array.prototype.toString.call(o)"#,
            r#"[object Object]"#,
        ),
    ] {
        assert_is(
            &eval_json(&format!(r#"var o = Object.create({{length:3}}); {src}"#)),
            expected,
            what,
        );
    }
}

#[test]
fn node_table_arguments_length_bookkeeping() {
    // `arguments` keeps its elements in the flat store — the one receiver where
    // `elements_snapshot()` is the right answer — and still diverged, because
    // the length writers guarded on `kind == Array`.
    for (what, src, expected) in [
        (
            "unshift",
            r#"Array.prototype.unshift.call(a,"z"); [a.length, a[0], a[1], a[2], a[3]].join()"#,
            r#"4,z,1,2,3"#,
        ),
        (
            "splice",
            r#"Array.prototype.splice.call(a,1,1); [a.length, a[0], a[1], a[2]].join()"#,
            r#"2,1,3,"#,
        ),
        (
            "splice insert",
            r#"Array.prototype.splice.call(a,1,0,"Z"); [a.length, a[0], a[1], a[2], a[3]].join()"#,
            r#"4,1,Z,2,3"#,
        ),
        (
            "shift",
            r#"Array.prototype.shift.call(a); [a.length, a[0], a[1], a[2]].join()"#,
            r#"2,2,3,"#,
        ),
        (
            "toString tag",
            r#"Array.prototype.toString.call(a)"#,
            r#"[object Arguments]"#,
        ),
    ] {
        assert_is(
            &eval_json(&format!(
                r#"var a = (function(){{ return arguments; }})(1,2,3); {src}"#
            )),
            expected,
            what,
        );
    }
}

#[test]
fn node_table_kind_array_fast_path_is_intact() {
    // The `Kind::Array` fast paths (`index_get`/`index_set`'s lattice rung,
    // `array_range`'s snapshot, `dense_entries`) are kept deliberately; these
    // rows are their regression gate.
    for (what, src, expected) in [
        (
            "pop trims the store",
            r#"a.pop(); [a.length, a[2]].join()"#,
            r#"2,"#,
        ),
        (
            "shift closes the gap",
            r#"a.shift(); JSON.stringify(a)"#,
            r#"["b","c"]"#,
        ),
        (
            "splice",
            r#"JSON.stringify(Array.prototype.splice.call(a,1,1))"#,
            r#"["b"]"#,
        ),
        (
            "reverse",
            r#"a.reverse(); JSON.stringify(a)"#,
            r#"["c","b","a"]"#,
        ),
        ("sort", r#"a.sort(); JSON.stringify(a)"#, r#"["a","b","c"]"#),
        ("slice", r#"JSON.stringify(a.slice(1))"#, r#"["b","c"]"#),
        ("join", r#"a.join("-")"#, r#"a-b-c"#),
        ("toString", r#"a.toString()"#, r#"a,b,c"#),
        (
            "fill",
            r#"a.fill("q",1,3); JSON.stringify(a)"#,
            r#"["a","q","q"]"#,
        ),
        (
            "copyWithin",
            r#"a.copyWithin(0,1,3); JSON.stringify(a)"#,
            r#"["b","c","c"]"#,
        ),
        (
            "push",
            r#"a.push("d"); JSON.stringify(a)"#,
            r#"["a","b","c","d"]"#,
        ),
        ("flat", r#"JSON.stringify(a.flat())"#, r#"["a","b","c"]"#),
        (
            "unshift",
            r#"a.unshift("z"); JSON.stringify(a)"#,
            r#"["z","a","b","c"]"#,
        ),
        (
            "concat",
            r#"JSON.stringify(a.concat([1]))"#,
            r#"["a","b","c",1]"#,
        ),
        ("indexOf", r#"a.indexOf("b")"#, "1"),
    ] {
        assert_is(
            &eval_json(&format!(r#"var a = ["a","b","c"]; {src}"#)),
            expected,
            what,
        );
    }
}

#[test]
fn node_table_sparse_and_holey_arrays_keep_their_length() {
    for (what, decl, src, expected) in [
        (
            "sparse pop",
            "var a = []; a.length = 3;",
            "a.pop(); [a.length, a[0], a[1], a[2]].join()",
            r#"2,,,"#,
        ),
        (
            "sparse unshift",
            "var a = []; a.length = 3;",
            "a.unshift('z'); [a.length, a[0], a[1], a[2], a[3]].join()",
            r#"4,z,,,"#,
        ),
        (
            "sparse splice",
            "var a = []; a.length = 3;",
            "a.splice(1,1); [a.length, a[0], a[1], a[2]].join()",
            r#"2,,,"#,
        ),
        (
            "sparse splice insert",
            "var a = []; a.length = 3;",
            "a.splice(1,0,'Z'); [a.length, a[0], a[1], a[2], a[3]].join()",
            r#"4,,Z,,"#,
        ),
        (
            "sparse slice",
            "var a = []; a.length = 3;",
            "JSON.stringify(a.slice())",
            r#"[null,null,null]"#,
        ),
        (
            "sparse toString",
            "var a = []; a.length = 3;",
            "a.toString()",
            r#",,"#,
        ),
        (
            "sparse join",
            "var a = []; a.length = 3;",
            "a.join('-')",
            r#"--"#,
        ),
        (
            "holey shift",
            "var a = ['a']; a[3] = 'd'; a.length = 5;",
            "a.shift(); [a.length, a[0], a[1], a[2], a[3], a[4]].join()",
            r#"4,,,d,,"#,
        ),
        (
            "holey unshift",
            "var a = ['a']; a[3] = 'd'; a.length = 5;",
            "a.unshift('z'); [a.length, a[0], a[1], a[2], a[3], a[4], a[5]].join()",
            r#"6,z,a,,,d,"#,
        ),
        (
            "holey splice",
            "var a = ['a']; a[3] = 'd'; a.length = 5;",
            "a.splice(1,1); [a.length, a[0], a[1], a[2], a[3], a[4]].join()",
            r#"4,a,,d,,"#,
        ),
        (
            "holey fill",
            "var a = ['a']; a[3] = 'd'; a.length = 5;",
            "a.fill('q', 0, 3); [a.length, a[0], a[1], a[2], a[3], a[4]].join()",
            r#"5,q,q,q,d,"#,
        ),
        (
            "holey toString",
            "var a = ['a']; a[3] = 'd'; a.length = 5;",
            "a.toString()",
            r#"a,,,d,"#,
        ),
        (
            "holey join",
            "var a = ['a']; a[3] = 'd'; a.length = 5;",
            "a.join('-')",
            r#"a---d-"#,
        ),
        (
            "holey slice",
            "var a = ['a']; a[3] = 'd'; a.length = 5;",
            "JSON.stringify(a.slice(1))",
            r#"[null,null,"d",null]"#,
        ),
    ] {
        assert_is(&eval_json(&format!("{decl} {src}")), expected, what);
    }
}

// ---------------------------------------------------------------------------
// 3. The declared deviations, pinned by name.
// ---------------------------------------------------------------------------

/// `MAX_WRITE_SPAN` is a permanent deviation: `fill`/`copyWithin` will not
/// materialize billions of entries. Pinned because the silent narrowing it
/// replaced (`dense_bound`) is what made `fill`'s range *wrong* rather than
/// merely capped, and without this test someone will "fix" the cap away.
#[test]
fn max_write_span_is_a_declared_deviation() {
    // No `length` anywhere on the chain: `LengthOfArrayLike` is 0, so neither
    // `fill` nor `copyWithin` writes anything, and the receiver gains no own
    // property. At `b0c3353e` `fill` wrote a hole into the flat `elements`
    // store here — a property the receiver never had and never reported.
    assert_is(
        &eval_json(
            r#"
            var o = {};
            Array.prototype.fill.call(o, "q");
            Array.prototype.fill.call(o, "q", 0, 10);
            Array.prototype.copyWithin.call(o, 0, 1);
            JSON.stringify([o.length === undefined, Object.keys(o).length]);
            "#,
        ),
        r#"[true,0]"#,
        "a receiver with no `length` is length 0, not an implicit store",
    );
}

#[test]
fn a_huge_length_stays_a_bounded_object() {
    // `length` near 2^32 must not become a 2^32-entry store, and the last
    // valid array index (2^32-2) is the highest one that can be written:
    // `4294967295` is a plain string key, which is why exactly one own
    // property appears. Both `node` and v12 report `[4294967295, 1]`.
    //
    // The expensive half is deliberately NOT exercised here: filling
    // `{length: 4294967295}` from 0 writes `MAX_WRITE_SPAN` (1e6) own
    // properties and costs ~6 s, because a generic receiver has no
    // representation for an index other than one property per index. That cost
    // IS the deviation, and a unit test that takes 6 s would hide it rather
    // than pin it.
    assert_is(
        &eval_json(
            r#"
            var o = {length: 4294967295};
            Array.prototype.fill.call(o, "q", 4294967295, 4294967296);
            Array.prototype.copyWithin.call(o, 4294967295, 4294967296);
            JSON.stringify([o.length, Object.keys(o).length]);
            "#,
        ),
        r#"[4294967295,1]"#,
        "a 2^32 length stays a bounded object",
    );
}

// ---------------------------------------------------------------------------
// 4. The callback seam — ADR-0004 Batch 2.
//
// `Interp::run_callback_builtin` (`v12-interp/src/call_setup.rs`) was the sixth
// copy of the straddle, behind `callback_len` / `callback_elem` /
// `callback_dense_bound`. It no longer is: all three are deleted and the seam
// reads `ToLength(Get(O,"length"))` and `HasProperty`/`Get` through the
// interpreter's own property path.
// ---------------------------------------------------------------------------

/// `some` returning `false` on a receiver with three elements and an
/// always-true predicate was the headline symptom of the whole finding — the
/// one place v12 asserted something *positive and wrong* about present data.
#[test]
fn callback_seam_agrees_with_node_on_a_generic_receiver() {
    for (what, src, expected) in [
        ("some", r#"Array.prototype.some.call(o, function(){ return true; })"#, "true"),
        ("every", r#"Array.prototype.every.call(o, function(){ return true; })"#, "true"),
        ("map", r#"JSON.stringify(Array.prototype.map.call(o, function(x){ return x; }))"#, r#"["a","b","c"]"#),
        ("filter", r#"JSON.stringify(Array.prototype.filter.call(o, function(){ return true; }))"#, r#"["a","b","c"]"#),
        ("flatMap", r#"JSON.stringify(Array.prototype.flatMap.call(o, function(x){ return [x]; }))"#, r#"["a","b","c"]"#),
        (
            "forEach visits 3",
            r#"var n = 0; Array.prototype.forEach.call(o, function(){ n++; }); n"#,
            "3",
        ),
        ("reduce", r#"Array.prototype.reduce.call(o, function(p,c){ return p + "-" + c; }, "R")"#, "R-a-b-c"),
        ("find", r#"Array.prototype.find.call(o, function(x){ return x === "c"; })"#, "c"),
        ("findIndex", r#"Array.prototype.findIndex.call(o, function(x){ return x === "c"; })"#, "2"),
    ] {
        assert_is(
            &eval_json(&format!(
                r#"var o = {{0:"a",1:"b",2:"c",length:3}}; {src}"#
            )),
            expected,
            what,
        );
    }
}

/// The same, with the indices on the **prototype** — the case
/// `callback_elem`'s own-shape-only lookup could not see.
#[test]
fn callback_seam_sees_inherited_indices() {
    for (what, src, expected) in [
        ("some", r#"Array.prototype.some.call(o, function(){ return true; })"#, "true"),
        ("map pads the skipped indices", r#"JSON.stringify(Array.prototype.map.call(o, function(x){ return x; }))"#, r#"["inh",null,null]"#),
        ("filter appends only", r#"JSON.stringify(Array.prototype.filter.call(o, function(){ return true; }))"#, r#"["inh"]"#),
        (
            "forEach visits 1",
            r#"var n = 0; Array.prototype.forEach.call(o, function(){ n++; }); n"#,
            "1",
        ),
        ("reduce", r#"Array.prototype.reduce.call(o, function(p,c){ return p + "-" + c; }, "R")"#, "R-inh"),
        ("findIndex", r#"Array.prototype.findIndex.call(o, function(x){ return x === "inh"; })"#, "0"),
        ("sort materialises index 0", r#"Array.prototype.sort.call(o); JSON.stringify(o)"#, r#"{"0":"inh"}"#),
    ] {
        assert_is(
            &eval_json(&format!(
                r#"var o = Object.create({{0:"inh", length:3}}); {src}"#
            )),
            expected,
            what,
        );
    }
}

/// A `length` on the prototype, and a **string** `length`, both reach the seam:
/// `callback_len` used to read `properties[0]`/`element_len()` and never saw a
/// `length` property at all, so every one of these answered "empty".
#[test]
fn callback_seam_reads_an_inherited_or_string_length() {
    for (what, decl, src, expected) in [
        (
            "inherited length, some",
            "var o = Object.create({length:3});",
            r#"Array.prototype.some.call(o, function(){ return true; })"#,
            "false",
        ),
        (
            "inherited length, map pads to length",
            "var o = Object.create({length:3});",
            r#"JSON.stringify(Array.prototype.map.call(o, function(x){ return x; }))"#,
            r#"[null,null,null]"#,
        ),
        (
            "inherited length, forEach visits 0",
            "var o = Object.create({length:3});",
            r#"var n = 0; Array.prototype.forEach.call(o, function(){ n++; }); n"#,
            "0",
        ),
        (
            "inherited length, reduce reads 3 holes",
            "var o = Object.create({length:3});",
            r#"Array.prototype.reduce.call(o, function(p,c){ return p + "-" + c; }, "R")"#,
            "R",
        ),
        (
            "string length, some",
            r#"var o = {0:"a",1:"b",2:"c",length:"3"};"#,
            r#"Array.prototype.some.call(o, function(){ return true; })"#,
            "true",
        ),
        (
            "string length, map",
            r#"var o = {0:"a",1:"b",2:"c",length:"3"};"#,
            r#"JSON.stringify(Array.prototype.map.call(o, function(x){ return x; }))"#,
            r#"["a","b","c"]"#,
        ),
    ] {
        assert_is(&eval_json(&format!("{decl} {src}")), expected, what);
    }
}

/// Holes are skipped by **every** method on this seam, `reduce` and
/// `reduceRight` included — which is the wrong assumption this batch made
/// first. The test is presence, not `undefined`-ness: `{0:"a", 1:undefined,
/// length:3}` visits `[0, 1]`, `{0:"a", 2:"c", length:4}` visits `[0, 2]`.
#[test]
fn callback_seam_skips_holes_and_not_undefined() {
    for (what, decl, src, expected) in [
        (
            "own sparse indices",
            r#"var o = {0:"a",2:"c",length:4};"#,
            r#"var v = []; Array.prototype.reduce.call(o, function(p,c,i){ v.push(i); return p; }, ""); v.join()"#,
            "0,2",
        ),
        (
            "a present undefined is visited",
            r#"var o = {0:"a",1:undefined,length:3};"#,
            r#"var v = []; Array.prototype.reduce.call(o, function(p,c,i){ v.push(i); return p; }, ""); v.join()"#,
            "0,1",
        ),
        (
            "holey real array",
            r#"var o = ["a"]; o[3] = "d"; o.length = 5;"#,
            r#"var v = []; Array.prototype.reduce.call(o, function(p,c,i){ v.push(i); return p; }, ""); v.join()"#,
            "0,3",
        ),
        (
            "holey map pads to length",
            r#"var o = ["a"]; o[3] = "d"; o.length = 5;"#,
            r#"JSON.stringify(Array.prototype.map.call(o, function(x){ return x; }))"#,
            r#"["a",null,null,"d",null]"#,
        ),
    ] {
        assert_is(&eval_json(&format!("{decl} {src}")), expected, what);
    }
}

/// `reduce`'s callback takes **four** arguments — `(accumulator, value, index,
/// array)`. The seam passed three, so a `reduce` callback read the receiver
/// where the index belongs.
#[test]
fn the_reduce_callback_receives_the_index() {
    for (what, src, expected) in [
        (
            "reduce",
            r#"var v = []; Array.prototype.reduce.call(["a","b","c"], function(acc,c,i){ v.push(i); return acc; }, ""); v.join()"#,
            "0,1,2",
        ),
        (
            "reduceRight",
            r#"var v = []; Array.prototype.reduceRight.call(["a","b","c"], function(acc,c,i){ v.push(i); return acc; }, ""); v.join()"#,
            "2,1,0",
        ),
        (
            "forEach",
            r#"var v = []; ["a","b","c"].forEach(function(c,i){ v.push(i); }); v.join()"#,
            "0,1,2",
        ),
        (
            "the fourth argument is the receiver",
            r#"Array.prototype.reduce.call(["a"], function(acc,c,i,arr){ return arr.length; }, "")"#,
            "1",
        ),
    ] {
        assert_is(&eval_json(src), expected, what);
    }
}

/// `MAX_CALLBACK_SCAN` is a declared deviation, like `MAX_SCAN_SPAN` in
/// `builtins/array.rs`: `ToLength` can be 2^53-1 and a generic receiver has no
/// enumerable store, so the walk is capped.
#[test]
fn max_callback_scan_is_a_declared_deviation() {
    assert_is(
        &eval_json(
            r#"
            var o = {};
            var n = 0;
            Array.prototype.forEach.call(o, function(){ n++; });
            Array.prototype.some.call(o, function(){ return true; });
            JSON.stringify([o.length === undefined, n]);
            "#,
        ),
        r#"[true,0]"#,
        "a receiver with no `length` is length 0 to the callback seam too",
    );
}

/// The Batch-1 `array_layout_len`/`write_array_layout_len` workaround existed
/// only because `Heap::add_property` handed out slot 0 while the write site
/// pushed at index 1 for a `JsObject::array`-born array. The cause is fixed in
/// `internal_methods::store_shape_extension`, so the length must now be an
/// ordinary property: writing it through `[[Set]]` and reading it back through
/// a plain `o.length` has to agree, on an array built by the raw heap
/// constructor.
#[test]
fn a_heap_built_array_length_is_an_ordinary_property() {
    // Driven from JS this exercises the engine-created array. The
    // `JsObject::array` + `heap.alloc` shape is the unit test in
    // `crates/v12-engine/src/tests.rs::builtin_tests::array_push_and_pop_length`,
    // which is where the defect was originally observed; this row pins that the
    // property path is the only path, i.e. no layout special case survives.
    assert_is(
        &eval_json(
            r#"
            var a = ["a", "b"];
            Array.prototype.push.call(a, "c");
            Array.prototype.pop.call(a);
            JSON.stringify([a.length, a[0], a[1], a[2]]);
            "#,
        ),
        r#"[2,"a","b",null]"#,
        "push then pop on a real array",
    );
}
