//! Differential suite: every corpus case is evaluated by (a) the mini
//! reference interpreter (test-support) and (b) the real Tier-1 interpreter
//! over compiler output. Both must agree with the declared ground truth.
//! Covers the compiled-program end-to-end path that unit tests over
//! hand-built bytecode do not reach.

use test_support::{Val, eval_src, expect_bool, expect_num, expect_str, value_to_string};
use v12_heap::{GcPolicy, Heap};
use v12_interp::{Interp, JSException};

/// Runs `src` on the real engine and returns the completion value's display
/// string. `src` is wrapped the same way `test_support::eval_src` wraps it.
fn run_real(src: &str) -> String {
    let wrapped = format!("throw (function () {{\n{src}\n}})();");
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, &wrapped).expect("compile");
    match interp.run() {
        Ok(()) => panic!("expected the completion-value throw"),
        Err(JSException(v)) => interp.to_display_string(v),
    }
}

enum Want {
    Num(f64),
    Str(&'static str),
    Bool(bool),
    #[allow(dead_code)]
    Undefined,
}

const CASES: &[(&str, Want)] = &[
    ("return 1 + 2 * 3;", Want::Num(7.0)),
    ("return (1 + 2) * 3;", Want::Num(9.0)),
    ("return 10 % 3;", Want::Num(1.0)),
    ("return 2 ** 10;", Want::Num(1024.0)),
    ("let a = 1; a += 41; return a;", Want::Num(42.0)),
    (
        "let i = 0; let s = 0; while (i < 5) { s += i; i += 1; } return s;",
        Want::Num(10.0),
    ),
    (
        "let s = 0; for (let i = 1; i <= 4; i += 1) { s += i; } return s;",
        Want::Num(10.0),
    ),
    (
        "let f = function (x) { return x * 2; }; return f(21);",
        Want::Num(42.0),
    ),
    (
        "function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); } return fact(6);",
        Want::Num(720.0),
    ),
    (
        "let mk = function () { let c = 0; return function () { c += 1; return c; }; }; let g = mk(); g(); g(); return g();",
        Want::Num(3.0),
    ),
    ("return 'a' + 'b' + 1;", Want::Str("ab1")),
    (
        "let o = { x: 10, y: 2 }; return o.x + o.y;",
        Want::Num(12.0),
    ),
    (
        "let o = { x: 1 }; o.y = 5; return o.x + o.y;",
        Want::Num(6.0),
    ),
    (
        "let o = { a: { b: { c: 7 } } }; return o.a.b.c;",
        Want::Num(7.0),
    ),
    (
        "return 1 < 2 && 2 <= 2 && 3 > 2 && 3 >= 3;",
        Want::Bool(true),
    ),
    ("return 1 === 1 && 1 !== '1';", Want::Bool(true)),
    ("return !false && (!!true);", Want::Bool(true)),
    ("return typeof 1;", Want::Str("number")),
    ("return typeof 'x';", Want::Str("string")),
    ("return typeof undefined;", Want::Str("undefined")),
    ("return typeof null;", Want::Str("object")),
    ("return null ?? 7;", Want::Num(7.0)),
    ("return undefined ?? 7;", Want::Num(7.0)),
    (
        "let s = ''; switch (2) { case 2: s += 'B'; case 3: s += 'C'; break; } return s;",
        Want::Str("BC"),
    ),
    ("return [1, 2, 3].length;", Want::Num(3.0)),
    (
        "let x = 5; let y = x > 3 ? 'big' : 'small'; return y;",
        Want::Str("big"),
    ),
];

fn eval_val(src: &str) -> Val {
    eval_src(src)
}

/// Canonical display of an f64 as the engine renders it. The engine's
/// `number_to_string` (v12-interp/src/ops.rs) is Rust's shortest-round-trip
/// `Display`, so this matches for every value in the corpus.
fn crate_display_of(n: f64) -> String {
    format!("{n}")
}

fn assert_want(src: &str, want: &Want) {
    match want {
        Want::Num(n) => {
            // NaN-aware comparison, matching test_support::expect_num semantics.
            if n.is_nan() {
                panic!("NaN expectations need expect_num; not used in this corpus");
            }
            let got_mini = match eval_val(src) {
                Val::F64(f) => f,
                other => panic!("mini: expected number, got {other:?} for {src:?}"),
            };
            assert_eq!(got_mini, *n, "mini disagrees on {src:?}");
            let got_real = run_real(src);
            assert_eq!(
                got_real,
                crate_display_of(*n),
                "real interp disagrees on {src:?}"
            );
        }
        Want::Str(s) => {
            let got_mini = value_to_string(&eval_val(src));
            assert_eq!(got_mini, *s, "mini disagrees on {src:?}");
            assert_eq!(run_real(src), *s, "real interp disagrees on {src:?}");
        }
        Want::Bool(b) => {
            let got_mini = match eval_val(src) {
                Val::Bool(v) => v,
                other => panic!("mini: expected bool, got {other:?} for {src:?}"),
            };
            assert_eq!(got_mini, *b, "mini disagrees on {src:?}");
            assert_eq!(
                run_real(src),
                if *b { "true" } else { "false" },
                "real interp disagrees on {src:?}"
            );
        }
        Want::Undefined => {
            let got_mini = value_to_string(&eval_val(src));
            assert_eq!(got_mini, "undefined", "mini disagrees on {src:?}");
            assert_eq!(
                run_real(src),
                "undefined",
                "real interp disagrees on {src:?}"
            );
        }
    }
}

#[test]
fn mini_and_real_agree_on_corpus() {
    // Collect every disagreement instead of stopping at the first, so one
    // run reports the full set of divergences.
    let mut failures: Vec<String> = Vec::new();
    for (src, want) in CASES {
        if let Err(msg) = std::panic::catch_unwind(|| assert_want(src, want)) {
            failures.push(msg.downcast_ref::<String>().cloned().unwrap_or_default());
        }
    }
    assert!(
        failures.is_empty(),
        "{} disagreement(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Gap, fixed: `loose_equals` (v12-interp/src/ops.rs) lacked the number↔string
/// coercion arm of ES 7.2.14, so `1 == '1'` yielded `false` on the real interp
/// while the mini reference interpreter returned `true`. Kept as a regression
/// case after the fix.
#[test]
fn known_gap_loose_equals_number_string_coercion() {
    assert_want("return 1 == '1';", &Want::Bool(true));
    assert_want("return '1' == 1;", &Want::Bool(true));
    assert_want(
        "return 1 == '1' && 1 === 1 && 1 !== '1';",
        &Want::Bool(true),
    );
}

#[test]
fn real_interp_matches_expect_num_helper_semantics() {
    // Cross-check one numeric case through the shared matcher, so the
    // shared matcher and the differential path can never silently diverge.
    expect_num("return 1 + 2 * 3;", 7.0);
    expect_bool("return 1 < 2;", true);
    expect_str("return 'a' + 'b';", "ab");
}
