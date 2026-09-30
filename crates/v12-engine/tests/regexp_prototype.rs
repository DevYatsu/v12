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

/// `to_display_string` with the `threw: ` prefix kept, for assertions that
/// must distinguish "returned null" from "threw" by reading the message.
fn eval_debug_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

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

// -- group 4: the 22.2.7 symbol-keyed protocol methods -----------------------

/// Each protocol method is a `{writable, non-enumerable, configurable}` data
/// property stored under the realm's *own* `Symbol.match` family, named
/// `[Symbol.x]`, with the spec `length`. `matchAll` is the one that exercises
/// slot creation: the macro's `Symbol` group never declared it, so its
/// well-known symbol has to be *defined* rather than overwritten.
#[test]
fn the_symbol_protocol_methods_are_installed_under_their_well_known_symbols() {
    for (name, len) in [
        ("match", "1"),
        ("matchAll", "1"),
        ("replace", "2"),
        ("search", "1"),
        ("split", "2"),
    ] {
        assert_eq!(
            eval_display(&format!(
                "var d = Object.getOwnPropertyDescriptor(RegExp.prototype, Symbol.{name}); \
                 [typeof d.value, d.writable, d.enumerable, d.configurable].join(',')"
            )),
            "function,true,false,true",
            "{name} descriptor"
        );
        assert_eq!(
            eval_display(&format!("RegExp.prototype[Symbol.{name}].name")),
            format!("[Symbol.{name}]"),
            "{name} name"
        );
        assert_eq!(
            eval_display(&format!("RegExp.prototype[Symbol.{name}].length")),
            len,
            "{name} length"
        );
    }
}

/// The mint and the install must be the *same* symbol handle, or
/// `re[Symbol.match]` computes a key the method is not stored under — the
/// failure mode of installing under a fresh symbol without publishing it.
#[test]
fn the_protocol_symbols_are_singletons_shared_with_the_symbol_constructor() {
    for name in ["match", "matchAll", "replace", "search", "split"] {
        assert_eq!(
            eval_display(&format!("typeof Symbol.{name}")),
            "symbol",
            "{name} is a symbol"
        );
        assert_eq!(
            eval_display(&format!("Symbol.{name} === Symbol.{name}")),
            "true",
            "{name} is a singleton"
        );
        assert_eq!(
            eval_display(&format!(
                "/a/[Symbol.{name}] === RegExp.prototype[Symbol.{name}]"
            )),
            "true",
            "{name} is installed under that very symbol"
        );
    }
    // Spec attributes on the `Symbol` constructor's own property.
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(Symbol, 'match'); \
             [d.writable, d.enumerable, d.configurable].join(',')"
        ),
        "false,false,false"
    );
}

/// `@@match` on a non-global pattern is `exec` itself; on a global one it is
/// the array of every whole match, and `null` when there is none.
#[test]
fn symbol_match_follows_the_global_flag() {
    // A global pattern answers the array of whole matches ...
    assert_eq!(eval_display("/a(b)c/gi[Symbol.match]('xabcx').length"), "1");
    // ... a non-global one answers `exec` itself, captures included.
    // `/a(b)c/i` on `xabcx`: two elements (whole + one capture); `index` and
    // `input` are named properties and do not count toward `length`.
    assert_eq!(
        eval_display("/a(b)c/i[Symbol.match]('xabcx').length"),
        "2",
        "whole + one capture"
    );
    assert_eq!(
        eval_display("JSON.stringify(/a/g[Symbol.match]('aab'))"),
        "[\"a\",\"a\"]"
    );
    assert_eq!(eval_display("/a/g[Symbol.match]('zzz')"), "null");
}

/// `@@search` saves, zeroes and restores `lastIndex` (steps 5-6) and answers
/// the match index or `-1`.
#[test]
fn symbol_search_saves_and_restores_last_index() {
    assert_eq!(eval_display("/b/gi[Symbol.search]('xabcx')"), "2");
    assert_eq!(eval_display("/zzz/gi[Symbol.search]('xabcx')"), "-1");
    assert_eq!(
        eval_display("var re = /b/g; re.lastIndex = 4; re[Symbol.search]('abc'); re.lastIndex"),
        "4"
    );
}

