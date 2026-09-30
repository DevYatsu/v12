//! Direct-eval scope capture and **compilation determinism**.
//!
//! # Why this file exists
//!
//! Measurement was silently nondeterministic: the same binary, on the same
//! test262 source, produced different verdicts run to run. One test flipped
//! roughly 4 times in 5 runs:
//!
//! ```text
//! language/expressions/arrow-function/arrow/capturing-closure-variables-1.js
//! // var a; function foo(){ eval("a = 10"); return ()=>a; }
//! // assert.sameValue(foo()(), 10, ...)
//! ```
//!
//! and it also flipped *within a single process*, which rules out per-process
//! hash seeding as the whole story and points at a randomized hasher whose
//! iteration order differs per map instance.
//!
//! The cause was **not** the eval runtime. It is one line in the compiler's
//! eval-metadata pass: to describe a captured binding to the eval seam, the
//! compiler looks a name up in `plans.symbol_names`, a
//! `HashMap<SymbolId, String>`, with `.iter().find(|(_, n)| *n == name)`. The
//! same identifier text legitimately has one `SymbolId` per declaring scope,
//! so that lookup had several correct-by-construction candidates and returned
//! whichever the randomized hash order yielded first. When the winner was the
//! top-level `var a` the test passed; when it was an unrelated inner symbol
//! the compiler stamped the eval binding as `Register(1)` — a register in
//! `foo`'s frame that does not hold `a` — and the arrow read `undefined`.
//!
//! So the compiled `Program` itself varied between runs. Any pass-rate or
//! fail-diff number measured across runs carried this as a noise floor.
//!
//! # The determinism tests are the point
//!
//! [`repeated_compilation_is_identical`] and
//! [`repeated_evaluation_is_identical`] are the assertions that must never be
//! deleted. They are the only things in the suite that would have caught this:
//! every functional test here passes on the *unfixed* tree, because the bug
//! only shows up as run-to-run variation, never as a stable wrong answer. A
//! suite whose assertions all hold on the broken build cannot detect it.
//!
//! The engine-level determinism test deliberately evaluates the *same* source
//! N times in one process. A fresh [`Engine`] per iteration is what the
//! test262 runner does, so this matches the observed failure mode; running the
//! iterations in one process also catches order dependence between two
//! `HashMap` instances in the same thread, which a per-process test cannot.

use v12_engine::Engine;

fn result(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(value) => format!("threw: {}", engine.to_display_string(value)),
    }
}

/// The flipping test262 source, reproduced standalone.
///
/// `assert.sameValue` is replaced by a plain global read so the test does not
/// depend on the harness; the property under test is the same — the direct
/// `eval` must write through to the caller's `var a`, and the arrow created
/// afterwards must observe it.
const CAPTURED_WRITE: &str = "var a; function foo(){ eval('a = 10'); return ()=>a; } foo()()";

/// [`CAPTURED_WRITE`] plus a **sibling** declaration of the same name.
///
/// The sibling is not decoration — it is what makes the test able to fail.
/// The compiler describes a captured binding to the eval seam by looking the
/// name up in `plans.symbol_names`, a `HashMap<SymbolId, String>`, and the
/// same identifier text has one `SymbolId` per declaring scope. With a single
/// `a` there is one candidate and the buggy lookup is accidentally right; add
/// a second and it has a choice, and returns whichever the randomized hash
/// order yields first.
///
/// This mirrors what `assert.js` does to every real test262 file: it declares
/// locals that happen to share names with the test body. A trigger that only
/// works with the real harness would be a bad regression test — it could not
/// tell a fix from a change in harness contents.
const CAPTURED_WRITE_WITH_SIBLING: &str = "\
var a;
function sibling(){ var a; }
function foo(){ eval('a = 10'); return ()=>a; }
foo()()";

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// Evaluating the same source many times must give the same answer every time.
///
/// Each iteration builds a fresh [`Engine`], matching what the test262 runner
/// does, so this covers the cross-engine case rather than only the
/// within-one-engine case. Run 64 times: the pre-fix outcome was
/// probabilistic (~4 in 5 wrong), not certain, so a single comparison would
/// have a real chance of passing on the broken build.
///
/// Both source shapes are checked. [`CAPTURED_WRITE_WITH_SIBLING`] is the one
/// that can actually fail; [`CAPTURED_WRITE`] is the minimal form and guards
/// against the sibling-dependent fix regressing the simple case.
#[test]
fn repeated_evaluation_is_identical() {
    for source in [CAPTURED_WRITE_WITH_SIBLING, CAPTURED_WRITE] {
        let mut seen = Vec::new();
        for _ in 0..64 {
            seen.push(result(source));
        }
        let first = &seen[0];
        assert_eq!(
            seen.iter().filter(|v| *v == first).count(),
            seen.len(),
            "evaluation was not deterministic across engines: {source}\n  saw {seen:?}"
        );
        // Assert the *value* too. A determinism test that only checked
        // self-consistency would happily pass while every run returned the
        // same wrong answer — which is the failure mode that let this ship.
        assert_eq!(first, "10", "expected the captured write to be observable");
    }
}

