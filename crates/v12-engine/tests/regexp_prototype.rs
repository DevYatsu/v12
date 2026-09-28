//! `%RegExp.prototype%` must carry the ES 22.2 surface as ordinary installed
//! properties, not as something the interpreter synthesizes per read.
//!
//! The engine already had `regexp_exec`/`regexp_test`/`regexp_to_string`/
//! `regexp_compile` bodies in `builtins/regexp.rs` and dispatch arms in the
//! native registry, but **none of them was installed** — and the ten 22.2.6
//! data accessors, the `lastIndex` accessor and the five 22.2.7 symbol-keyed
//! protocol methods did not exist at all. Only the interpreter's
//! `Kind::RegExp` structural surface (`Interp::regexp_surface`) kept
//! `/a/.exec(…)` working, so the failure was invisible from a script and total
//! from test262: `RegExp.prototype.exec` read `undefined`.
//!
//! These tests are deliberately written against *descriptors*, not against
//! behavior alone, for two reasons.
//!
//! 1. A large share of `built-ins/RegExp/prototype/**` is `verifyProperty`-style
//!    attribute assertions. Installing `source` as a data property instead of an
//!    accessor, or marking any of it enumerable, converts a real fix into a
//!    wash — so the attributes are asserted directly.
//! 2. Property storage layout is a real hazard here. `Heap::define_accessor`
//!    grows a shape's `num_own` without storing a value, so a prototype that
//!    installs accessors *before* data properties ends up with
//!    `properties.len() < num_own` and every later data slot reads out of
//!    bounds. `prototype_data_properties_survive_the_accessor_prefix` is the
//!    regression test for that: the accessor half of the surface sits between
//!    `constructor` and `exec` in the install order, and `exec` is the first
//!    data property after it.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// `getOwnPropertyDescriptor(RegExp.prototype, name)` rendered as
/// `"<get|value>,<set>,<enumerable>,<configurable>"`, with `get`/`set`/`value`
/// as `fn`/`undef`/`ok` so an attribute sweep needs no per-property JS.
fn accessor_attrs(name: &str) -> String {
    eval_display(&format!(
        "var d = Object.getOwnPropertyDescriptor(RegExp.prototype, '{name}'); \
         [typeof d.get, typeof d.set, d.enumerable, d.configurable].join(',')"
    ))
}

fn method_attrs(name: &str) -> String {
    eval_display(&format!(
        "var d = Object.getOwnPropertyDescriptor(RegExp.prototype, '{name}'); \
         [typeof d.value, d.writable, d.enumerable, d.configurable].join(',')"
    ))
}

// -- group 1: the 22.2.6 data accessors -------------------------------------

/// `source` is a getter-only, non-enumerable, configurable accessor whose
/// function is named `get source` and takes no arguments.
#[test]
fn source_is_a_getter_only_accessor() {
    assert_eq!(accessor_attrs("source"), "function,undefined,false,true");
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(RegExp.prototype, 'source').get.name"),
        "get source"
    );
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(RegExp.prototype, 'source').get.length"),
        "0"
    );
}

/// The same descriptor shape for all eight single-flag getters, which is the
/// whole of the rest of 22.2.6's accessor set.
#[test]
fn every_flag_getter_is_a_getter_only_accessor() {
    for name in [
        "global",
        "ignoreCase",
        "multiline",
        "dotAll",
        "unicode",
        "unicodeSets",
        "sticky",
        "hasIndices",
    ] {
        assert_eq!(
            accessor_attrs(name),
            "function,undefined,false,true",
            "{name}"
        );
        assert_eq!(
            eval_display(&format!(
                "Object.getOwnPropertyDescriptor(RegExp.prototype, '{name}').get.name"
            )),
            format!("get {name}"),
            "{name} name"
        );
    }
}