/// `@@replace` with a string replacement: `GetSubstitution` handles `$$`,
/// `$&`, `` $` ``, `$'`, `$1`..`$9` and a literal `$`.
#[test]
fn symbol_replace_expands_substitutions() {
    assert_eq!(eval_display("/a/g[Symbol.replace]('aab', 'Z')"), "ZZb");
    assert_eq!(
        eval_display("/(a)(b)/g[Symbol.replace]('abab', '[$2$1]')"),
        "[ba][ba]"
    );
    assert_eq!(eval_display("/a/gi[Symbol.replace]('a', '$$')"), "$");
    assert_eq!(eval_display("/(a)/[Symbol.replace]('xa', '[$&]')"), "x[a]");
}

/// `@@split` honours `limit`, splices in a non-empty match's captures, splits
/// an empty pattern into code units, and terminates on a zero-width pattern.
#[test]
fn symbol_split_handles_limit_captures_and_the_empty_pattern() {
    assert_eq!(
        eval_display("JSON.stringify(/a/[Symbol.split]('1a2a3', 2))"),
        "[\"1\",\"2\"]"
    );
    assert_eq!(
        eval_display("JSON.stringify(/a(b)/[Symbol.split]('1ab2ab3'))"),
        "[\"1\",\"b\",\"2\",\"b\",\"3\"]"
    );
    assert_eq!(
        eval_display("JSON.stringify(new RegExp('')[Symbol.split]('abc'))"),
        "[\"a\",\"b\",\"c\"]"
    );
    assert_eq!(
        eval_display("JSON.stringify(/a/[Symbol.split]('1a2a3', 0))"),
        "[]"
    );
    // A zero-width pattern must not spin: this line is the termination test.
    // The empty pattern is stored canonically as `(?:)`, which is the branch
    // that used to hang, and it answers a plain code-unit split.
    assert_eq!(
        eval_display("JSON.stringify(new RegExp('')[Symbol.split]('ab'))"),
        "[\"a\",\"b\"]"
    );
}

/// `@@matchAll` yields the same match arrays `exec` produces, one per match,
/// and `null` when nothing matches.
#[test]
fn symbol_match_all_yields_every_match() {
    assert_eq!(eval_display("/a/g[Symbol.matchAll]('aab').length"), "2");
    assert_eq!(eval_display("/a/g[Symbol.matchAll]('aab')[1].index"), "1");
    assert_eq!(eval_display("/a/g[Symbol.matchAll]('aab')[0].input"), "aab");
    assert_eq!(eval_debug_display("/zzz/g[Symbol.matchAll]('aab')"), "null");
}

/// Every result these five methods return is engine-allocated, so each must
/// carry `%Array.prototype%` and a correct `length` — the linkage this lane's
/// predecessor had to retrofit.
#[test]
fn every_protocol_result_is_a_real_array() {
    assert_eq!(
        eval_display("Array.isArray(/a/g[Symbol.match]('aab'))"),
        "true"
    );
    assert_eq!(
        eval_display("/a/g[Symbol.match]('aab').constructor === Array"),
        "true"
    );
    assert_eq!(
        eval_display("Array.isArray(/a/[Symbol.split]('aab'))"),
        "true"
    );
    assert_eq!(eval_display("/a/[Symbol.split]('aab').length"), "3");
    assert_eq!(
        eval_display("Array.isArray(/a/g[Symbol.matchAll]('aab'))"),
        "true"
    );
}

// -- group 5: `RegExp [ @@species ]` ----------------------------------------

