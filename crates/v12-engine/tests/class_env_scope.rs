//! A class scope with a heritage (`class T extends X`) needs a real
//! environment to hold the parent constructor for `super`, and
//! `NewEnvironment` installs it as the frame's innermost environment. Because
//! `Closure` captures `frame.env`, that scope has to be the innermost one while
//! the constructor and method closures are built — but it also has to be
//! retired afterwards, which used to be impossible: there was no
//! `PopEnvironment` opcode, so the class scope shadowed the enclosing
//! function's own environment for the rest of the unit.
//!
//! Two distinct symptoms followed, both **silent** because the class scope
//! holds exactly one slot (the parent constructor), so a misdirected
//! `GetEnvSlot`/`SetEnvSlot` at `depth=0` read back a constructor rather than
//! raising:
//!
//! 1. accesses emitted by the enclosing unit after the class statement
//!    resolved one hop too shallow (`acc` read back `Object`);
//! 2. a closure created after the class statement captured the class scope as
//!    its root environment, so *inside* that closure every env access resolved
//!    one hop too shallow (`iterator.tag` read `undefined`).
//!
//! Symptom 2 is why a compile-time depth bias alone is not a sufficient fix:
//! the closure's captured root genuinely *is* the class scope, so no static
//! correction to the enclosing unit's depths can reach it. The fix pairs both
//! halves — `FnCtx::env_bias` shifts the depths resolved *while* the scope is
//! current (the heritage expression and computed keys are evaluated there), and
//! `PopEnvironment` retires it so everything after is back to the structural
//! depths. See `class_expression` in `v12-bccompiler`.
//!
//! The four control tests are the negative controls: they pass both before and
//! after the fix, so an over-correction (breaking `super()`, or perturbing the
//! two shapes that never triggered) is caught.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// Negative control: no class at all, so the pre-existing correct path.
#[test]
fn closure_write_without_any_class_is_unaffected() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } bump(); return acc; } h()"
        ),
        "1"
    );
}

/// Negative control: a class with no `extends` allocates no class-scope
/// environment, so it must be identical to the no-class case.
#[test]
fn class_without_extends_is_unaffected() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } class T { m() {} } bump(); return acc; } h()"
        ),
        "1"
    );
}

/// Negative control: a direct (register-resident) write is unaffected, which is
/// what made the bug look like a closure/throw problem during triage.
#[test]
fn direct_write_after_extending_class_is_unaffected() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; class T extends Object { m() {} } acc = 5; return acc; } h()"
        ),
        "5"
    );
}

/// Negative control: the class scope must keep doing its real job, so a fix
/// that over-corrects and breaks `super()` or the prototype chain is caught.
#[test]
fn super_call_still_reaches_the_parent_constructor() {
    assert_eq!(
        eval_display(
            "class Base { constructor() { this.tag = 'base'; } } class Derived extends Base { constructor() { super(); this.n = 1; } describe() { return 'derived:' + this.tag; } } new Derived().describe()"
        ),
        "derived:base"
    );
}

/// Symptom 1: a closure writing a captured `let` in a scope that also declares
#[test]
fn closure_write_after_extending_class_reads_back_written_value() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } class T extends Object { m() {} } bump(); return acc; } h()"
        ),
        "1"
    );
}

/// Symptom 1, repeated: the write was always fine, only the read-back was
/// corrupted, so assert the value survives several mutations.
#[test]
fn closure_writes_accumulate_across_repeated_calls() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } class T extends Object { m() {} } bump(); bump(); bump(); return acc; } h()"
        ),
        "3"
    );
}

/// Symptom 1 with two class scopes, so any fix has to handle more than one
#[test]
fn two_extending_classes_accumulate_the_bias() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } class A extends Object { m() {} } class B extends Object { m() {} } bump(); return acc; } h()"
        ),
        "1"
    );
}

/// A class *expression* takes the same `class_expression` path, so it leaks the
#[test]
fn class_expression_after_capture_is_unaffected() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } const C = class extends Object { m() {} }; bump(); return acc; } h()"
        ),
        "1"
    );
}

/// The heritage expression is evaluated *after* the class scope is pushed, and
/// the class scope's single slot is still `undefined` at that point, so a
/// `TypeError: prototype must be an object or null`.
#[test]
fn heritage_expression_reads_outer_captured_binding() {
    assert_eq!(
        eval_display(
            "function h() { let Base = Object; function bump() { Base = Array; } bump(); class T extends Base { m() {} } return Object.getPrototypeOf(T) === Array; } h()"
        ),
        "true"
    );
}

/// Symptom 2, and the case that rules out a compile-time depth bias: the
/// closure is created *after* the class statement, so `Closure` captures the
#[test]
fn closure_created_after_class_captures_the_right_environment() {
    assert_eq!(
        eval_display(
            "function f() { let iterator = { tag: 'real' }; class T extends Object { m() {} } const get = function () { return iterator.tag; }; return get(); } f()"
        ),
        "real"
    );
}