/// `get flags` reads the receiver's own flag string, and the empty pattern's
/// canonical `"(?:)"` source — the two values the accessors are *for*. A
/// getter that always returned `undefined` would pass the attribute tests
/// above, so this is the behavioural half.
#[test]
fn source_and_flags_read_the_receivers_pattern() {
    assert_eq!(eval_display("/a(b)c/gi.source"), "a(b)c");
    assert_eq!(eval_display("/a(b)c/gi.flags"), "gi");
    assert_eq!(eval_display("/a/.global"), "false");
    assert_eq!(eval_display("/a/g.global"), "true");
    assert_eq!(eval_display("/a/g.ignoreCase"), "false");
    assert_eq!(eval_display("/a/gi.ignoreCase"), "true");
    assert_eq!(eval_display("/a/.sticky"), "false");
    assert_eq!(eval_display("/a/y.sticky"), "true");
    assert_eq!(eval_display("/a/.hasIndices"), "false");
    assert_eq!(eval_display("/a/d.hasIndices"), "true");
    assert_eq!(eval_display("/a/.dotAll"), "false");
    assert_eq!(eval_display("/a/s.dotAll"), "true");
    assert_eq!(eval_display("/a/.unicode"), "false");
    assert_eq!(eval_display("/a/u.unicode"), "true");
    assert_eq!(eval_display("/a/.unicodeSets"), "false");
    assert_eq!(eval_display("/a/v.unicodeSets"), "true");
    assert_eq!(eval_display("/a/.multiline"), "false");
    assert_eq!(eval_display("/a/m.multiline"), "true");
}

/// ES 22.2.6.1 step 3a: `%RegExp.prototype%` is the one non-RegExp receiver
/// `get source` admits, and it answers `"(?:)"` — while `get flags` answers
/// `""` and a single-flag getter answers `undefined`.
#[test]
fn the_prototype_itself_is_the_special_receiver() {
    let d = "Object.getOwnPropertyDescriptor(RegExp.prototype,";
    assert_eq!(
        eval_display(&format!("{d} 'source').get.call(RegExp.prototype)")),
        "(?:)"
    );
    assert_eq!(
        eval_display(&format!("{d} 'flags').get.call(RegExp.prototype)")),
        ""
    );
    assert_eq!(
        eval_display(&format!("{d} 'global').get.call(RegExp.prototype)")),
        "undefined"
    );
}

/// ES 22.2.6.1 step 2: a `this` that is not an object, and an object without
/// the RegExp internal slots, are both `TypeError`s. This is what keeps a
/// `{}` from reporting a bogus `source`/`flags`.
#[test]
fn a_non_regexp_receiver_is_a_type_error() {
    let g = "Object.getOwnPropertyDescriptor(RegExp.prototype, 'source').get";
    for recv in ["undefined", "{}", "[]", "3"] {
        assert_eq!(
            eval_display(&format!(
                "(function () {{ try {{ ({g}).call({recv}); return 'no throw' }} catch (e) {{ return e.constructor.name }} }})()"
            )),
            "TypeError",
            "{recv}"
        );
    }
}

/// The `[object RegExp]` brand comes from `Object.prototype.toString`'s spec
/// brand table, *not* from an installed `@@toStringTag` (ES 10.4.1 makes
/// `%RegExp.prototype%` an ordinary object, and test262
/// `built-ins/RegExp/prototype/15.10.6.js` pins that).
#[test]
fn the_object_prototype_brand_is_regexp_for_instances_only() {
    assert_eq!(
        eval_display("Object.prototype.toString.call(/a/)"),
        "[object RegExp]"
    );
    assert_eq!(
        eval_display("Object.prototype.toString.call(RegExp.prototype)"),
        "[object Object]"
    );
}

// -- group 2: the `lastIndex` accessor --------------------------------------

/// `lastIndex` is the one accessor on the surface with both halves, so it is
/// the one whose descriptor carries a real `set`.
#[test]
fn last_index_is_a_read_write_accessor() {
    assert_eq!(accessor_attrs("lastIndex"), "function,function,false,true");
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(RegExp.prototype, 'lastIndex').get.name"),
        "get lastIndex"
    );
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(RegExp.prototype, 'lastIndex').set.name"),
        "set lastIndex"
    );
}

/// The accessor pair must reach the receiver's `lastIndex` *slot*, and coerce
/// through `ToNumber` on the way in (ES 22.2.6.1's setter).
#[test]
fn the_last_index_accessor_round_trips_through_the_slot() {
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(RegExp.prototype, 'lastIndex'); \
             var re = /a/g; d.set.call(re, 7); d.get.call(re)"
        ),
        "7"
    );
    // The slot write is visible through the ordinary instance read too, and
    // `ToNumber` applies (`'12'` → 12, `undefined` → NaN).
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(RegExp.prototype, 'lastIndex'); \
             var re = /a/g; d.set.call(re, '12'); re.lastIndex + 1"
        ),
        "13"
    );
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(RegExp.prototype, 'lastIndex'); \
             var re = /a/g; d.set.call(re, undefined); String(re.lastIndex)"
        ),
        "NaN"
    );
}

