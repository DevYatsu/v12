//! A `let`/`const` binding exists from the moment its scope is entered but
//! holds no value until its *declaration* executes; a reference in between is
//! the temporal dead zone (ES §8.5.2, §9.1.1 step 2) and must raise a
//! `ReferenceError`. Before this engine had any TDZ at all, `x; let x = 1;`
//! evaluated to `undefined` and a whole `dstr` cluster of `language` tests
//! failed for want of the throw.
//!
//! The representation is a parallel "initialized" cell per guarded binding,
//! allocated by the collect pass in the *same* storage class as its value
//! (`UnitPlan::tdz_flags`): `undefined` means uninitialized, which is free
//! because registers start `undefined` per the frame ABI and `JsObject::
//! environment` fills fresh slots with `undefined`. The check is a
//! `JumpIfNullish` on that cell, emitted in `FnCtx::tdz_test`, and only where
//! the declarator does not provably dominate the reference
//! (`FnCtx::tdz_initialized`) — a reference that cannot be in a TDZ costs
//! nothing, which also keeps ordinary functions compilable by the baseline
//! JIT, whose template supports neither `JumpIfNullish` nor a `throw`.
//!
//! The three negative controls are load-bearing. Function declarations hoist
//! and are initialized at scope entry, `var` bindings are created with the
//! environment record and initialized to `undefined` there, and a `typeof` of
//! an *undeclared* name still yields `"undefined"`. Over-broad guarding —
//! guarding anything that is not specifically a `let`/`const` — breaks all
//! three at once, so they are asserted explicitly rather than left to the
//! conformance corpus.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

// ---------------------------------------------------------------------------
// Requirement 1 — a read of a `let`/`const` before its declaration throws.
// ---------------------------------------------------------------------------

/// The simplest form: the reference precedes the declarator in the same scope.
#[test]
fn read_before_declaration_throws_reference_error() {
    assert_eq!(
        eval_display("x2; let x2 = 1;"),
        "threw: ReferenceError: Cannot access 'x2' before initialization"
    );
}

/// The same reference read *after* the declarator ran is ordinary. This is the
/// control for the guard itself: a check that always threw, or one attached to
/// the wrong register, fails here rather than looking correct.
#[test]
fn read_after_declaration_is_unaffected() {
    assert_eq!(
        eval_display("let y1 = 1; (function () { return y1; })();"),
        "1"
    );
}

/// `let x;` has no initializer, so its value is `undefined` from the moment
/// the declarator runs — the binding leaves the TDZ without ever being written.
#[test]
fn declaration_without_initializer_leaves_the_tdz() {
    assert_eq!(
        eval_display("let z1; (function () { return z1; })();"),
        "undefined"
    );
    assert_eq!(
        eval_display("let z1; (function () { return z1 === undefined; })();"),
        "true"
    );
}

/// A block is its own declarative scope, so a `let` inside it is in the TDZ for
/// the rest of the block even though the block itself has started.
#[test]
fn tdz_binding_inside_a_block_throws_for_the_whole_block() {
    assert_eq!(
        eval_display("{ bl1; let bl1 = 1; }"),
        "threw: ReferenceError: Cannot access 'bl1' before initialization"
    );
    assert_eq!(
        eval_display("function f() { { return cc1; } let cc1 = 1; } f();"),
        "threw: ReferenceError: Cannot access 'cc1' before initialization"
    );
}

/// The reference is inside a *nested* closure that runs while the binding is
/// still uninitialized — the shape a plain "did the declarator execute yet?"
/// flag on the enclosing frame would get wrong.
#[test]
fn read_from_a_nested_closure_before_initialization_throws() {
    assert_eq!(
        eval_display("function f() { return (() => inner4)(); let inner4 = 1; } f();"),
        "threw: ReferenceError: Cannot access 'inner4' before initialization"
    );
}

/// The mirror image: a nested closure that is *called after* the declarator has
/// run must see the value. This is the case a captured env-resident flag has to
/// get right, and the reason the flag lives in the binding's own environment
/// rather than in the reading frame.
#[test]
fn read_from_a_nested_closure_after_initialization_succeeds() {
    assert_eq!(
        eval_display(
            "function f() { let fn; { fn = () => later5; } let later5 = 5; return fn(); } f();"
        ),
        "5"
    );
}

