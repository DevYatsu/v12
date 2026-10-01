//! The two halves of the environment model agreeing end to end, on the real
//! interpreter over real compiler output. `docs/architecture-maintainability-plan.md`
//! §4 Track 1, tasks W1.3 and W1.4.
//!
//! The compiler resolves a captured binding to a *static* hop count
//! (`Plans::env_depth`, a walk over the unit graph) and the interpreter resolves
//! it to a *runtime* one (`Interp::env_at`: the innermost `ENV_DISPLAY_CAP = 8`
//! environments out of a per-frame cache, the parent-link walk beyond). Nothing
//! forces those two numbers to mean the same thing, and a disagreement is a
//! **silent wrong value**: a misdirected `GetEnvSlot` at depth 0 does not
//! raise, it reads whatever else happens to live in that environment.
//!
//! The documented instance is `class T extends Object`, which emitted a
//! `NewEnvironment` with no matching pop, so the runtime chain stayed one level
//! deeper than the static model for the rest of the unit and
//!
//! ```text
//! function h(){ let acc=0; function bump(){acc=acc+1;}
//!               class T extends Object{m(){}} bump(); return acc; }
//! ```
//!
//! returned `function` (the parent constructor, the class scope's one slot)
//! where the spec says `1`. `v12-engine`'s `class_env_scope.rs` covers that
//! from the embedder's side; the compiler-side halves live in
//! `v12-bccompiler/src/tests.rs` (`env_bias_increments_on_a_scoped_env_and_returns_on_the_matching_pop`,
//! `class_scope_biases_depths_while_live_and_restores_them_after_the_pop`). What
//! is added here is the third thing neither has: an access whose static depth
//! is **past `ENV_DISPLAY_CAP`**, so the compiler's unbounded count and the
//! interpreter's fixed cache are forced to meet.
//!
//! `v12-engine`'s `loop_env_scope.rs` covers the per-iteration environment
//! (`for (let i…)`) from the embedder's side; it is not repeated here.

use v12_heap::{GcPolicy, Heap};
use v12_interp::{Interp, JSException};

/// Evaluates `src` on the real interpreter and returns the completion value's
/// display string. The body is wrapped in an IIFE and the result thrown, so the
/// completion value is observable — the same shape `differential.rs` uses.
fn run_real(src: &str) -> String {
    let wrapped = format!("throw (function () {{\n{src}\n}})();");
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, &wrapped).expect("compile");
    match interp.run() {
        Ok(()) => panic!("expected the completion-value throw"),
        Err(JSException(v)) => interp.to_display_string(v),
    }
}

/// **The historical bug, pinned end to end.** Before `PopEnvironment` existed,
/// the class scope shadowed the enclosing function's environment for the rest
/// of the unit: the `GetEnvSlot` at depth 0 after the class statement read the
/// parent constructor out of the class scope's single slot instead of `acc`.
/// The value is `1`. The *silence* was the defect.
#[test]
fn an_extending_class_does_not_shift_later_env_accesses() {
    assert_eq!(
        run_real(
            "function h(){ let acc=0; function bump(){acc=acc+1;} class Base{} \
             class T extends Base{m(){}} bump(); return acc; } return h();"
        ),
        "1"
    );
}

/// The same, but the reading access is *inside a closure created after* the
/// class. This is the variant that rules out a compile-time depth bias as a
/// sufficient fix: the closure's captured root genuinely **is** the class
/// scope, so no static correction to the enclosing unit's depths can reach it.
#[test]
fn a_closure_created_after_an_extending_class_captures_the_right_environment() {
    assert_eq!(
        run_real(
            "function f(){ let iterator = { tag: 'real' }; class Base{} \
             class T extends Base{m(){}} \
             const get = function(){ return iterator.tag; }; return get(); } return f();"
        ),
        "real"
    );
}

/// Negative control: with no class at all there is no scope to leak, so the
/// answer is identical. A fix that over-corrected — say, by biasing every unit's
/// depths — would break this.
#[test]
fn the_same_shape_without_a_class_is_unaffected() {
    assert_eq!(
        run_real(
            "function h(){ let acc=0; function bump(){acc=acc+1;} bump(); return acc; } return h();"
        ),
        "1"
    );
}

/// A class with no heritage allocates no class-scope environment, so the
/// enclosing unit's depths must be untouched by it too.
#[test]
fn a_class_without_extends_does_not_shift_later_env_accesses() {
    assert_eq!(
        run_real(
            "function h(){ let acc=0; function bump(){acc=acc+1;} \
             class T{m(){}} bump(); return acc; } return h();"
        ),
        "1"
    );
}

/// `levels` lexically nested functions. Level `i` (for `i < levels - 1`)
/// declares `let a{i} = i + 1`, declares the next level inside itself, and
/// returns it; the innermost declares nothing and returns the sum of every
/// outer binding.
///
/// Two properties make this the right shape. Nesting is *lexical* — each level
/// is a statement-list item of its parent's body, not a sibling — so the unit
/// graph is a chain and `a0`'s static depth from the innermost unit is
/// `levels - 2`. And the innermost declares no local of its own, so it owns no
/// environment: the chain is `levels - 1` env-bearing units with a bare unit on
/// top, which is the shape `env_depth` has to get right.
fn nested_capture_source(levels: usize) -> (String, i32) {
    assert!(levels >= 2, "need at least one level to capture from");
    let reads: Vec<String> = (0..levels - 1).map(|i| format!("a{i} ")).collect();
    let mut src = format!(
        "function f{}() {{ return {}; }}",
        levels - 1,
        reads.join("+ ")
    );
    for i in (0..levels - 1).rev() {
        src = format!(
            "function f{i}() {{ let a{i} = {}; {src} return f{}(); }}",
            i + 1,
            i + 1
        );
    }
    let sum: i32 = (1..=levels as i32 - 1).sum();
    (format!("{src} return f0();"), sum)
}