/// An instance's own `lastIndex` is served from the internal slot ahead of the
/// prototype walk, so the accessor must not shadow or duplicate it.
#[test]
fn an_instance_last_index_keeps_advancing_across_exec_calls() {
    assert_eq!(
        eval_display(
            "var re = /a/g; var seen = []; \
             seen.push(re.exec('aab').index, re.lastIndex, re.exec('aab').index, re.lastIndex); \
             seen.join(',')"
        ),
        "0,1,1,2"
    );
}

// -- group 3: the 22.2.5 methods -------------------------------------------

/// `exec`/`test`/`toString`/`compile` are `{writable: true, enumerable:
/// false, configurable: true}` data properties with their spec `length`.
#[test]
fn the_four_methods_are_writable_non_enumerable_configurable() {
    for (name, len) in [
        ("exec", "1"),
        ("test", "1"),
        ("toString", "0"),
        ("compile", "2"),
    ] {
        assert_eq!(method_attrs(name), "function,true,false,true", "{name}");
        assert_eq!(
            eval_display(&format!("RegExp.prototype.{name}.length")),
            len,
            "{name} length"
        );
        assert_eq!(
            eval_display(&format!("RegExp.prototype.{name}.name")),
            name,
            "{name} name"
        );
    }
}

/// The point of group 3: an instance's method read and the prototype property
/// are the *same function object*, so `RegExp.prototype.exec.call(re, s)` and
/// `re.exec(s)` cannot diverge.
#[test]
fn instance_methods_are_the_installed_prototype_methods() {
    assert_eq!(
        eval_display("var re = /a/; re.exec === RegExp.prototype.exec"),
        "true"
    );
    assert_eq!(
        eval_display("var re = /a/; re.test === RegExp.prototype.test"),
        "true"
    );
    assert_eq!(
        eval_display("var re = /a/; re.toString === RegExp.prototype.toString"),
        "true"
    );
    assert_eq!(
        eval_display("RegExp.prototype.exec.call(/a(b)/, 'abc').join('|')"),
        "ab|b"
    );
}

/// `test` is `Boolean(exec(…))`, including the `lastIndex` advance a global
/// pattern performs.
#[test]
fn exec_and_test_agree_with_the_pattern() {
    assert_eq!(eval_display("/a(b)c/.exec('xabcx').index"), "1");
    assert_eq!(eval_display("/a(b)c/.exec('xabcx')[1]"), "b");
    assert_eq!(eval_display("/a(b)c/.test('xabcx')"), "true");
    assert_eq!(eval_display("/zzz/.test('xabcx')"), "false");
    assert_eq!(eval_display("/zzz/.exec('xabcx')"), "null");
    assert_eq!(eval_display("/a/gi.toString()"), "/a/gi");
    assert_eq!(eval_display("/a/.toString()"), "/a/");
}

/// Every `exec` result array is engine-allocated, so it must carry
/// `%Array.prototype%` and a correct `length` — the linkage bug that once
/// cost a whole builtin ~141 tests.
#[test]
fn exec_results_are_real_arrays_with_the_right_length() {
    assert_eq!(eval_display("Array.isArray(/a(b)(c)/.exec('abc'))"), "true");
    assert_eq!(eval_display("/a(b)(c)/.exec('abc').length"), "3");
    assert_eq!(
        eval_display("/a(b)(c)/.exec('abc').constructor === Array"),
        "true"
    );
    assert_eq!(eval_display("/a(b)(c)/.exec('abc').index"), "0");
    assert_eq!(eval_display("/a(b)(c)/.exec('abc').input"), "abc");
}

/// Regression test for the storage skew an accessor prefix would create: the
/// ten accessors and the `lastIndex` accessor install before the four methods,
/// so `exec` is the first data property to land after a run of
/// `define_accessor` calls that grow `num_own` without storing a value.
#[test]
fn prototype_data_properties_survive_the_accessor_prefix() {
    assert_eq!(eval_display("RegExp.prototype.exec.length"), "1");
    assert_eq!(eval_display("typeof RegExp.prototype.exec"), "function");
    assert_eq!(eval_display("RegExp.prototype.test.length"), "1");
    assert_eq!(eval_display("RegExp.prototype.compile.length"), "2");
    // Same shape, one level up: an instance reading through the chain.
    assert_eq!(eval_display("/a/.test('a')"), "true");
}
