//! Declared arity: the `length`/`name` own properties of the built-in
//! functions on `%Function.prototype%`, and the `length`/`name` a *bound*
//! function computes from its target.
//!
//! `Function.prototype.call`, `.apply` and `.bind` were the only three
//! built-ins with **no** declared arity anywhere in the engine. They have no
//! `define_builtins!` entry (their bodies live in the interpreter router,
//! because `call`/`apply` must re-enter the machine), so nothing consulted
//! `builtin_length` for them and nothing installed them: the only way to reach
//! them was the interpreter's synthesized `function_method_surface`, which
//! allocated a bare function object per read — no `length`, no `name`, and no
//! identity (`Function.prototype.call !== Function.prototype.call`).
//!
//! The second half is the bound function's own `length`/`name`, which
//! `BoundFunctionCreate` (ES 10.4.1.1) computes. Two independent defects there:
//!
//! * `length` was read as a **Smi**, so a target whose own `length` is
//!   `2147483648`, `Number.MAX_SAFE_INTEGER` or `Infinity` (all legal — a
//!   function's `length` is writable user data) collapsed to `0`, and the
//!   result was re-narrowed through `as i32`.
//! * `name` for a non-String target name was `"bound"` instead of `"bound "`.
//!
//! and one blocker underneath both: `Object.defineProperty` could not change the
//! value of a property that is **configurable but not writable**, which is
//! precisely the shape of a function's `length`. Without that fix the tests
//! could not even set up their precondition.
//!
//! Two negative controls pin what this lane did *not* do: the `@@toStringTag`
//! method is still absent (it needs real `IsCallable` semantics, not an
//! install), and a `[[Set]]` assignment to a non-writable `length` is still
//! refused — the `length` a built-in function carries is not writable by
//! assignment, only by `defineProperty`.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

// --- declared arity of the %Function.prototype% methods --------------------

/// `Function.prototype.apply.length` is 2 (ES 10.2.3).
#[test]
fn apply_declares_length_two() {
    assert_eq!(eval_display("Function.prototype.apply.length"), "2");
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(Function.prototype.apply, 'length'); \
             d.writable === false && d.enumerable === false && d.configurable === true"
        ),
        "true"
    );
}

/// `Function.prototype.call.length` is 1 (ES 10.2.4).
#[test]
fn call_declares_length_one() {
    assert_eq!(eval_display("Function.prototype.call.length"), "1");
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(Function.prototype.call, 'length'); \
             d.writable === false && d.enumerable === false && d.configurable === true"
        ),
        "true"
    );
}

/// `Function.prototype.bind.length` is 1 (ES 10.2.5).
#[test]
fn bind_declares_length_one() {
    assert_eq!(eval_display("Function.prototype.bind.length"), "1");
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(Function.prototype.bind, 'length'); \
             d.writable === false && d.enumerable === false && d.configurable === true"
        ),
        "true"
    );
}

/// Each method carries its own `name` (`built-ins/Function/prototype/*/name.js`).
#[test]
fn the_methods_carry_their_names() {
    assert_eq!(eval_display("Function.prototype.call.name"), "call");
    assert_eq!(eval_display("Function.prototype.apply.name"), "apply");
    assert_eq!(eval_display("Function.prototype.bind.name"), "bind");
}

/// The install is `{ writable: true, enumerable: false, configurable: true }`
/// on `%Function.prototype%` — `built-ins/Function/prototype/call/prop-desc.js`
/// and its `apply`/`bind` twins.
#[test]
fn the_methods_are_writable_but_not_enumerable() {
    assert_eq!(
        eval_display(
            "var d = Object.getOwnPropertyDescriptor(Function.prototype, 'call'); \
             d.writable === true && d.enumerable === false && d.configurable === true \
               && typeof d.value === 'function'"
        ),
        "true"
    );
}

/// The install is a *real* property, so identity holds and the value survives a
/// `getOwnPropertyDescriptor` round trip — the synthesized alternative
/// produced a different object on every read.
#[test]
fn the_installed_methods_have_stable_identity() {
    assert_eq!(
        eval_display("Function.prototype.call === Function.prototype.call"),
        "true"
    );
    assert_eq!(
        eval_display(
            "Object.getOwnPropertyDescriptor(Function.prototype, 'call').value \
               === Function.prototype.call"
        ),
        "true"
    );
}

/// An *unlinked* closure still reads `f.call` through the synthesized surface —
/// but the surface now serves the real `%Function.prototype%.call`, which is the
/// object the spec's prototype chain would hand back. So `f.call.length` and
/// `f.call === f.call` both hold even though `f` has no link
/// (`built-ins/Function/prototype/call/S15.3.4.4_A2_T2.js`).
#[test]
fn an_unlinked_closure_reads_the_real_method() {
    assert_eq!(eval_display("function f() {} f.call.length"), "1");
    assert_eq!(eval_display("function f() {} f.call === f.call"), "true");
}

