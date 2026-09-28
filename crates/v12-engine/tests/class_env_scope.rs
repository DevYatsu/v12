//! A class scope with a heritage (`class T extends X`) needs a real
//! environment to hold the parent constructor for `super`, and
//! `NewEnvironment` installs it as the frame's innermost environment with no
//! matching pop — there is no `PopEnvironment` opcode. Nothing ever retires
//! that head, so for the rest of the unit it shadows the unit's own
//! environment.
//!
//! Two distinct symptoms follow, and both are **silent** because the class
//! scope holds exactly one slot (the parent constructor), so a misdirected
//! `GetEnvSlot`/`SetEnvSlot` at `depth=0` reads back a constructor instead of
//! raising:
//!
//! 1. accesses emitted by the *enclosing* unit after the class statement
//!    resolve one hop too shallow (`acc` reads back `Object`);
//! 2. a closure created after the class statement captures the class scope as
//!    its root environment (`Closure` takes `frame.env` — see
//!    `v12-interp/src/execute.rs`), so *inside* that closure every env access
//!    resolves one hop too shallow.
//!
//! Symptom 2 is why a compile-time depth bias is not a sufficient fix: the
//! closure's captured root genuinely *is* the class scope, so no static
//! correction to the enclosing unit's depths can reach it. The fix has to
//! retire the environment, which needs a `PopEnvironment` opcode — currently
//! blocked on write scope for `crates/v12-bytecode/src/opcode.rs` (the `Opcode`
//! enum and its `from_u8` decoder live there, not in `lib.rs`).
//!
//! The four active tests are the negative controls: they pass today and must
//! keep passing, so a future fix that over-corrects (breaking `super()`, or
//! perturbing the two non-triggering shapes) is caught.

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
/// an extending class reads back the parent constructor. Today: `function() {}`.
#[test]
#[ignore = "class scope is never popped; needs a PopEnvironment opcode"]
fn closure_write_after_extending_class_reads_back_written_value() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } class T extends Object { m() {} } bump(); return acc; } h()"
        ),
        "1"
    );
}

/// Symptom 1, repeated: the write was always fine, only the read-back was
/// corrupted, so assert the value survives several mutations. Today:
/// `function() {}`.
#[test]
#[ignore = "class scope is never popped; needs a PopEnvironment opcode"]
fn closure_writes_accumulate_across_repeated_calls() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } class T extends Object { m() {} } bump(); bump(); bump(); return acc; } h()"
        ),
        "3"
    );
}

/// Symptom 1 with two class scopes, so any fix has to handle more than one
/// outstanding head. Today: `function() {}`.
#[test]
#[ignore = "class scope is never popped; needs a PopEnvironment opcode"]
fn two_extending_classes_accumulate_the_bias() {
    assert_eq!(
        eval_display(
            "function h() { let acc = 0; function bump() { acc = acc + 1; } class A extends Object { m() {} } class B extends Object { m() {} } bump(); return acc; } h()"
        ),
        "1"
    );
}

/// A class *expression* takes the same `class_expression` path, so it leaks the
/// same way. Today: `function() {}`.
#[test]
#[ignore = "class scope is never popped; needs a PopEnvironment opcode"]
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
/// misdirected read makes `T` extend `undefined`. Today: throws
/// `TypeError: prototype must be an object or null`.
#[test]
#[ignore = "class scope is never popped; needs a PopEnvironment opcode"]
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
/// class scope as its root environment. Today: `undefined`.
#[test]
#[ignore = "class scope is never popped; needs a PopEnvironment opcode"]
fn closure_created_after_class_captures_the_right_environment() {
    assert_eq!(
        eval_display(
            "function f() { let iterator = { tag: 'real' }; class T extends Object { m() {} } const get = function () { return iterator.tag; }; return get(); } f()"
        ),
        "real"
    );
}