/// A destructuring default is evaluated while the declarator is still being
/// created, so a self-reference in it is a TDZ *read*, not a self-assignment.
/// (`let [d = d] = []` — the spec order is initializer-first, then binding.)
#[test]
fn destructuring_default_that_self_references_is_a_tdz_read() {
    assert_eq!(
        eval_display("function f() { let [dd1 = dd1] = []; return dd1; } f();"),
        "threw: ReferenceError: Cannot access 'dd1' before initialization"
    );
    assert_eq!(
        eval_display("function f() { let { ee1 } = { ee1: 3 }; return ee1; } f();"),
        "3"
    );
}

/// `PutValue` on an uninitialized binding throws as well — the reference is the
/// assignment's left-hand side, and it is evaluated before the right-hand side
/// is even needed.
#[test]
fn assignment_to_an_uninitialized_binding_throws() {
    assert_eq!(
        eval_display("function f() { ww1 = 1; let ww1 = 2; } f();"),
        "threw: ReferenceError: Cannot access 'ww1' before initialization"
    );
    assert_eq!(
        eval_display("function f() { pp1 += 1; let pp1 = 1; } f();"),
        "threw: ReferenceError: Cannot access 'pp1' before initialization"
    );
    assert_eq!(
        eval_display("function f() { pp2++; let pp2 = 1; } f();"),
        "threw: ReferenceError: Cannot access 'pp2' before initialization"
    );
    assert_eq!(
        eval_display("function f() { qq1 ||= 1; let qq1; } f();"),
        "threw: ReferenceError: Cannot access 'qq1' before initialization"
    );
    assert_eq!(
        eval_display("function f() { for (rr1 of [1]); let rr1; } f();"),
        "threw: ReferenceError: Cannot access 'rr1' before initialization"
    );
}

/// The post-declaration counterparts of the four forms above: each has to keep
/// working, so a guard on the wrong side of the update is caught.
#[test]
fn assignment_forms_after_declaration_are_unaffected() {
    assert_eq!(
        eval_display("function f() { let pp3 = 1; pp3++; return pp3; } f();"),
        "2"
    );
    assert_eq!(
        eval_display("function f() { let qq2 = 2; qq2 ||= 9; return qq2; } f();"),
        "2"
    );
    assert_eq!(
        eval_display("function f() { let [aa, bb] = [1, 2]; aa = 9; return aa + bb; } f();"),
        "11"
    );
}

/// The throw is a normal abrupt completion, so an enclosing `try` observes it.
#[test]
fn tdz_read_inside_try_is_catchable() {
    assert_eq!(
        eval_display(
            "function f() { try { return tt1; } catch (e) { return e.name; } let tt1 = 1; } f()"
        ),
        "ReferenceError"
    );
}

// ---------------------------------------------------------------------------
// Requirement 2 — `const` without an initializer is a *compile-time*
// SyntaxError, never a runtime ReferenceError.
// ---------------------------------------------------------------------------