// --- the bound function's computed length ----------------------------------

/// `length` is `max(0, ToIntegerOrInfinity(target.length) - argCount)`.
#[test]
fn bound_length_subtracts_the_bound_arguments() {
    assert_eq!(
        eval_display("function f(a, b, c) {} f.bind(null, 1).length"),
        "2"
    );
    assert_eq!(
        eval_display("function f(a, b, c) {} f.bind(null, 1, 2, 3).length"),
        "0"
    );
}

/// A function's `length` is writable user data, so it can exceed the Smi range.
/// The old read was Smi-only and answered `0` for all of these.
#[test]
fn bound_length_survives_values_past_the_smi_range() {
    assert_eq!(
        eval_display(
            "function f() {} Object.defineProperty(f, 'length', { value: 2147483648 }); \
             f.bind().length"
        ),
        "2147483648"
    );
    assert_eq!(
        eval_display(
            "function f() {} \
             Object.defineProperty(f, 'length', { value: Number.MAX_SAFE_INTEGER }); \
             f.bind().length"
        ),
        "9007199254740991"
    );
}

/// `SetFunctionLength` (ES 10.2.4) clamps: `+Infinity` becomes 2^53-1, `NaN`
/// becomes +0, and `ToIntegerOrInfinity` truncates toward zero.
#[test]
fn bound_length_clamps_and_truncates() {
    let cases = [
        ("Infinity", "9007199254740991"),
        ("-Infinity", "0"),
        ("NaN", "0"),
        ("3.7", "3"),
        ("-3.7", "0"),
    ];
    for (input, want) in cases {
        assert_eq!(
            eval_display(&format!(
                "function f() {{}} Object.defineProperty(f, 'length', {{ value: {input} }}); \
                 f.bind().length"
            )),
            want,
            "target length {input}"
        );
    }
}

/// Step 14: a non-String target name becomes the empty string, so the result is
/// `"bound "` — with its trailing space.
#[test]
fn bound_name_of_a_non_string_target_name_has_a_trailing_space() {
    assert_eq!(
        eval_display(
            "var t = Object.defineProperty(function() {}, 'name', { value: undefined }); \
             '[' + t.bind().name + ']'"
        ),
        "[bound ]"
    );
    assert_eq!(
        eval_display(
            "var t = Object.defineProperty(function() {}, 'name', { value: 42 }); \
             '[' + t.bind().name + ']'"
        ),
        "[bound ]"
    );
    // …and a String one is prefixed, including through a second bind.
    assert_eq!(
        eval_display(
            "var t = Object.defineProperty(function() {}, 'name', { value: 'target' }); \
             t.bind().name + ' / ' + t.bind().bind().name"
        ),
        "bound target / bound bound target"
    );
}

// --- the defineProperty blocker -------------------------------------------

/// `ValidateAndApplyPropertyDescriptor` step 5 runs only for a
/// **non-configurable** current property, so a configurable one accepts any
/// `[[Value]]` — including when it is not writable. That is the shape of a
/// function's `length`, and the precondition every bound-`length` test above
/// depends on.
#[test]
fn define_property_can_change_a_configurable_non_writable_value() {
    assert_eq!(
        eval_display(
            "var o = {}; \
             Object.defineProperty(o, 'x', { value: 1, writable: false, configurable: true }); \
             Object.defineProperty(o, 'x', { value: 2 }); o.x"
        ),
        "2"
    );
    // A *non-configurable*, non-writable property still refuses, and still
    // reports failure rather than silently dropping the value.
    assert_eq!(
        eval_display(
            "var o = {}; \
             Object.defineProperty(o, 'x', { value: 1, writable: false, configurable: false }); \
             Object.defineProperty(o, 'x', { value: 2 })"
        ),
        "threw: TypeError: Cannot redefine property: Invalid property definition"
    );
}

// --- negative controls -----------------------------------------------------

/// Negative control: `Function.prototype[@@hasInstance]` is still absent. It
/// needs real `IsCallable`/`OrdinaryHasInstance` semantics behind it, not an
/// install, and installing one would turn a `TypeError` into a wrong answer.
#[test]
fn symbol_has_instance_is_still_absent() {
    assert_eq!(
        eval_display("Function.prototype[Symbol.hasInstance]"),
        "undefined"
    );
}

/// Negative control: `[[Set]]` is unchanged. A built-in function's `length` is
/// not writable by assignment, only by `defineProperty` — the descriptor is
/// what `define_property_can_change_a_configurable_non_writable_value` reads.
#[test]
fn assignment_cannot_change_a_builtin_functions_length() {
    assert_eq!(
        eval_display(
            "function f() {} f.length = 9; \
             f.length === 0 && Function.prototype.call.length === 1"
        ),
        "true"
    );
}
