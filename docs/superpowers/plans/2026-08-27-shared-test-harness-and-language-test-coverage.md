# Shared Test Harness & Language-Test Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the main issues in the shared test infrastructure and raise executable language-test coverage (in-repo differential coverage + ~4.9k currently-skipped Test262 `language` tests).

**Architecture:** Extract the mini reference interpreter trapped inside `v12-bccompiler`'s test module into a shared dev-only crate (`test-support`), dedupe per-crate helpers onto it, add the missing differential suite (`Mini` reference vs real `Interp`), guard the hardcoded opcode inventory, and wire the Test262 runner's async/`$262` skips into executable tests via a JS shim + output re-read. Conformance buckets A/B (compiler Tier coverage, constant-pool capacity) are **out of scope** — those are engine features, not test issues.

**Tech Stack:** Rust 2024 edition, Cargo workspace, cargo-nextest, existing crates `v12-bytecode` / `v12-bccompiler` / `v12-interp` / `v12-engine`, `test262-runner` conformance harness.

**Spec:** This plan is its own spec — the Issue Register below is the analyzed baseline (2026-08-27), grounded in `conformance/known-failures.md` (last scored commit `f47ec78`: 24 873 tests, 4 940 pass / 14 972 fail / 4 961 skip, 24.8 %).

## Global Constraints

- Rust edition 2024; workspace members are `crates/*` + `conformance/harness` (a new `crates/test-support` is picked up automatically by the `crates/*` glob — verify it appears in `cargo metadata`).
- All internal crate deps go through `[workspace.dependencies]` (Cargo.toml:53-62) and are referenced with `.workspace = true`.
- Test runner is cargo-nextest: `cargo nextest run --workspace` (README:76-79).
- `Realm::INTRINSIC_NAMES` order is a hard contract with `GLOBAL_VAR_OFFSET` in v12-interp and v12-bccompiler — **do not append to the intrinsic table** (realm.rs:31-47).
- Determinism: any new fuzzing randomness must be seeded (convention set by `crates/v12-bytecode/tests/common/mod.rs:3-5`).
- Dev-dependency cycles (crate X dev-depends on test-support, which normally depends on X) are permitted by Cargo; do not introduce *normal* dependency cycles.
- Conformance scoring convention: `pass%` is over executable tests only (`pass + fail`), skips excluded (known-failures.md:6).
- Every new engine/harness behavior must run both with and without the `jit` feature where relevant (feature-gated JIT, v12-jit-baseline).

---

## Issue Register (what this plan fixes)

Identified 2026-08-27 by auditing the test infrastructure. Each issue maps to a task.

| # | Issue | Evidence | Fix (task) |
|---|---|---|---|
| I1 | **Shared harness trapped inline.** A ~600-line mini reference interpreter (`Val`/`Obj`/`Env`/`Mini`/`run_fn` + 13 coercion helpers) and the `eval_src`/matcher harness live as private items inside `#[cfg(test)] mod tests` in `crates/v12-bccompiler/src/tests.rs` (types :33-73, `Mini` :79-86, helpers :447-624, `eval_src`+matchers :630-672). Single-consumer; `v12-interp` and `v12-engine` tests cannot reuse it. No shared test crate exists anywhere in the workspace. | `crates/v12-bccompiler/src/tests.rs`; Cargo.toml:53-62 has no test crate | Task 1, 2 |
| I2 | **Duplicated helpers.** `crates/v12-interp/src/tests.rs:12-44` re-implements `expect_throw`/`eval_thrown`/`empty_fn` locally; `empty_fn` overlaps `fn_with` in `crates/v12-bytecode/tests/common/mod.rs:63-80`. | interp tests.rs:12-44 | Task 3 |
| I3 | **Stale doc pointer + missing differential coverage.** `crates/v12-interp/src/tests.rs:3-4` claims "The differential suite in `tests/differential.rs` covers compiled Tier-1 programs end to end" — `crates/v12-interp/tests/` does not exist. The compiled-program execution path is under-tested in-tree (covered only indirectly via Test262). | interp tests.rs:3-4; workspace scan | Task 4 |
| I4 | **Opcode inventory drift risk.** `KNOWN_DISCRIMINANTS` (57 hardcoded discriminants, `crates/v12-bytecode/tests/common/mod.rs:16-28`) is maintained by hand. Nothing verifies the list equals the set of discriminants `Opcode` actually assigns; a renumbering that happens to keep the same *count* passes the sweep silently. | common/mod.rs:13-28 | Task 5 |
| I5 | **~4.9k language tests skipped by harness gaps** (24.8 % of the suite is never executed): `skip_reason_for` (conformance/harness/src/runner.rs:317-344) hard-skips `flags: [async]` (4 883 tests) and any source mentioning `$262` (78 tests). The engine already has the required primitives (job queue, realm-global injection); only harness plumbing is missing. | runner.rs:317-344; known-failures.md bucket C | Task 6 |
| I6 | **No CI gate.** No `.github/` directory; the "484 tests" unit gate and the conformance gate are manual runs, so coverage regressions land unnoticed. | workspace scan | Task 7 |