/// oxc rejects `const x;` in the parser, so the engine never reaches a TDZ
/// check for it: the failure is a diagnostic about the program text, not about
/// a binding's state. Asserted here so the distinction from requirement 1 stays
/// explicit — a compiler that "fixed" this by turning it into a runtime throw
/// would satisfy a naive test and be wrong.
#[test]
fn const_without_initializer_is_rejected_before_execution() {
    let out = eval_display("const c1;");
    assert!(
        out.contains("Missing initializer in const declaration"),
        "expected a parse-time diagnostic, got {out:?}"
    );
    assert!(
        !out.contains("before initialization"),
        "a `const` with no initializer must not surface as a TDZ error: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// Requirement 3 — `typeof` is a read, not a safe probe.
// ---------------------------------------------------------------------------

/// `typeof x` in the TDZ throws. The common mistake is routing `typeof` through
/// the lenient global path (which exists for *undeclared* names) and thereby
/// reporting `"undefined"` for a binding that exists but has no value yet.
#[test]
fn typeof_in_the_tdz_throws_rather_than_reporting_undefined() {
    assert_eq!(
        eval_display("typeof x3; let x3 = 1;"),
        "threw: ReferenceError: Cannot access 'x3' before initialization"
    );
}

#[test]
fn typeof_after_declaration_is_unaffected() {
    assert_eq!(
        eval_display("function f() { let t3 = 1; return typeof t3; } f();"),
        "number"
    );
}

// ---------------------------------------------------------------------------
// Requirement 4 (negative control) — function declarations hoist.
// ---------------------------------------------------------------------------

/// Function declarations are initialized when the scope is entered (ES §9.2.12),
/// so a call above the declaration must reach the closure. This is the control
/// that catches a guard applied to "any lexically-scoped binding" rather than to
/// `let`/`const` specifically.
#[test]
fn function_declarations_are_not_in_the_tdz() {
    assert_eq!(
        eval_display("hoist4(); function hoist4() { return 42; }"),
        "42"
    );
    assert_eq!(
        eval_display("function u4() { g4(); function g4() { return 7; } return g4(); } u4();"),
        "7"
    );
    assert_eq!(
        eval_display("function u4b() { return typeof g4b; function g4b() {} } u4b();"),
        "function"
    );
}

/// A `let` that shares a name with a hoisted function is still its own binding:
/// the guard must fire for the `let` and only for the `let`.
#[test]
fn let_shadowing_a_function_declaration_is_still_guarded() {
    assert_eq!(
        eval_display("function h1() { g1(); let g1 = 1; } h1();"),
        "threw: ReferenceError: Cannot access 'g1' before initialization"
    );
}

// ---------------------------------------------------------------------------
// Requirement 5 (negative control) — `var` is never in the TDZ.
// ---------------------------------------------------------------------------

/// `var` bindings are created with the function's environment record and
/// initialized to `undefined` there, so a read above the assignment yields
/// `undefined` rather than throwing.
#[test]
fn var_is_never_in_the_tdz() {
    assert_eq!(
        eval_display("var v1; (function () { return v1; })();"),
        "undefined"
    );
    assert_eq!(
        eval_display("function f() { return v2; var v2 = 5; } f();"),
        "undefined"
    );
}

/// Same for an undeclared name: `typeof` on it is the one genuinely lenient
/// read in the language and must stay lenient.
#[test]
fn typeof_an_undeclared_name_is_still_lenient() {
    assert_eq!(eval_display("typeof nope_undeclared_zz;"), "undefined");
}

// ---------------------------------------------------------------------------
// Coverage guards: the paths that must keep working at all.
// ---------------------------------------------------------------------------

/// Ten `let` bindings in one function, every one flagged. A flag register that
/// collides with a value register (the layout allocates them interleaved) shows
/// up here as a `NaN` rather than as an exception, which is why this exists
/// separately from the single-binding cases above.
#[test]
fn many_guarded_bindings_keep_distinct_storage() {
    assert_eq!(
        eval_display(
            "function f() { let a1=1,a2=2,a3=3,a4=4,a5=5,a6=6,a7=7,a8=8,a9=9,a10=10; \
             return a1+a2+a3+a4+a5+a6+a7+a8+a9+a10; } f();"
        ),
        "55"
    );
}

/// Loop heads declare a binding too, and a `for`-head declarator runs on every
/// path into the body — so the counter's own reads must stay unguarded. A
/// missing "end the TDZ" at a `for-of`/`for-in` head would make every read of
/// the loop variable throw.
#[test]
fn loop_head_bindings_are_usable_inside_the_body() {
    assert_eq!(
        eval_display(
            "function f() { let n = 0; for (let i = 0; i < 3; i++) { n += i; } return n; } f();"
        ),
        "3"
    );
    assert_eq!(
        eval_display(
            "function f() { let g = 0; for (const v of [1, 2, 3]) { g += v; } return g; } f();"
        ),
        "6"
    );
    assert_eq!(
        eval_display(
            "function f() { let g = 0; for (const k in {a: 1, b: 2}) { g += 1; } return g; } f();"
        ),
        "2"
    );
}

/// `switch` is the one construct whose case clauses share a block scope, so a
/// binding declared under one case is genuinely in the TDZ under another.
#[test]
fn switch_clauses_share_one_scope() {
    assert_eq!(
        eval_display("function f() { switch (1) { case 2: let s1 = 1; case 1: return s1; } } f();"),
        "threw: ReferenceError: Cannot access 's1' before initialization"
    );
    assert_eq!(
        eval_display("function f() { switch (1) { case 1: let s2 = 1; return s2; } } f();"),
        "1"
    );
}
