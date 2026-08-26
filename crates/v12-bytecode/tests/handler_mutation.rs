#![forbid(unsafe_code)]

//! Requirement 3: mutation testing of `FunctionBytecode::validate`.
//!
//! A fully valid nested handler table is the control; every single-field
//! mutant below must be rejected, and each assertion pins *which* invariant
//! fired, so a mutant that trips a different check than intended still
//! fails loudly.
//!
//! Base table (code length 40 words), sorted by start, properly nested:
//!   A: [0, 20)  target 5  depth 1   (outermost)
//!   B: [5, 15)  target 6  depth 2   (nested in A)
//!   C: [6, 8)   target 7  depth 3   (nested in B and A)

mod common;

use common::fn_with;
use v12_bytecode::{FunctionBytecode, HandlerRange};

const CODE_LEN: u32 = 40;

fn base() -> [HandlerRange; 3] {
    [
        HandlerRange {
            start: 0,
            end: 20,
            target: 5,
            stack_depth: 1,
        },
        HandlerRange {
            start: 5,
            end: 15,
            target: 6,
            stack_depth: 2,
        },
        HandlerRange {
            start: 6,
            end: 8,
            target: 7,
            stack_depth: 3,
        },
    ]
}

fn fb(handlers: Vec<HandlerRange>) -> FunctionBytecode {
    let mut fb = fn_with(CODE_LEN, handlers);
    fb.max_regs = 1;
    fb
}

#[track_caller]
fn must_reject(desc: &str, table: Vec<HandlerRange>, fragment: &str) {
    let Err(err) = fb(table).validate() else {
        panic!("MUTANT SURVIVED validate(): {desc}")
    };
    assert!(
        err.contains(fragment),
        "mutant {desc} was rejected by the wrong invariant:\n  got:    {err:?}\n  wanted: {fragment:?}"
    );
}

#[track_caller]
fn must_accept(desc: &str, table: Vec<HandlerRange>) {
    if let Err(err) = fb(table).validate() {
        panic!("control {desc} must validate cleanly, got: {err}");
    }
}

/// Single-field mutation of base entry `idx`.
fn mutate(idx: usize, f: impl Fn(&mut HandlerRange)) -> Vec<HandlerRange> {
    let mut table = base().to_vec();
    f(&mut table[idx]);
    table
}

#[test]
fn every_single_field_mutant_is_rejected() {
    // (description, mutated table, invariant fragment that must fire).
    // Fragments come from validate()'s messages: "empty or inverted",
    // "not sorted", "out of bounds", "partially overlaps",
    // "non-increasing stack depth".
    let mut cases: Vec<(&str, Vec<HandlerRange>, &str)> = Vec::new();

    // ---- A (outermost): start/end/target ----
    cases.extend([
        (
            "A.start == end",
            mutate(0, |h| h.start = 20),
            "empty or inverted",
        ),
        (
            "A.start > end",
            mutate(0, |h| h.start = 21),
            "empty or inverted",
        ),
        (
            "A.end shrunk inside B",
            mutate(0, |h| h.end = 14),
            "partially overlaps",
        ),
        (
            "A.end == start",
            mutate(0, |h| h.end = 0),
            "empty or inverted",
        ),
        (
            "A.target == len",
            mutate(0, |h| h.target = CODE_LEN),
            "out of bounds",
        ),
        (
            "A.target == u32::MAX",
            mutate(0, |h| h.target = u32::MAX),
            "out of bounds",
        ),
    ]);

    // ---- B (nested in A): start/end/target/depth ----
    cases.extend([
        (
            "B.start == end",
            mutate(1, |h| h.start = 15),
            "empty or inverted",
        ),
        (
            "B.start > end",
            mutate(1, |h| h.start = 16),
            "empty or inverted",
        ),
        (
            "B.start beyond A.end",
            mutate(1, |h| h.start = 21),
            "empty or inverted",
        ),
        (
            "B.end == start",
            mutate(1, |h| h.end = 5),
            "empty or inverted",
        ),
        (
            "B.end < start",
            mutate(1, |h| h.end = 4),
            "empty or inverted",
        ),
        (
            "B.end pokes out of A",
            mutate(1, |h| h.end = 21),
            "partially overlaps",
        ),
        (
            "B.target == len",
            mutate(1, |h| h.target = CODE_LEN),
            "out of bounds",
        ),
        (
            "B.target == u32::MAX",
            mutate(1, |h| h.target = u32::MAX),
            "out of bounds",
        ),
        (
            "B.depth == A.depth",
            mutate(1, |h| h.stack_depth = 1),
            "non-increasing stack depth",
        ),
        (
            "B.depth < A.depth",
            mutate(1, |h| h.stack_depth = 0),
            "non-increasing stack depth",
        ),
    ]);

    // ---- C (nested in B and A): start/end/target/depth ----
    cases.extend([
        (
            "C.start == end",
            mutate(2, |h| h.start = 8),
            "empty or inverted",
        ),
        (
            "C.start > end",
            mutate(2, |h| h.start = 9),
            "empty or inverted",
        ),
        (
            "C.start beyond A.end",
            mutate(2, |h| h.start = 22),
            "empty or inverted",
        ),
        (
            "C.end == start",
            mutate(2, |h| h.end = 6),
            "empty or inverted",
        ),
        (
            "C.end < start",
            mutate(2, |h| h.end = 5),
            "empty or inverted",
        ),
        (
            "C.end pokes out of B",
            mutate(2, |h| h.end = 16),
            "partially overlaps",
        ),
        (
            "C.end pokes out of A",
            mutate(2, |h| h.end = 21),
            "partially overlaps",
        ),
        (
            "C.target == len",
            mutate(2, |h| h.target = CODE_LEN),
            "out of bounds",
        ),
        (
            "C.target == u32::MAX",
            mutate(2, |h| h.target = u32::MAX),
            "out of bounds",
        ),
        (
            "C.depth == B.depth",
            mutate(2, |h| h.stack_depth = 2),
            "non-increasing stack depth",
        ),
        (
            "C.depth == A.depth (skips level)",
            mutate(2, |h| h.stack_depth = 1),
            "non-increasing stack depth",
        ),
        (
            "C.depth < B.depth",
            mutate(2, |h| h.stack_depth = 0),
            "non-increasing stack depth",
        ),
    ]);

    let total = cases.len();
    let mut survived = Vec::new();
    for (desc, table, fragment) in cases {
        match fb(table.clone()).validate() {
            Ok(()) => survived.push(desc),
            Err(err) => assert!(
                err.contains(fragment),
                "mutant {desc} rejected by the wrong invariant:\n  got:    {err:?}\n  wanted: {fragment:?}"
            ),
        }
    }
    assert!(
        survived.is_empty(),
        "{}/{} mutants were accepted by validate(): {survived:?}",
        survived.len(),
        total
    );
}