**Excluded (recorded, not planned here):** bucket A `unsupported expression` (12 625 fails — compiler Tier coverage, engine feature work), bucket B `too many functions/constants` (1 303 fails — capacity fix). Both are engine work with their own queue in `conformance/known-failures.md`.

---

### Task 1: Create the `test-support` crate and move the mini interpreter into it

**Files:**
- Create: `crates/test-support/Cargo.toml`
- Create: `crates/test-support/src/lib.rs`
- Create: `crates/test-support/src/mini.rs`
- Source of moved code (do not modify yet): `crates/v12-bccompiler/src/tests.rs:29-672`

**Interfaces:**
- Consumes: `v12_bccompiler::compile_source_with_strings(src) -> Result<(Program, Vec<String>), CompileError>` (bccompiler lib.rs:357), `v12_bytecode::{Opcode, Instr}`.
- Produces (later tasks rely on these exact names, all `pub`): `Val` (+variants), `Obj`, `ClosureVal`, `Env`, `Env::root()`, `Mini`, `Mini::run_fn(idx, this, args, env) -> Result<Val, Val>`, `eval_src(src) -> Val`, `expect_num(src, f64)`, `expect_bool(src, bool)`, `expect_str(src, &str)`, `expect_undefined(src)`, `value_to_string(&Val) -> String`.

- [x] **Step 1: Create the crate manifest**

```toml
# crates/test-support/Cargo.toml
[package]
name = "test-support"
version = "0.1.0"
edition = "2024"
publish = false
description = "Shared dev-only test harness: mini reference interpreter + eval matchers"

[dependencies]
v12-bytecode = { workspace = true }
v12-bccompiler = { workspace = true }
```

- [x] **Step 2: Register in workspace deps**

Add one line to `[workspace.dependencies]` in the root `Cargo.toml` (next to the other crate entries, Cargo.toml:53-62):

```toml
test-support = { path = "crates/test-support" }
```

- [x] **Step 3: Create the crate root**

```rust
// crates/test-support/src/lib.rs
//! Shared dev-only test harness for the v12 workspace.
//!
//! Not shipped: dev-dependency of `v12-bccompiler`, `v12-interp`, and
//! `v12-bytecode` test binaries. Depends on the real compiler so callers
//! get `eval_src`-style source-in, value-out helpers.

pub mod mini;
pub use mini::*;
```

- [x] **Step 4: Move the mini interpreter verbatim**

Move `crates/v12-bccompiler/src/tests.rs` lines 29-672 (the `Val`/`Obj`/`ClosureVal`/`Env` types at :33-73, `Mini` + `BUDGET` at :79-86, `run_fn` at :96, the free helpers `walk_env`/`truthy`/`to_string`/`to_number`/`to_int32`/`type_of`/`strict_eq`/`loose_eq`/`compare`/`binop`/`to_key`/`get_prop`/`set_prop`/`delete_prop` at :447-624, and `eval_src`/`expect_num`/`expect_bool`/`expect_str`/`expect_undefined` at :630-672) into `crates/test-support/src/mini.rs`.

Mechanical adjustments while moving (no logic changes):
1. Make every moved item `pub` (types, fields, `run_fn`, all helpers, `BUDGET` may stay private).
2. Replace `super::*`-style crate-internal imports at the top with:
   ```rust
   use std::cell::RefCell;
   use std::collections::HashMap;
   use std::rc::Rc;

   use v12_bccompiler::Program;
   use v12_bytecode::Opcode;
   ```
   (`compile_source_with_strings` inside `eval_src` becomes `v12_bccompiler::compile_source_with_strings`.)
