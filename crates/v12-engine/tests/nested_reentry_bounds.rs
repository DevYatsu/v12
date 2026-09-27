//! Every seam that re-enters the dispatch loop recursively — a compiled
//! accessor body, an inline call, a value call, a generator resume — spends
//! *native* stack, not frame-stack budget. `MAX_CALL_DEPTH` (10 000 frames)
//! is orders of magnitude above what an 8 MiB thread stack survives, so each
//! of these cycles aborted the process with
//! `fatal runtime error: stack overflow` instead of raising a catchable
//! `RangeError`.
//!
//! Each case below killed the process before the fix; each now reports a
//! `RangeError` that user code can observe and handle.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

fn is_range_error(src: &str) -> bool {
    eval_display(&format!(
        "try {{ {src} 'no throw' }} catch (e) {{ e instanceof RangeError }}"
    )) == "true"
}

/// A getter that reads its own property: the accessor body re-enters
/// `execute` through `call_accessor_with` for every level.
#[test]
fn self_referential_getter_is_a_range_error() {
    assert!(is_range_error(
        "var o = {}; Object.defineProperty(o, 'x', { get: function () { return o.x; } }); o.x;"
    ));
}

/// Two objects whose getters call each other — the same cycle, entered from
/// two sites.
#[test]
fn mutually_recursive_getters_are_a_range_error() {
    assert!(is_range_error(
        "var a = {}, b = {};
         Object.defineProperty(a, 'x', { get: function () { return b.x; } });
         Object.defineProperty(b, 'x', { get: function () { return a.x; } });
         a.x;"
    ));
}

/// A callback that re-enters the same callback-taking builtin: the
/// `call_value_nested` seam.
#[test]
fn callback_re_entering_its_own_builtin_is_a_range_error() {
    assert!(is_range_error(
        "var f = function () { return [1].map(f); }; [1].map(f);"
    ));
}

/// The generator-resume seam, which `MAX_RESUME_DEPTH` was introduced for —
/// it only starts working once the cap is below the stack ceiling.
#[test]
fn self_resuming_generator_is_a_range_error() {
    assert!(is_range_error("function* g() { g().next(); } g().next();"));
}

/// A *bounded* accessor chain must still work: the cap counts native
/// re-entries, not prototype hops, and a long getter chain is a legitimate
/// program.
#[test]
fn long_getter_chain_still_resolves() {
    assert_eq!(
        eval_display(
            "var n = 200, objs = [];
             for (var i = 0; i < n; i++) objs.push({});
             for (var i = 0; i < n - 1; i++) {
               (function (k) {
                 Object.defineProperty(objs[k], 'x', { get: function () { return objs[k + 1].x; } });
               })(i);
             }
             Object.defineProperty(objs[n - 1], 'x', { get: function () { return 'end'; } });
             objs[0].x;"
        ),
        "end"
    );
}

/// Ordinary deep *call* recursion is still governed by `MAX_CALL_DEPTH` and
/// must not be tightened by the native-stack budget: 5 000 nested plain calls
/// are iterative in the dispatch loop and have to keep working.
#[test]
fn deep_plain_call_recursion_is_unaffected() {
    assert_eq!(
        eval_display("function f(n) { return n === 0 ? 0 : 1 + f(n - 1); } f(5000);"),
        "5000"
    );
}