#[test]
fn every_non_sorted_ordering_is_rejected_identity_passes() {
    let b = base();
    let perms: [(usize, usize, usize); 6] = [
        (0, 1, 2),
        (0, 2, 1),
        (1, 0, 2),
        (1, 2, 0),
        (2, 0, 1),
        (2, 1, 0),
    ];

    for &(i, j, k) in &perms {
        let table = vec![b[i], b[j], b[k]];
        if (i, j, k) == (0, 1, 2) {
            must_accept("sorted identity permutation", table);
        } else {
            must_reject(
                &format!("permutation ({i},{j},{k}) is unsorted"),
                table,
                "not sorted",
            );
        }
    }
}

#[test]
fn wholesale_inverted_nesting_is_rejected() {
    // Same extents, depths assigned inversely to nesting depth.
    let mut table = base().to_vec();
    table[0].stack_depth = 3;
    table[1].stack_depth = 2;
    table[2].stack_depth = 1;
    must_reject(
        "inverted nesting depths (3,2,1)",
        table,
        "non-increasing stack depth",
    );
}

#[test]
fn valid_tables_documenting_the_acceptance_boundary() {
    must_accept("base nested table", base().to_vec());

    // Target at the last valid pc: inclusive upper bound.
    let mut t = base().to_vec();
    t[0].target = CODE_LEN - 1;
    must_accept("target == len - 1", t);

    // The outermost handler has no parent, so nothing imposes a *lower*
    // bound on its depth; 0 is legal because children only need to be
    // strictly deeper. (An upper extreme like u32::MAX would NOT be legal:
    // children could not exceed it — the inverted-nesting test pins that.)
    must_accept("outermost depth == 0", mutate(0, |h| h.stack_depth = 0));

    // Duplicate extent nested inside itself with a deeper stack: starts may
    // be equal (sorting permits ties), containment holds, depth increases.
    let mut dup = base().to_vec();
    dup.push(HandlerRange {
        start: 6,
        end: 8,
        target: 7,
        stack_depth: 4,
    });
    must_accept("duplicate extent with deeper stack", dup);

    // Non-overlapping sibling at the same depth as an unrelated handler:
    // the depth rule applies only within a nesting chain.
    let mut sib = base().to_vec();
    sib.push(HandlerRange {
        start: 20,
        end: 30,
        target: 7,
        stack_depth: 1,
    });
    must_accept("sibling range reusing depth 1", sib);

    // Adjacent ranges touching at an endpoint are disjoint under the
    // half-open [start, end) convention, so shrinking A to [0,5) next to
    // B's [5,15) stays valid.
    let mut adj = base().to_vec();
    adj[0].end = 5;
    must_accept("adjacent ranges touching at endpoints", adj);

    // No handlers at all is trivially fine.
    must_accept("empty handler table", Vec::new());
}

#[test]
fn zero_max_regs_is_rejected_even_with_no_handlers() {
    let mut fb = fb(Vec::new());
    fb.max_regs = 0;
    assert_eq!(
        fb.validate().unwrap_err(),
        "max_regs must be greater than zero"
    );
}