/// `RegExp[Symbol.species]` is a getter-only accessor named
/// `get [Symbol.species]` whose body is `1. Return the this value.`
#[test]
fn regexp_species_returns_the_receiver() {
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(RegExp, Symbol.species); \
             [typeof d.get, typeof d.set, d.enumerable, d.configurable].join(',')"
        ),
        "function,undefined,false,true"
    );
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(RegExp, Symbol.species).get.name"),
        "get [Symbol.species]"
    );
    // The *accessor* takes no arguments; the receiver it returns is the
    // constructor, whose own `length` is 2.
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(RegExp, Symbol.species).get.length"),
        "0"
    );
    assert_eq!(eval_display("RegExp.length"), "2");
    assert_eq!(eval_display("RegExp[Symbol.species] === RegExp"), "true");
}

// -- group 6: `SpeciesConstructor` (ES 7.3.22) --------------------------------
//
// The defect this group exists for: the implementation tested
// `IsConstructor(O.constructor)` where the spec says `Type(O.constructor) is
// Object`, so any receiver whose `constructor` was a plain object — the shape
// `Symbol.matchAll/species-regexp-get-{global,unicode}-throws.js` uses — threw
// a `TypeError` no engine produces. The two steps also disagree in the *other*
// direction, so one predicate cannot serve both: an object `constructor` passes
// step 3 and can still be rejected at step 7 by a non-constructible
// `@@species`. Each step below is therefore asserted on its own.

/// Runs `body` with `r`'s `constructor` set to the expression `ctor_expr`,
/// and reports the outcome of `r[Symbol.matchAll]('')` as a compact tag:
/// `"ok"`, `"<ErrorName>"` for a thrown value, or `"no-throw"` when the
/// matcher found nothing in the empty string (also a pass, per the spec).
fn match_all_with_constructor(ctor_expr: &str) -> String {
    eval_display(&format!(
        "var r = /./; r.constructor = {ctor_expr}; var out; \
         try {{ r[Symbol.matchAll](''); out = 'ok'; }} \
         catch (e) {{ out = (e && e.constructor) ? e.constructor.name : 'threw'; }} \
         out"
    ))
}

/// Steps 2 and 3: `undefined` selects the default, and *only* a non-Object
/// `constructor` throws. `Symbol.matchAll/species-constructor-is-not-object-
/// throws.js` pins the `null`/`true`/`""`/`Symbol()`/`1` half.
#[test]
fn species_step_2_and_3_turn_on_type_c_not_is_constructor() {
    // Step 2 — `If ctor is undefined, return defaultCtor.` No throw.
    assert_eq!(
        eval_display(
            "var r = /\\w/g; r.constructor = undefined; \
             r[Symbol.matchAll]('a*b').length"
        ),
        "2"
    );
    // Step 3 — the throw cases, verbatim from the corpus test.
    for ctor in ["null", "true", "false", "\"\"", "Symbol()", "1", "0", "86"] {
        assert_eq!(
            match_all_with_constructor(ctor),
            "TypeError",
            "`constructor = {ctor}` must be a TypeError (spec step 3)"
        );
    }
}

/// Step 3 is `Type(ctor) is Object`, *not* `IsConstructor(ctor)`: every one of
/// these `constructor` values is a non-constructor Object, and the spec runs
/// them straight through to a result. This is the regression.
#[test]
fn an_object_constructor_is_not_a_type_error_even_when_it_is_not_a_constructor() {
    for ctor in [
        "{}",
        "Object.create(null)",
        "[]",
        "function(){}",
        "{ [Symbol.species]: function(){} }",
        "Object.defineProperty({}, Symbol.species, { value: RegExp })",
        "new Proxy({}, {})",
    ] {
        assert_ne!(
            match_all_with_constructor(ctor),
            "TypeError",
            "`constructor = {ctor}` is an Object: step 3 must not fire (this \
             is the `species-regexp-get-*-throws.js` regression)"
        );
    }
}

