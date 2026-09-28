//! A `let`/`const` loop variable that is captured by a closure lives in an
//! Environment, and the spec gives every iteration its own binding instance
//! (`CreatePerIterationEnvironment`). The compiler models that by pushing a
//! fresh environment per iteration and copying every slot forward.
//!
//! The push was never matched by a pop, so the runtime chain grew one hop
//! **per runtime iteration** while the compiler's static depth model stayed
//! fixed. That is not a bias bug and cannot be fixed by one: a per-iteration
//! environment's parent is the *previous iteration's* environment, so the
//! chain depth is only known while running. The fix is a real `PopEnvironment`
//! on every path out of the body.
//!
//! The symptom is silent, because the copy-forward copies *all* slots, so a
//! read at static depth 0 after the loop still finds a slot — it just finds the
//! last iteration's loop environment instead of the function's own. Writes
//! made from a closure that captured the function environment (so it resolves
//! the real binding) become invisible to reads made inside the loop:
//!
//! ```js
//! function f() { let g = []; let acc = 0; function bump() { acc = acc + 1; }
//!                 for (const v of [1, 2]) { g.push(() => v); bump(); }
//!                 return acc; }        // spec: 2 — was: 0
//! ```
//!
//! `return` and a `throw` that escapes the frame need no pop: `Return` pops
//! the frame outright (`Interp::complete_frame`) and an escaping exception
//! unwinds it, so the chain dies with the frame. `break` and `continue` are
//! the paths that keep running in the same frame, and they are the ones this
//! file pins down — including labeled forms, which pop once per exited loop.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// A `for-of` body must not leave its per-iteration environment behind: a
/// closure created *before* the loop writes the real `acc`, and a read after
/// the loop must see every increment.
#[test]
fn for_of_body_does_not_leak_its_per_iteration_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (const v of [1, 2]) { bump(); } return acc; } f()"
        ),
        "2"
    );
}

/// Same for `for-in`, which shares the helper.
#[test]
fn for_in_body_does_not_leak_its_per_iteration_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (let k in { a: 1, b: 2 }) { bump(); } return acc; } f()"
        ),
        "2"
    );
}

/// Three iterations, so a chain that grew but was still shallow enough to read
/// would be caught: the defect is per-iteration, not once per loop.
#[test]
fn leak_does_not_accumulate_over_many_iterations() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (const v of [1, 2, 3, 4, 5, 6, 7, 8]) { bump(); } return acc; } f()"
        ),
        "8"
    );
}

/// The loop variable itself must still be per-iteration (spec 22.2.2.5) —
/// this is what the environment is *for*, and it was already correct. Asserted
/// so a pop that retires the environment too eagerly (before the body, or
/// before the copy-forward) is caught rather than silently fixing the leak.
#[test]
fn for_of_binding_is_still_per_iteration_after_the_pop() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (const v of [1, 2, 3]) { g.push(() => v); } return [g[0](), g[1](), g[2]()].join(','); } f()"
        ),
        "1,2,3"
    );
}

/// `for-in` counterpart of the per-iteration binding test.
#[test]
fn for_in_binding_is_still_per_iteration_after_the_pop() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (const k in { a: 1, b: 2 }) { g.push(() => k); } return [g[0](), g[1]()].join(','); } f()"
        ),
        "a,b"
    );
}

/// `break` leaves the body with the environment still live, so the pop has to
/// be emitted on that path too.
#[test]
fn break_out_of_for_of_body_pops_the_per_iteration_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (const v of [1, 2, 3]) { bump(); break; } return acc; } f()"
        ),
        "1"
    );
}

/// `break` must also leave the *chain* intact for the code after the loop, not
/// just pop once: two bumps after a `break` both have to land.
#[test]
fn code_after_a_break_in_for_of_still_resolves_the_enclosing_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (const v of [1, 2, 3]) { bump(); break; } bump(); bump(); return acc; } f()"
        ),
        "3"
    );
}

/// `continue` jumps to the loop's `cont` label, from where the loop re-enters
/// at the top and pushes *another* environment — so `continue` has to pop too,
/// or the chain grows once per `continue`.
#[test]
fn continue_in_for_of_body_pops_the_per_iteration_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (const v of [1, 2, 3]) { bump(); if (v < 3) continue; } return acc; } f()"
        ),
        "3"
    );
}

/// A labeled `break` resolves the same way but through the label machinery,
/// and the pop count is driven by the loop stack, so it is pinned separately.
#[test]
fn labeled_break_out_of_for_of_pops_the_per_iteration_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } outer: for (const v of [1, 2, 3]) { bump(); break outer; } return acc; } f()"
        ),
        "1"
    );
}

/// Nested loops: the inner `break` must pop exactly one environment, and the
/// outer loop's own must survive it — an over-eager pop would retire the
/// outer environment and corrupt everything after the outer loop too.
#[test]
fn nested_loops_pop_exactly_the_exited_iteration_environments() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (const a of [1, 2]) { for (const b of [1, 2, 3]) { bump(); break; } } return acc; } f()"
        ),
        "2"
    );
}

/// Negative control: an *uncaptured* loop variable never gets a per-iteration
/// environment (it is a register local, invisible to closures), so a pop
/// emitted unconditionally would underflow the chain and break this. It must
/// pass both before and after the fix.
#[test]
fn uncaptured_loop_variable_is_unaffected() {
    assert_eq!(
        eval_display("function f() { let acc = 0; for (const v of [1, 2, 3]) { acc = acc + v; } return acc; } f()"),
        "6"
    );
}

/// Negative control: a `var` loop variable is deliberately *not* per-iteration
/// (spec gives `var` no `perIterationBindings`), so it must alias and keep
/// aliasing.
#[test]
fn var_loop_variable_still_aliases() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (var v = 0; v < 3; v++) { g.push(() => v); } return [g[0](), g[1](), g[2]()].join(','); } f()"
        ),
        "3,3,3"
    )
}

/// Negative control: a loop with no captured binding at all, run long enough
/// that an unconditional per-iteration pop would exhaust the chain.
#[test]
fn long_loop_without_captured_bindings_is_unaffected() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; for (let i = 0; i < 1000; i++) { acc = acc + 1; } return acc; } f()"
        ),
        "1000"
    )
}