/// The same source evaluated many times **within one engine and one process**.
///
/// This is the tighter form: two `HashMap` instances created in the same
/// thread get different hasher seeds, so a per-process test cannot see
/// order-dependence between them. Run 64 times because the pre-fix failure was
/// probabilistic, not certain.
#[test]
fn repeated_evaluation_in_one_engine_is_identical() {
    for source in [CAPTURED_WRITE_WITH_SIBLING, CAPTURED_WRITE] {
        let mut engine = Engine::new();
        let mut seen = Vec::new();
        for _ in 0..64 {
            match engine.eval(source) {
                Ok(value) => seen.push(engine.to_display_string(value)),
                Err(value) => seen.push(format!("threw: {}", engine.to_display_string(value))),
            }
        }
        let first = &seen[0];
        assert_eq!(
            seen.iter().filter(|v| *v == first).count(),
            seen.len(),
            "evaluation was not deterministic within one engine: {source}\n  saw {seen:?}"
        );
        assert_eq!(first, "10", "expected the captured write to be observable");
    }
}

/// Sources that all exercise the eval-metadata capture path, each of which
/// selects a captured name by *text* rather than by symbol. Any
/// name-keyed lookup over a randomized map shows up here.
///
/// This is the tightest form of the regression: it compiles the same source
/// repeatedly and compares the emitted eval metadata, with no interpreter in
/// the loop. Everything else in this file is downstream of that decision, so
/// if the compiler goes back to a name-keyed lookup over a `HashMap`, this is
/// what notices — the runtime tests could pass by luck.
#[test]
fn repeated_compilation_is_identical_over_eval_shapes() {
    use std::collections::BTreeSet;
    // The same name declared in a sibling scope, at several nesting depths,
    // with the eval'd name varying. `assert.js` creates exactly this shape.
    let sources = [
        CAPTURED_WRITE_WITH_SIBLING,
        CAPTURED_WRITE,
        "var a; function q(){ var a; } function foo(){ eval('a = 1'); } foo()",
        "var a; function q(){ function s(){ var a; } } function foo(){ eval('a = 1'); } foo()",
        "function q(){ var a; } var a; function foo(){ eval('a = 1'); } foo()",
        "var v; function f(){ let v = 1; eval('v = 2'); return v; } f()",
        "var n; function f(){ { let n = 1; eval('n = 2'); } return n; } f()",
    ];
    for source in sources {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for _ in 0..32 {
            let mut interner = v12_bccompiler::Interner::default();
            let program =
                v12_bccompiler::compile_source_with_interner(source, &mut interner)
                    .expect("source compiles");
            let mut record = String::new();
            for function in &program.functions {
                if let Some(metadata) = &function.eval_metadata {
                    // The binding list is the artifact the bug corrupted.
                    record.push_str(&format!("{:?}|", metadata.bindings));
                }
            }
            seen.insert(record);
        }
        assert_eq!(
            seen.len(),
            1,
            "compilation was not deterministic: {source}\n  saw {seen:?}"
        );
    }
}

/// Asserts the spec value of each eval shape, once the flakiness is gone.
///
/// These are the documented direct-eval semantics (ES 19.2.1.3 /
/// `EvalDeclarationInstantiation`), pinned so the determinism fix cannot be
/// "achieved" by making every eval write go to the global.
#[test]
fn direct_eval_writes_to_the_caller_scope() {
    // `var` in a direct eval is created in the caller's *variable*
    // environment, so the caller sees it and the global does not.
    assert_eq!(result("function f(){ eval('var b = 99;'); return b; } f()"), "99");
    assert_eq!(
        result("function f(){ eval('var b = 99;'); } f(); typeof b"),
        "undefined"
    );
    // ...but a top-level eval still targets the global.
    assert_eq!(result("eval('var g1 = 5'); g1"), "5");
    // A `var` in the eval is instantiated into the *caller's* variable
    // environment, so when the caller already has a same-named `var` the two
    // are one binding and the eval's assignment is visible to the caller.
    // (Verified against Node: `A eval`, not `caller`.) This is the observable
    // consequence of "created in the caller's variable environment" — it is
    // not a shadowing rule.
    assert_eq!(
        result("function f(){ var x = 'caller'; eval(\"var x = 'eval';\"); return x; } f()"),
        "eval"
    );
    // Lexical bindings are readable from a direct eval.
    assert_eq!(result("function f(){ let L = 7; return eval('L'); } f()"), "7");
    assert_eq!(result("function f(){ const C = 8; return eval('C'); } f()"), "8");
}

/// Indirect eval — `(0, eval)(...)` — must keep global-only scope.
///
/// This is the negative control for the test above. If direct and indirect
/// eval shared a code path, a fix that made direct eval write through to the
/// caller would also make this see the caller's `x`, which the spec forbids.
#[test]
fn indirect_eval_stays_global_only() {
    assert_eq!(
        result("function f(){ var x = 10; return (0, eval)('typeof x'); } f()"),
        "undefined"
    );
    assert_eq!(
        result("function f(){ var x = 10; return (0, eval)('x = 3'); } f(); x"),
        "3"
    );
    // An alias is **not** a direct eval: only the syntactic identifier `eval`
    // in call position is. `var e = eval; e(...)` compiles to an indirect
    // call, so `x` resolves against the global — seeded explicitly, because
    // [`result`] builds a fresh engine per call and a value assigned by an
    // earlier assertion would not survive. The caller's own `x` (10) must not
    // be what comes back.
    assert_eq!(
        result("globalThis.x = 42; function f(){ var x = 10; var e = eval; return e('x'); } f()"),
        "42"
    );
    // Direct eval, by contrast, sees the caller's `x` and leaves the global
    // alone. Contrast the two lines: same body, different scope.
    assert_eq!(result("function f(){ var x = 10; return eval('x'); } f()"), "10");
}