/// Steps 4 and 5: a `@@species` that is `undefined` or `null` selects the
/// default, so a *function-valued* `constructor` that never had `@@species`
/// installed takes the default path — the case the `Type`-only misreading
/// would have rejected.
#[test]
fn species_step_5_selects_the_default_for_undefined_and_null() {
    for species in ["undefined", "null"] {
        assert_eq!(
            eval_display(&format!(
                "var r = /\\w/g; \
                 r.constructor = {{ [Symbol.species]: {species} }}; \
                 r[Symbol.matchAll]('a*b').length"
            )),
            "2",
            "`@@species = {species}` selects the default constructor"
        );
    }
    // A function `constructor` with no `@@species` at all is the same path.
    assert_eq!(
        eval_display(
            "var r = /\\w/g; r.constructor = function(){}; \
             r[Symbol.matchAll]('a*b').length"
        ),
        "2"
    );
}

/// Steps 6 and 7 are a second, *different* predicate on a second, different
/// value: `@@species` must be a constructor, and step 7 **throws** rather than
/// falling back to the default. This is the half a `Type`-only reading of
/// step 3 would lose, so it is asserted explicitly.
#[test]
fn species_step_7_throws_rather_than_falling_back_to_the_default() {
    for species in ["{}", "[]", "0", "\"\"", "Symbol()", "true", "/x/"] {
        assert_eq!(
            match_all_with_constructor(&format!("{{ [Symbol.species]: {species} }}")),
            "TypeError",
            "`@@species = {species}` is not a constructor: spec step 7 throws"
        );
    }
}

/// Step 6: a genuine constructor is returned and *used*. `Construct(C, « R,
/// flags »)` copies `R`'s source and flags, so the drain still finds `R`'s
/// matches — a species constructor that returns a different pattern is not
/// reachable from a `&mut Ctx` native (see `construct_species_matcher`) and is
/// covered by `the_documented_species_constructor_gap` instead.
#[test]
fn species_step_6_returns_a_constructor_and_the_matcher_is_built_from_it() {
    assert_eq!(
        eval_display(
            "var r = /\\w/g; r.constructor = { [Symbol.species]: RegExp }; \
             r[Symbol.matchAll]('a*b').length"
        ),
        "2"
    );
    // `Construct(%RegExp%, « R, flags »)` must clone `R`'s source *and* flags,
    // so the drain finds `R`'s matches and not the receiver's whole text.
    assert_eq!(
        eval_display(
            "var r = /\\d/g; \
             r.constructor = { [Symbol.species]: RegExp }; \
             r[Symbol.matchAll]('a1b').map(function(m) { return m[0]; }).join(',')"
        ),
        "1"
    );
}

/// Steps 1 and 4 are `Get`s, so a throwing getter on `constructor` or on
/// `@@species` should propagate — that is what
/// `Symbol.matchAll/species-constructor-get-constructor-throws.js` and
/// `…/species-constructor-get-species-throws.js` assert.
///
/// **Neither works, and both are asserted as a known gap rather than as
/// correct behaviour.** `internal_methods::invoke_accessor_getter` answers
/// `undefined` for a *bytecode* getter: running bytecode needs the interpreter,
/// which a `&mut Ctx` native cannot re-enter, so the read falls through to
/// "property absent" — step 1 selects the default, step 4 selects the default,
/// and neither throw is observed. The accessor *is* reachable from a script
/// (`r.constructor[Symbol.species]` calls it), which is how the gap stays
/// invisible outside `@@matchAll`.
///
/// The fix is to route the read through the interpreter's `get_property`,
/// i.e. `crates/v12-interp/**`. These two assertions fail today **on purpose**,
/// so the gap is visible if it ever changes; flip them to `"ERR:RangeError: \
/// boom"` when that lands.
#[test]
fn throwing_constructor_and_species_getters_are_a_documented_gap() {
    assert_eq!(
        eval_display(
            "var r = /./; \
             Object.defineProperty(r, 'constructor', { get(){ throw new RangeError('boom'); } }); \
             var out; try { r[Symbol.matchAll](''); out = 'ok'; } \
             catch (e) { out = 'ERR:' + e; } out"
        ),
        "ok",
        "step 1's `Get` cannot invoke a bytecode getter from a `&mut Ctx` native"
    );
    assert_eq!(
        eval_display(
            "var r = /./; \
             r.constructor = { get [Symbol.species](){ throw new RangeError('boom'); } }; \
             var out; try { r[Symbol.matchAll](''); out = 'ok'; } \
             catch (e) { out = 'ERR:' + e; } out"
        ),
        "ok",
        "step 4's `Get` cannot invoke a bytecode getter either"
    );
    // The accessor really is reachable from script, so this is the internal
    // read path and not the accessor itself.
    assert_eq!(
        eval_display(
            "var o = { get [Symbol.species](){ return RegExp; } }; \
             var out; try { o[Symbol.species]; out = 'ok'; } catch (e) { out = 'ERR'; } out"
        ),
        "ok"
    );
    // A *data* property at `@@species` is read correctly, so the gap is
    // specifically the accessor path and not the `@@species` key lookup.
    assert_eq!(
        match_all_with_constructor("{ [Symbol.species]: 0 }"),
        "TypeError"
    );
}

