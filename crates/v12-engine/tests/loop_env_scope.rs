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

// ---------------------------------------------------------------------------
// C-style `for` (D2): the loop form that had no per-iteration environment at
// all.
//
// `for-of` and `for-in` call `emit_per_iteration_env`; `for_loop` never did, so
// a captured `let` counter was one slot aliased across every iteration and
// every closure saw the final value:
//     for (let i = 0; i < 3; i++) g.push(() => i)   // 3,3,3 -- spec: 0,1,2
// The fix reuses D1's machinery wholesale: same push, same bias rule, same
// `LoopCtx::per_iter_env` pop on every exit.
// ---------------------------------------------------------------------------

/// The defect itself: a captured `for` counter is per-iteration (spec
/// `ForBodyEvaluation`'s `perIterationBindings`).
#[test]
fn for_statement_binding_is_per_iteration() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (let i = 0; i < 3; i++) { g.push(() => i); } return [g[0](), g[1](), g[2]()].join(','); } f()"
        ),
        "0,1,2"
    );
}

/// Two captured bindings in one head: the gate checks *every* declarator, so a
/// first-declarator-only check would regress this.
#[test]
fn for_statement_with_two_captured_bindings_is_per_iteration() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (let i = 0, j = 10; i < 3; i++, j++) { g.push(() => i + ':' + j); } return [g[0](), g[1](), g[2]()].join(','); } f()"
        ),
        "0:10,1:11,2:12"
    );
}

/// A `const` head is a per-iteration binding too, and the update expression
/// cannot assign to it -- so this also pins that the gate is the declaration
/// *kind*, not assignability.
#[test]
fn const_for_statement_binding_is_per_iteration() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (const c = 7, n = 0; n < 3; n++) { g.push(() => c); } return [g[0](), g[1](), g[2]()].join(','); } f()"
        ),
        "7,7,7"
    )
}

/// The `continue` path, and the reason the push sits *after* the test: `i` has
/// to be incremented exactly once per iteration, which only holds if the update
/// expression runs against the enclosing environment the test also read.
#[test]
fn continue_in_for_statement_still_advances_the_counter_once() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (let i = 0; i < 4; i++) { if (i === 2) continue; g.push(() => i); } return [g[0](), g[1](), g[2]()].join(','); } f()"
        ),
        "0,1,3"
    );
}

/// `break` out of a C-style `for`: the pop has to be on that path too, or the
/// rest of the frame resolves one hop too shallow.
#[test]
fn break_out_of_for_statement_pops_the_per_iteration_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } for (let i = 0; i < 5; i++) { bump(); break; } bump(); return acc; } f()"
        ),
        "2"
    )
}

/// A `for` with no test (`for (;;)`) has no exit test, so the push is
/// unconditional there; the `break` is the only way out and must pop.
#[test]
fn infinite_for_statement_break_pops_the_per_iteration_environment() {
    assert_eq!(
        eval_display(
            "function f() { let acc = 0; function bump() { acc = acc + 1; } let i = 0; for (;;) { bump(); i++; if (i === 3) break; } return acc; } f()"
        ),
        "3"
    )
}

/// Nested C-style `for`s with a labeled `break` and a labeled `continue`: two
/// per-iteration environments are created, and each exit has to retire the
/// right number of them.
#[test]
fn nested_for_statements_pop_exactly_the_exited_environments() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; outer: for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { g.push(() => i + ':' + j); if (j === 1) continue outer; } } return g.map(function (fn) { return fn(); }).join(','); } f()"
        ),
        "0:0,0:1,1:0,1:1,2:0,2:1"
    )
}

/// Negative control: an uncaptured C-style `for` counter is a register local
/// and must never get an environment.
#[test]
fn uncaptured_for_statement_counter_is_unaffected() {
    assert_eq!(
        eval_display("function f() { let acc = 0; for (let i = 0; i < 100; i++) { acc = acc + i; } return acc; } f()"),
        "4950"
    )
}

/// Negative control: `var` in a C-style `for` head has no per-iteration
/// bindings and must keep aliasing (spec 14.7.4.7).
#[test]
fn var_for_statement_counter_still_aliases() {
    assert_eq!(
        eval_display(
            "function f() { let g = []; for (var i = 0; i < 3; i++) { g.push(() => i); } return [g[0](), g[1](), g[2]()].join(','); } f()"
        ),
        "3,3,3"
    )
}

/// Negative control: a captured counter mixed with a *closure created after*
/// the loop, which is the case that rules out a compile-time-only correction:
/// `Closure` captures `frame.env` implicitly, so the loop environment must
/// really be retired or this closure's depths are off by one.
#[test]
fn closure_created_after_a_for_statement_captures_the_right_environment() {
    assert_eq!(
        eval_display(
            "function f() { let tag = 'real'; let n = 0; for (let i = 0; i < 3; i++) { n = n + i; } const get = function () { return tag + n; }; return get(); } f()"
        ),
        "real3"
    )
}