3. Add one new thin wrapper at the end of `mini.rs` (needed by Task 4 to compare `Mini` values against the real engine's display strings):
   ```rust
   /// Renders a mini-interpreter value the way tests compare it.
   pub fn value_to_string(v: &Val) -> String {
       to_string(v)
   }
   ```
4. Do **not** move the ~50 bytecode-inspection tests (e.g. tests.rs:2603 asserting on `fb.instrs`/`validate()`) — they stay in `bccompiler` and use `crate::model::{spur_of_str_id, str_id_of}`, which is crate-private and out of reach for `test-support`.

- [x] **Step 5: Verify the crate builds standalone**

Run: `cargo build -p test-support`
Expected: success, zero errors.

- [x] **Step 6: Sanity-check the moved harness with a self-test**

Append to `crates/test-support/src/mini.rs`:

```rust
#[cfg(test)]
mod self_tests {
    use super::*;

    #[test]
    fn eval_src_returns_completion_value() {
        expect_num("return 1 + 2;", 3.0);
        expect_str("return 'a' + 'b';", "ab");
        expect_bool("return 1 < 2;", true);
        expect_undefined("return void 0;");
    }

    #[test]
    fn value_to_string_matches_expect_str() {
        assert_eq!(value_to_string(&eval_src("return 'x' + 1;")), "x1");
    }
}
```

Run: `cargo nextest run -p test-support`
Expected: PASS (2 tests).

- [x] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/test-support
git commit -m "test: extract mini reference interpreter into shared test-support crate"
```

---

### Task 2: Migrate `v12-bccompiler` tests onto `test-support`

**Files:**
- Modify: `crates/v12-bccompiler/Cargo.toml` (add dev-dependency)
- Modify: `crates/v12-bccompiler/src/tests.rs` (delete moved code, add import)
- Create: `crates/v12-bccompiler/tests/mini_harness_parity.rs` (regression guard)

**Interfaces:**
- Consumes: everything `test-support` exports (Task 1).
- Produces: `v12-bccompiler` tests compile with the inline copy deleted; no public API change.

- [x] **Step 1: Add the dev-dependency**

In `crates/v12-bccompiler/Cargo.toml`, append (create `[dev-dependencies]` if absent):

```toml
[dev-dependencies]
test-support = { workspace = true }
```

- [x] **Step 2: Delete the inline copy**

In `crates/v12-bccompiler/src/tests.rs`:
1. Delete lines 29-672 (the moved items — types, `Mini`, helpers, `eval_src`, matchers).
2. Add at the top of `mod tests`:
   ```rust
   use test_support::*;
   ```
3. Fix any remaining unqualified `compile_source_with_strings` references *inside remaining tests* — those stay valid because they call the crate's own public fn; leave them.

- [x] **Step 3: Run the full bccompiler suite**

Run: `cargo nextest run -p v12-bccompiler`
Expected: all 125 tests PASS (the ~186 `expect_*`/`eval_src` call sites now resolve to `test-support`; the ~50 bytecode-inspection tests are untouched).

- [x] **Step 4: Add a parity guard test**

This pins the contract that the shared copy behaves identically for a few representative programs from the original suite (patterns from tests.rs:682, :2666, :2683):

```rust
// crates/v12-bccompiler/tests/mini_harness_parity.rs
//! Guards the extraction of the mini interpreter: if `test-support` and the
//! compiler drift apart on basic semantics, these fail before 125 dependent
//! tests give misleading results.

use test_support::{expect_bool, expect_num, expect_str};

#[test]
fn switch_fallthrough_with_break() {
    expect_str(
        "
        let s = '';
        switch (2) { case 2: s += 'B'; case 3: s += 'C'; break; }
        return s;
    ",
        "BC",
    );
}

#[test]
fn typeof_null_is_object() {
    expect_str("return typeof null;", "object");
}

#[test]
fn nullish_coalescing_short_circuit() {
    expect_num("return null ?? 7;", 7.0);
    expect_bool("return 0 || false;", false);
}
```

- [x] **Step 5: Run and commit**

Run: `cargo nextest run -p v12-bccompiler`
Expected: PASS (125 + 3).

```bash
git add crates/v12-bccompiler
git commit -m "test: consume shared test-support harness in v12-bccompiler"
```

---

### Task 3: Dedupe `v12-interp` test helpers and fix the stale doc pointer

**Files:**
- Modify: `crates/v12-interp/Cargo.toml` (add dev-dependency)
- Modify: `crates/v12-interp/src/tests.rs:1-44`

**Interfaces:**
- Consumes: `test-support` (Task 1), `v12_bytecode::{Const, ConstantPool, FunctionBytecode}`.
- Produces: interp tests import shared helpers; doc comment matches reality.

- [x] **Step 1: Add the dev-dependency**

```toml
[dev-dependencies]
test-support = { workspace = true }
```

- [x] **Step 2: Replace local helpers with shared ones**

In `crates/v12-interp/src/tests.rs`:

1. Delete `expect_throw` (:12-17), `eval_thrown` (:20-23), and `empty_fn` (:25-40).
2. Add to `test-support` a `src/interp_util.rs` module (test-support gains `v12-interp` as a **normal** dependency — the resulting dev-cycle with v12-interp is permitted by Cargo):

   ```rust
   // crates/test-support/src/interp_util.rs
   //! Helpers for tests that drive the real interpreter.

   use v12_interp::{Interp, JSException};

   /// Runs `interp`, expecting an uncaught throw; returns the thrown value.
   pub fn expect_throw(interp: &mut Interp) -> v12_heap_types::JsValue {
       match interp.run() {
           Err(JSException(v)) => v,
           Ok(()) => panic!("expected an uncaught exception"),
       }
   }

   /// Compiles + runs `src`, returning the thrown value (completion-value trick).
   pub fn eval_thrown(src: &str) -> v12_heap_types::JsValue {
       let mut interp = Interp::from_source(src).expect("compile");
       expect_throw(&mut interp)
   }
   ```

   Note: `v12_heap` is the actual crate name providing `JsValue`/`Heap` (used at interp tests.rs:7) — replace `v12_heap_types` above with `v12_heap` and add `v12-heap = { workspace = true }` to `test-support`'s `[dependencies]`. Export from `test-support/src/lib.rs` with `pub mod interp_util; pub use interp_util::*;`.
3. Keep `program_of` (:42-44) local — it is 3 lines and interp-specific.
4. Replace `empty_fn(...)` call sites with a shared builder. Add to `test-support/src/mini.rs` (it already depends on `v12-bytecode`):
   ```rust
   /// A `FunctionBytecode` wrapping exactly `instrs` with an empty span table.
   pub fn fn_with_instrs(max_regs: u16, instrs: Vec<v12_bytecode::Instr>, consts: v12_bytecode::ConstantPool) -> v12_bytecode::FunctionBytecode {
       let spans = vec![(0, 0); instrs.len()];
       v12_bytecode::FunctionBytecode {
           name_hint: None,
           max_regs,
           instrs,
           consts,
           handlers: Vec::new(),
           spans,
           pc_map: Vec::new(),
           is_strict: false,
           fixed_params: 0,
           has_rest: false,
           rest_reg: 0,
       }
   }
   ```
   In interp tests.rs, `s/empty_fn(/fn_with_instrs(/` and add `use test_support::*;` alongside the existing imports.

- [x] **Step 3: Fix the stale doc pointer**

Replace interp tests.rs lines 1-4 with:

```rust
//! Focused unit tests over hand-built bytecode: the wide-operand encodings,
//! handler delivery depth, the call-depth guard, the native seam, and
//! fall-off-the-end completion. The differential suite in
//! `tests/differential.rs` covers compiled Tier-1 programs end to end
//! (added in Task 4 of the test-coverage plan; it will not exist until that
//! task lands — until then this pointer is forward-looking).
```

- [x] **Step 4: Run and commit**

Run: `cargo nextest run -p v12-interp && cargo nextest run -p test-support`
Expected: PASS.

```bash
git add crates/test-support crates/v12-interp
git commit -m "test: dedupe interp helpers via test-support, correct differential-suite doc pointer"
```

---

### Task 4: Add the missing differential suite (`Mini` vs real `Interp`)

**Files:**
- Create: `crates/v12-interp/tests/differential.rs`

**Interfaces:**
- Consumes: `test_support::{eval_src, expect_num, expect_bool, expect_str, value_to_string, Val}`, `v12_interp::{Interp, JSException}` (`Interp::from_source` compiles via `compile_source_with_strings` — interp lib.rs:444-447), `Interp::to_display_string(&mut self, v: JsValue) -> String` (interp lib.rs:615).
- Produces: `run_real(src) -> String` (display string of the completion value thrown by the wrapped IIFE) — the reusable seam for growing the corpus later.

- [x] **Step 1: Write the suite with a seed corpus of ~25 cases**

The ground truth is *declared per case* (not compared between the two implementations) so number-formatting differences between `Mini`'s `to_string` and the engine's `to_display_string` cannot cause false failures. Each case runs through **both** implementations.

```rust
// crates/v12-interp/tests/differential.rs
//! Differential suite: every corpus case is evaluated by (a) the mini
//! reference interpreter (test-support) and (b) the real Tier-1 interpreter
//! over compiler output. Both must agree with the declared ground truth.
//! Covers the compiled-program end-to-end path that unit tests over
//! hand-built bytecode do not reach.

use test_support::{expect_bool, expect_num, expect_str, value_to_string, Val};
use v12_interp::{Interp, JSException};

/// Runs `src` on the real engine and returns the completion value's display
/// string. `src` is wrapped the same way `test_support::eval_src` wraps it.
fn run_real(src: &str) -> String {
    let wrapped = format!("throw (function () {{\n{src}\n}})();");
    let mut interp = Interp::from_source(&wrapped).expect("compile");
    match interp.run() {
        Ok(()) => panic!("expected the completion-value throw"),
        Err(JSException(v)) => interp.to_display_string(v),
    }
}

enum Want {
    Num(f64),
    Str(&'static str),
    Bool(bool),
    Undefined,
}

const CASES: &[(&str, Want)] = &[
    ("return 1 + 2 * 3;", Want::Num(7.0)),
    ("return (1 + 2) * 3;", Want::Num(9.0)),
    ("return 10 % 3;", Want::Num(1.0)),
    ("return 2 ** 10;", Want::Num(1024.0)),
    ("let a = 1; a += 41; return a;", Want::Num(42.0)),
    ("let i = 0; let s = 0; while (i < 5) { s += i; i += 1; } return s;", Want::Num(10.0)),
    ("let s = 0; for (let i = 1; i <= 4; i += 1) { s += i; } return s;", Want::Num(10.0)),
    ("let f = function (x) { return x * 2; }; return f(21);", Want::Num(42.0)),
    ("function fact(n) { return n <= 1 ? 1 : n * fact(n - 1); } return fact(6);", Want::Num(720.0)),
    ("let mk = function () { let c = 0; return function () { c += 1; return c; }; }; let g = mk(); g(); g(); return g();", Want::Num(3.0)),
    ("return 'a' + 'b' + 1;", Want::Str("ab1")),
    ("let o = { x: 10, y: 2 }; return o.x + o.y;", Want::Num(12.0)),
    ("let o = { x: 1 }; o.y = 5; return o.x + o.y;", Want::Num(6.0)),
    ("let o = { a: { b: { c: 7 } } }; return o.a.b.c;", Want::Num(7.0)),
    ("return 1 < 2 && 2 <= 2 && 3 > 2 && 3 >= 3;", Want::Bool(true)),
    ("return 1 == '1' && 1 === 1 && 1 !== '1';", Want::Bool(true)),
    ("return !false && (!!true);", Want::Bool(true)),
    ("return typeof 1;", Want::Str("number")),
    ("return typeof 'x';", Want::Str("string")),
    ("return typeof undefined;", Want::Str("undefined")),
    ("return typeof null;", Want::Str("object")),
    ("return null ?? 7;", Want::Num(7.0)),
    ("return undefined ?? 7;", Want::Num(7.0)),
    ("let s = ''; switch (2) { case 2: s += 'B'; case 3: s += 'C'; break; } return s;", Want::Str("BC")),
    ("return [1, 2, 3].length;", Want::Num(3.0)),
    ("let x = 5; let y = x > 3 ? 'big' : 'small'; return y;", Want::Str("big")),
];

fn assert_want(src: &str, want: &Want) {
    match want {
        Want::Num(n) => {
            // NaN-aware comparison, matching test_support::expect_num semantics.
            if n.is_nan() {
                panic!("NaN expectations need expect_num; not used in this corpus");
            }
            let got_mini = match eval_val(src) { Val::F64(f) => f, other => panic!("mini: expected number, got {other:?} for {src:?}") };
            assert_eq!(got_mini, *n, "mini disagrees on {src:?}");
            let got_real = run_real(src);
            assert_eq!(got_real, crate_display_of(*n), "real interp disagrees on {src:?}");
        }
        Want::Str(s) => {
            let got_mini = value_to_string(&eval_val(src));
            assert_eq!(got_mini, *s, "mini disagrees on {src:?}");
            assert_eq!(run_real(src), *s, "real interp disagrees on {src:?}");
        }
        Want::Bool(b) => {
            let got_mini = match eval_val(src) { Val::Bool(v) => v, other => panic!("mini: expected bool, got {other:?} for {src:?}") };
            assert_eq!(got_mini, *b, "mini disagrees on {src:?}");
            assert_eq!(run_real(src), if *b { "true" } else { "false" }, "real interp disagrees on {src:?}");
        }
        Want::Undefined => {
            let got_mini = value_to_string(&eval_val(src));
            assert_eq!(got_mini, "undefined", "mini disagrees on {src:?}");
            assert_eq!(run_real(src), "undefined", "real interp disagrees on {src:?}");
        }
    }
}
```

Because `eval_src` is private to `test_support`'s call sites only as a function returning `Val` (it **is** `pub` after Task 1), add the tiny shims this file needs at the top:

```rust
fn eval_val(src: &str) -> Val {
    test_support::eval_src(src)
}

/// Canonical display of an f64 as the engine renders it. If the engine's
/// `to_display_string` formats integers differently, adjust here once and
/// note the divergence — do NOT weaken the corpus.
fn crate_display_of(n: f64) -> String {
    format!("{n}")
}
```

Then the test entry points:

```rust
#[test]
fn mini_and_real_agree_on_corpus() {
    for (src, want) in CASES {
        assert_want(src, want);
    }
}

#[test]
fn real_interp_matches_expect_num_helper_semantics() {
    // Cross-check one numeric case through the shared matcher, so the
    // shared matcher and the differential path can never silently diverge.
    expect_num("return 1 + 2 * 3;", 7.0);
    expect_bool("return 1 < 2;", true);
    expect_str("return 'a' + 'b';", "ab");
}
```

- [x] **Step 2: Run the suite to verify it fails (or passes) informatively**

Run: `cargo nextest run -p v12-interp --test differential`
Expected: either all PASS (real engine agrees with ground truth — good) or specific cases FAIL with a message naming the disagreeing implementation (`mini disagrees on ...` / `real interp disagrees on ...`). A failure here is a **finding, not a plan failure**: record the case in `conformance/known-failures.md` as a new bullet under a new bucket "in-repo differential" with the exact filter `cargo nextest run -p v12-interp --test differential`, then fix the engine or (only if the declared ground truth was wrong) correct the corpus entry — never delete the case.

- [x] **Step 3: Commit**

```bash
git add crates/v12-interp/tests/differential.rs
git commit -m "test: add differential suite (mini reference vs real Tier-1 interpreter)"
```

---

### Task 5: Guard the hardcoded opcode inventory against enum drift

**Files:**
- Modify: `crates/v12-bytecode/tests/decode_sweep.rs` (append one test)

**Interfaces:**
- Consumes: `KNOWN_DISCRIMINANTS` from `common/mod.rs:16-28` (already `pub`, already imported by the sweep binaries), `v12_bytecode::Instr` tuple constructor and `Instr::op()` (public — used at common/mod.rs:163 and in bccompiler tests via `i.op()`).
- Produces: a bidirectional drift guard; no API change.

- [x] **Step 1: Write the failing-nothing guard test**

Append to `crates/v12-bytecode/tests/decode_sweep.rs`:

```rust
/// Bidirectional drift guard for the hardcoded inventory: every discriminant
/// the enum actually assigns must be listed, and every listed discriminant
/// must actually be assigned. Catches renumberings that keep the count
/// constant — which the exhaustive sweep alone would miss.
#[test]
fn known_discriminants_exactly_match_opcode_enum() {
    for d in 0u8..=255 {
        let assigned = Instr((u32::from(d) << 24)).op().is_some();
        let listed = KNOWN_DISCRIMINANTS.contains(&d);
        assert_eq!(
            assigned, listed,
            "discriminant {d}: enum says assigned={assigned}, KNOWN_DISCRIMINANTS says listed={listed}"
        );
    }
    assert_eq!(
        KNOWN_DISCRIMINANTS.len(),
        EXPECTED_OPCODE_COUNT,
        "list length drifted from EXPECTED_OPCODE_COUNT"
    );
}
```

If `Instr::op()` turns out to be non-public at this call site, use the public decode path already exercised by `decode_sweep.rs` (whatever that binary uses to decode one word — check its imports) instead of widening `v12-bytecode`'s API for a test.

- [x] **Step 2: Run it**

Run: `cargo nextest run -p v12-bytecode --test decode_sweep known_discriminants`
Expected: PASS on the current tree (list matches enum). To prove the guard works: temporarily change one entry in `KNOWN_DISCRIMINANTS` (e.g. swap `20` for `20`→`21` duplicate), re-run, observe FAIL, revert.

- [x] **Step 3: Commit**

```bash
git add crates/v12-bytecode/tests/decode_sweep.rs
git commit -m "test: bidirectional drift guard for hardcoded opcode discriminants"
```

---

### Task 6: Wire Test262 async tests and the `$262` host shim into the runner

**Files:**
- Modify: `conformance/harness/src/runner.rs` (`skip_reason_for` :317-344; `run_single_test` preamble assembly :224-241 and eval/verdict block :261-313)
- Modify: `conformance/known-failures.md` (bucket C bookkeeping) and `conformance/fix-log.md` (before/after entry)

**Interfaces:**
- Consumes: `Frontmatter::has_flag("async")` (frontmatter.rs:45-47), `Engine::eval` / `Engine::run_jobs` / `Engine::to_display_string` (engine.rs:79/311/…), `Status::{Pass, Fail, Skip}` (runner.rs:27-35), `handle_positive_or_negative_ok` (runner.rs:347-405), `handle_thrown` (runner.rs:408-526).
- Produces: async-flagged and `$262`-mentioning tests become executable (skips → pass/fail); new skip only for tests needing real multi-realm (`$262.createRealm`) or the `agent` API.

Design decisions (fixed by this plan):
- **JS shim, not Rust natives.** `print` and `$262` are defined by a preamble prepended to `combined`; captured output is stored in a global array and re-read with a **second `engine.eval` on the same engine** (same realm/global). This needs zero engine changes and no new native indices.
- **No stdout capture.** The existing `println!`-based `console.log` (builtins/mod.rs:238) is left alone; async classification reads the captured array, not process stdout — this stays correct under the rayon-parallel runner (main.rs:121-125).
- **Honest skips stay.** Tests whose source contains `createRealm(` or `agent.` keep skipping (multi-realm/agent support is a separate, larger effort).

- [x] **Step 1: Add the shim constant and skip-logic narrowing**

In `conformance/harness/src/runner.rs`, add near the top (beside `MINIMAL_HARNESS_POLYFILL`'s sibling constants):

```rust
/// JS preamble defining the `print` sink and the `$262` host object that
/// Test262 harness files expect. Output is captured in a global array that
/// the runner re-reads after `run_jobs`; nothing touches process stdout.
const TEST262_HOST_SHIM: &str = r#"
globalThis.__test262Prints = [];
function __consolePrintHandle__(s) { globalThis.__test262Prints.push(String(s)); }
function print(s) { globalThis.__test262Prints.push(String(s)); }
var $262 = {
    createRealm: function () { throw new Error('$262.createRealm: not implemented'); },
    detachArrayBuffer: function (b) { return b; },
    getReport: function () { return null; },
    destroy: function () {},
    gc: function () {},
    global: globalThis,
};
"#;
```

Narrow `skip_reason_for` (:317-344) to:

```rust
fn skip_reason_for(fm: &Frontmatter, source: &str) -> Option<String> {
    // Multi-realm and agent API remain unsupported: tests that actually call
    // them would fail, so keep an honest skip instead of a guaranteed red.
    if source.contains("createRealm(") {
        return Some("requires $262.createRealm (multi-realm)".to_string());
    }
    if source.contains("agent.") || source.contains("$262.agent") {
        return Some("requires $262.agent (worker/Atomics harness)".to_string());
    }
    // async-flagged tests and doneprintHandle-based tests now run via the
    // async verdict path in run_single_test; do NOT skip them here.
    let _ = fm; // async flag is handled at the verdict, not as a skip
    None
}
```

- [x] **Step 2: Prepend the shim and add the async verdict**

In `run_single_test`:
1. At the `combined` assembly (~:224-241), prepend the shim *after* the strict directive and *before* harness includes:
   ```rust
   let combined = format!("{strict_directive}{TEST262_HOST_SHIM}{harness_source}{test_body}");
   ```
2. After `let _ = engine.run_jobs();` (~:268), replace the unconditional `Ok(_) → Pass` branch (:291-294) with an async-aware verdict. Compute `let is_async = fm.has_flag("async") || (source.contains("$DONE(") && source.contains("doneprint"));` before the eval block. Then:
   ```rust
   if is_async {
       let printed = engine
           .eval("globalThis.__test262Prints.join('\\n')")
           .map(|v| engine.to_display_string(v))
           .unwrap_or_default();
       return if printed.contains("Test262:AsyncTestComplete") {
           TestOutcome { status: Status::Pass, ..same_shape_as_existing_outcome }
       } else if let Some(rest) = printed.strip_prefix("Test262:AsyncTestFailure:") {
           TestOutcome { status: Status::Fail, detail: format!("async failure: {rest}"), .. }
       } else {
           TestOutcome { status: Status::Fail, detail: "async test never completed".into(), .. }
       };
   }
   ```
   Keep the existing `negative:` handling for the non-async path unchanged. Match the actual `TestOutcome` struct shape in runner.rs (it is defined near :27-35 alongside `Status`) rather than the sketch above — the sketch fixes *semantics*, the struct fields come from the file.
3. If the async test also threw during eval, prefer the thrown-error verdict (existing `handle_thrown` path) — an eval-time throw means the test body failed before `$DONE` could ever run.

- [x] **Step 3: Prove the mechanics with a harness self-test**

Add to the runner's existing self-test module (11 tests already live there — follow their setup pattern):

```rust
#[test]
fn async_doneprint_test_completes_via_captured_print() {
    // Arrange: a tiny async-shaped source; the real doneprintHandle.js
    // semantics are `$DONE()` → prints Test262:AsyncTestComplete.
    let src = "globalThis.__test262Prints = [];\n\
               function print(s) { globalThis.__test262Prints.push(String(s)); }\n\
               Promise.resolve().then(function () { print('Test262:AsyncTestComplete'); });";
    let mut engine = v12_engine::Engine::new();
    engine.eval(src).expect("eval");
    engine.run_jobs();
    let printed = engine
        .eval("globalThis.__test262Prints.join('\\n')")
        .map(|v| engine.to_display_string(v))
        .unwrap_or_default();
    assert!(printed.contains("Test262:AsyncTestComplete"), "printed: {printed:?}");
}
```

Run: `cargo nextest run -p test262-runner`
Expected: PASS. If this fails, Promise reactions are not scheduling into `JobQueue` — stop, file it in `known-failures.md` as a new bucket, and do not narrow the skip until the job queue actually resolves thenables (otherwise ~4.9k tests flip from skip to guaranteed-fail).

- [x] **Step 4: Measure before/after and update the books**

```sh
# Before-numbers are already recorded: 4 940 pass / 14 972 fail / 4 961 skip
cargo run -p test262-runner -- --filter language --jobs 8 --json-out /tmp/t262-after.json
```

Update `conformance/known-failures.md`:
- Move bucket C to the Done section with the actual before/after numbers.
- Re-score the header totals (new executable denominator — pass% may shift either way; that is expected and must be recorded, not hidden).
Add the corresponding entry to `conformance/fix-log.md` (command used, commit, deltas).

- [x] **Step 5: Commit**

```bash
git add conformance/harness/src/runner.rs conformance/known-failures.md conformance/fix-log.md
git commit -m "conformance: wire async harness + \$262 host shim; ~4.9k language tests become executable"
```

---

### Task 7: Add a CI gate so coverage cannot silently regress

**Files:**
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `cargo nextest run --workspace` (README:76-79), `./conformance/run.sh` (self-clones test262 per run.sh:18-45).
- Produces: two jobs — unit gate (all tasks above) and conformance gate (Task 6's numbers).

- [x] **Step 1: Write the workflow**

```yaml
# .github/workflows/ci.yml
name: ci
on:
  push:
    branches: [main]
  pull_request:

jobs:
  tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: taiki-e/install-action@v2
        with:
          tool: cargo-nextest
      - run: cargo nextest run --workspace
      # JIT is feature-gated; run it explicitly so gated tests are not silently skipped.
      - run: cargo nextest run --workspace --features jit

  conformance:
    runs-on: ubuntu-latest
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: ./conformance/run.sh --filter language --jobs 4 --format tap --tap-out /tmp/t262.tap
      - name: Report pass rate
        if: always()
        run: |
          tail -n 40 /tmp/t262.tap || true
```

Note: verify the `jit` feature name/shape at `crates/v12-jit-baseline/Cargo.toml:19` (`[features] jit = [...]`) — if the feature must be enabled workspace-wide rather than per-crate, use `cargo nextest run --workspace --all-features` instead.

- [x] **Step 2: Validate locally, then commit**

```sh
act -j tests 2>/dev/null || echo "act not installed; validate YAML syntax only"
```

At minimum: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"` must succeed.

```bash
git add .github/workflows/ci.yml
git commit -m "ci: unit gate (nextest, incl. jit feature) + language conformance gate"
```

---

## Verification (whole plan)

1. `cargo nextest run --workspace` — all green.
2. `cargo nextest run --workspace --features jit` (or `--all-features` per Task 7 note) — all green.
3. `cargo nextest run -p test-support -p test262-runner` — shared harness self-tests + new runner self-test green.
4. `cargo run -p test262-runner -- --filter language --jobs 8` — skips drop from 4 961 to ≤ ~200 (only createRealm/agent skips remain); totals updated in `known-failures.md`.
5. `grep -rn "differential.rs" crates/v12-interp/src/tests.rs` — pointer now matches an existing file.
6. No duplicate definitions remain: `grep -rn "fn empty_fn\|fn expect_throw\|fn eval_thrown" crates/ --include="*.rs"` returns only `test-support` definitions.

## Self-Review Notes

- Spec coverage: every register issue I1-I6 maps to exactly one task; excluded buckets A/B are named with the reason and their existing queue (`known-failures.md`).
- Type consistency: `run_real` (Task 4) and `expect_throw`/`eval_thrown` (Task 3, moved into `test-support::interp_util`) use the same `Interp::from_source` + `JSException` seam; `fn_with_instrs` (Task 3) mirrors `empty_fn` field-for-field against interp tests.rs:25-40 and common/mod.rs:67-79.
- Known uncertainty, flagged where it lands: exact `TestOutcome` field names (Task 6 Step 2) and `Instr::op()` visibility (Task 5 Step 1) are sketched semantically with the authoritative file/line given — executors reconcile against those lines, they do not invent new behavior.
- Untraced, stated honestly: whether Promise reaction jobs currently resolve through `JobQueue::drain` — Task 6 Step 3 is the explicit gate that decides this before any skip is narrowed.