/// `@@matchAll` reads the receiver's `lastIndex` once and drives a *separate*
/// matcher with it, leaving `R.lastIndex` untouched — ES 22.2.6.9's
/// `Set(matcher, "lastIndex", lastIndex, true)`. Asserted by
/// `Symbol.matchAll/this-lastindex-cached.js` in the corpus.
#[test]
fn match_all_copies_last_index_onto_a_fresh_matcher() {
    assert_eq!(
        eval_display(
            "var r = /./g; r.lastIndex = 2; \
             var it = r[Symbol.matchAll]('abcd'); \
             r.lastIndex = 0; \
             it.map(function(m){ return m[0]; }).join('')"
        ),
        "cd"
    );
    assert_eq!(
        eval_display(
            "var r = /./g; r.lastIndex = 2; \
             r[Symbol.matchAll]('abcd'); r.lastIndex"
        ),
        "2",
        "the drain must not write through to the receiver's own `lastIndex`"
    );
}

/// ES 22.2.6.11 `RegExp.prototype[@@replace]` has no `SpeciesConstructor` step
/// (an earlier draft did; it was removed), so a receiver with a non-object
/// `constructor` must replace normally instead of throwing.
#[test]
fn replace_does_not_select_a_species_constructor() {
    for ctor in ["null", "1", "\"s\"", "Symbol()"] {
        assert_eq!(
            eval_display(&format!(
                "var r = /x/g; r.constructor = {ctor}; r[Symbol.replace]('axb', 'y')"
            )),
            "ayb",
            "`@@replace` must not call SpeciesConstructor (`constructor = {ctor}`)"
        );
    }
}

/// The gap `construct_species_matcher` documents: a user `@@species` is a
/// bytecode closure, and `Construct` on it needs the interpreter's construct
/// seam, which a `&mut Ctx` native cannot reach. The receiver is used as the
/// matcher instead, so a species constructor that returns a *different
/// pattern* is not honoured.
///
/// Asserted as a known gap for the same reason as the step-4 getter above —
/// it fails today on purpose. `Symbol.matchAll/species-constructor.js` and
/// `Symbol.split/species-ctor.js` are the corpus tests it will flip.
#[test]
fn the_documented_species_constructor_gap_is_untested_yet() {
    // The species constructor returns /\w/g, which would match "a" in "a*b".
    // The answer is `null` because the *receiver* (\d) is used as the matcher.
    // Flip this to "a" when `Construct` becomes reachable from a `&mut Ctx`
    // native; `Symbol.matchAll/species-constructor.js` is the corpus test.
    assert_eq!(
        eval_display(
            "var r = /\\d/u; \
             r.constructor = { [Symbol.species]: function(){ return /\\w/g; } }; \
             String(r[Symbol.matchAll]('a*b'))"
        ),
        "null",
        "the receiver is used as the matcher because `Construct` is not \
         reachable from a `&mut Ctx` native; see `construct_species_matcher`"
    );
}