/// **The compiler's depth and the interpreter's cap, forced to meet.**
/// `ENV_DISPLAY_CAP` is 8, so from 11 levels up the outermost binding is past
/// the cache and served by the parent-link walk. If the static count and the
/// runtime resolution ever disagreed by one at that depth, the read would land
/// on a neighbouring environment — and since every level's binding holds a
/// distinct value, the sum would change.
#[test]
fn a_capture_chain_deeper_than_the_env_display_cap_resolves_at_every_depth() {
    // `levels - 2` is the depth of `a0` from the innermost unit, so the range
    // spans both sides of the cap: cached below 8, walked at and above it.
    for levels in [2usize, 9, 10, 11, 14, 21] {
        let (src, want) = nested_capture_source(levels);
        let depth = levels - 2;
        assert_eq!(
            run_real(&src),
            want.to_string(),
            "{levels} nested env-owning functions: `a0` is {depth} hops out, \
             which is {} the display cap",
            if depth >= 8 { "past" } else { "within" }
        );
    }
}

/// The mirror image: a *write* at a depth past the cap must land in the right
/// environment, and the reads below must observe it. Writes and reads travel
/// different code paths (`env_write` vs `env_read`, `SetEnvSlot` vs
/// `GetEnvSlot`, and the two opcodes put `depth` in different operand
/// positions), so agreeing on reads is not agreement on writes.
#[test]
fn writes_at_a_depth_past_the_env_display_cap_land_in_the_right_environment() {
    // Ten env-owning levels, so `a0` is 9 hops out of the innermost `f9` —
    // past the cap, served by the parent-link walk. Each binding gets a
    // distinct power of two, so a write that landed one level off would still
    // show up in the sum.
    let mut assigns = String::new();
    let mut read_terms = Vec::new();
    for i in 0..10 {
        let bit = 1i64 << i;
        assigns.push_str(&format!("a{i} = {bit}; "));
        read_terms.push(format!("a{i} "));
    }
    let reads = read_terms.join("+ ");
    let mut src = format!("function f9() {{ {assigns}return {reads}; }}");
    for i in (0..9).rev() {
        src = format!(
            "function f{i}() {{ let a{i} = 0; {src} return f{}(); }}",
            i + 1
        );
    }
    src.push_str(" return f0();");
    // 1 + 2 + 4 + ... + 512 = 1023.
    assert_eq!(run_real(&src), "1023");
}

/// **The `u8` env-depth overflow, pinned as a value.** `Plans::env_depth_between`
/// used to count hops on a `u8`, so a capture chain deeper than 255 env-bearing
/// units overflowed: a debug build panicked, and a release build — with
/// `overflow-checks` off — wrapped the depth to `0`. The wrap emitted a
/// `GetEnvSlot` at depth 0, which resolves successfully against whatever
/// environment happens to be innermost, so it produced a **plausible wrong
/// number** rather than an error. Measured on 300 levels before the fix: debug
/// `attempt to add with overflow` (exit 101), release `55559` where node prints
/// `44551` (exit 0).
///
/// The depth counter is now `u16`, matching the depth operand the bytecode
/// carries (`WideOp::GetEnvSlotW.depth` is a `u16`, and `env_read` takes one), so
/// this depth is emitted through the wide op and resolves exactly.
///
/// Asserting the *value* is the point: every level holds a distinct number, so a
/// depth resolved even slightly wrong changes the sum, and a panic-free
/// assertion would have passed while release printed the wrong answer.
#[test]
fn a_capture_chain_past_the_narrow_depth_operand_resolves_to_the_correct_sum() {
    // On a 64 MiB stack: compiling N nested functions recurses once per level and
    // nextest gives each test a 2 MiB thread stack. That recursion limit is
    // pre-existing and byte-identical with and without the `u16` widening.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(a_capture_chain_past_the_narrow_depth_operand_body)
        .expect("spawn deep-nesting test thread")
        .join()
        .expect("deep-nesting test thread completed");
}

fn a_capture_chain_past_the_narrow_depth_operand_body() {
    // `levels - 2` is the depth of `a0` from the innermost unit. 300 puts that
    // at 298 — past the old `u8` budget, and past `ENV_DISPLAY_CAP` too, so the
    // read is served by the parent-link walk at a depth only the wide op can
    // name.
    for levels in [2usize, 254, 256, 257, 300, 600] {
        let (src, want) = nested_capture_source(levels);
        assert_eq!(
            run_real(&src),
            want.to_string(),
            "{levels} nested env-owning functions: `a0` is {} hops out, which {} \
             the narrow depth operand",
            levels - 2,
            if levels - 2 > u8::MAX as usize {
                "exceeds"
            } else {
                "fits within"
            }
        );
    }
}
