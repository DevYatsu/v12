//! Every `Array.prototype` method that touches an index or `length` must
//! agree with a plain property read **on the same object, at the same index,
//! in the same tick** — and, where v12 is known to differ, the difference must
//! be a NAMED divergence rather than a transcription.
//!
//! # The defect this pins
//!
//! `Array.prototype` methods are generic in the spec: they are defined over
//! `LengthOfArrayLike(O)` = `ToLength(Get(O, "length"))` plus
//! `HasProperty`/`Get`/`Set`/`Delete` at integer keys, and the spec has no
//! element store for a generic receiver at all. v12 had five index readers and
//! five length readers; the two in `builtins/array.rs` probed the `elements`
//! store **first**, inverting the precedence `ordinary_get` uses (shape first),
//! so a method's write landed somewhere the property read never consulted:
//!
//! ```js
//! var o = {0:"a", length:3};
//! Array.prototype.fill.call(o, "q", 0, 3);
//! o[0]                                // v12 "a"   node "q"
//! Array.prototype.indexOf.call(o,"q")  // v12  0    node  0
//! ```
//!
//! # The oracle is GENERATED, and this file used to be its own worst enemy
//!
//! Until this rewrite the expectations below were **hand-transcribed** —
//! node's output copied by hand, row by row. That cannot report its own
//! errors, and it did not: Batch 1 marked the `sparse`/`holey` columns as
//! matching `node` while `new Array(3)` followed by `a[0] = "z"` reported
//! `length` 1 where node reports 3. Two things now prevent a repeat.
//!
//! 1. **The `expected` values are generated.**
//!    `scripts/gen-array-receiver-table.py` reads
//!    `scripts/array-receiver-table.txt` (the receiver/row source), builds the
//!    observation program, runs it under `node`, and writes
//!    `generated/array_receiver_table.rs`. Regenerating is a **command**:
//!
//!    ```text
//!    ./scripts/gen-array-receiver-table.py
//!    ```
//!
//!    The program text is committed verbatim, so the Rust test executes
//!    byte-for-byte what `node` answered — there is no second harness in Rust
//!    that could drift from the oracle's.
//!
//! 2. **The row source is digest-linked to the generated file.** Edit a row in
//!    `scripts/array-receiver-table.txt` without regenerating and
//!    `the_generated_table_matches_its_row_source` fails on the FNV-1a/64
//!    digest, instead of the row being quietly compared against a stale
//!    oracle.
//!
//! **The second failure mode was subtler and needed the table itself fixed.**
//! Every row in the old table was an `Array.prototype` *method* call, and every
//! method either writes all indices in range (`fill`, `copyWithin`) or routes
//! its length through `set_array_len` (`push`, `pop`, `splice`) — so **no row
//! could ever observe a partial index write below a declared length**, which is
//! the only thing that reaches `ArraySetLength`. The 315-row version of this
//! table measured the same 105 divergences with and without the `new Array(n)`
//! fix in it. The four `index_assign_*` / `length_lower` rows exist for that
//! reason and are the only non-method rows in the file.
//!
//! # Self-consistency — the mutation-sensitive half
//!
//! `self_consistency_over_the_whole_corpus` compares the receiver against
//! ITSELF after a mutating call, with no oracle at all, so it still catches a
//! reader that disagrees with `ordinary_get` even if `node` moves.
//!
//! # What `KNOWN_DIVERGENCES` is, and is not
//!
//! It is a ratchet in BOTH directions: the test recomputes the divergence set
//! and requires it to equal the declared set, so a new divergence fails and a
//! FIXED one also fails (until the entry is deleted). An entry marked
//! `absent: true` additionally has its excuse checked mechanically — v12's
//! observation must actually contain `threw:TypeError`, so a row cannot be
//! excused as "not implemented" while v12 quietly returns a wrong value.
//!
//! Measured at this commit: **177 of 663 rows diverge, across all 39 methods**
//! (17 receivers × 39 rows). 85 of those are five unimplemented methods on all
//! seventeen receivers and say nothing about the index/length model. The rest is
//! five clusters, itemised at [`KNOWN_DIVERGENCES`]: the pre-existing
//! `Object.keys`-after-delete defect (17 rows), two accessor defects that no
//! pre-Batch-6 receiver could reach (E1 and E2 below, 21 rows with a
//! behavioural difference plus 47 rows that differ only in the harness's key
//! count), the arguments-exotic `index_assign_past` row, and the seven
//! `frozen_object` rows Batch 7 added.
//!
//! **Batch 7 fixed one of the three defects it set out to fix, and could not
//! fix the other two — both because their fix lives outside this batch's file
//! set.** What was settled here, per defect:
//!
//! * **E3 is FIXED.** A `delete`d property handed the internal `Hole` tag to
//!   JavaScript. Seven rungs disagreed about what a `hole` cell means; they now
//!   all say "absent", which is what ES 10.1.8 `OrdinaryGet` step 2 says. See
//!   [`KNOWN_HOLE_READ_ERRORS`] and
//!   [`a_deleted_property_never_hands_javascript_the_hole_sentinel`].
//! * **E2 is OPEN and is not patch-shaped.** `FunctionTarget::Bytecode` is a
//!   bare index into a program table that `Ctx`'s single `&mut Heap` cannot
//!   reach, so the two `invoke_accessor_getter` call sites cannot be repaired
//!   in place — it needs seam interception (25 methods) or a `NativeHandler`
//!   signature change.
//! * **E1 is OPEN and the fix is in `v12-bytecode` + `v12-bccompiler`**, which
//!   this batch may not modify: `DefineAccessor` carries no attribute operand,
//!   so the interpreter cannot tell a class accessor from a literal one.
//!
//! # `frozen_object` — the integrity-level rung (Batch 7)
//!
//! Every one of the previous sixteen receivers has a WRITABLE `length` and
//! writable indices, so no row had ever reached the branch where a required
//! `[[Set]]`/`[[Delete]]` must be **refused**. That branch is reached by
//! `Object.freeze`, and it found the worst defect in this file's history:
//! `Array.prototype.reverse.call(Object.freeze({0:"a",length:3}))` **deleted the
//! non-configurable own property `0`** — `o[0]` became `undefined`,
//! `Object.keys(o)` dropped it — while `Object.isFrozen(o)` still answered
//! `true`. That is state corruption plus a false integrity report, not merely a
//! missing `TypeError`. `copyWithin` and `shift` do the same delete; `fill`,
//! `unshift`, `splice` and `pop` complete silently after a refused write.
//! `push` is correct, so the refusal machinery is reachable and it is the
//! observation of it that is missing. Recorded, **not fixed** — see
//! [`FROZEN_REFUSAL_UNOBSERVED`].
//!
//! # Every number in this file's history was stated against 351 rows
//!
//! The receiver set went 9 → 16 in Batch 6, so **the denominator moved and the
//! series is not comparable across that boundary**. Restated:
//!
//! | batch | receivers × rows | divergences | methods |
//! |---|---|---|---|
//! | ADR-0004 as filed | 1 × 306 | 24 | 19 |
//! | batch 3 (generated) | 9 × 351 | 108 | — |
//! | batch 4 | 9 × 351 | 80 | 14 |
//! | batch 5 | 9 × 351 | 54 | 10 |
//! | batch 6 | 16 × 624 | 165 | 39 |
//! | **batch 7 (this one)** | **17 × 663** | **177** | **39** |
//!
//! The 24 → 108 step is the one that matters: it is what a stronger instrument
//! found when a transcribed table was replaced by a generated one. The
//! 108 → 165 step is the same thing one level further out — seven more spec
//! decision points — and 165 → 177 is the same thing again: ONE receiver
//! (`frozen_object`) added twelve rows, five of them the unimplemented methods
//! re-counted on the new receiver and seven of them a defect no earlier
//! receiver could reach. In both steps **no row an earlier batch claimed fixed
//! has come back**
//! (`slice`/`map`/`concat`/`sort`/`reverse`/`splice` are still clean on every
//! receiver without an accessor, and the six "hole materialised as an own
//! `undefined`" rows stay fixed on `holey_object`, which is a holey receiver no
//! earlier batch had).
//!
//! What did change is *coverage*: on the six new receivers that carry no
//! accessor, 5 of 39 rows diverge (`holey_object`, `length_float`,
//! `shadowed_index`, `proto_chain_two_deep`, `length_negative` — the last three
//! diverge only through the five unimplemented methods). `shadowed_index`,
//! `proto_chain_two_deep` and `length_negative` are **clean**: a receiver that
//! makes a method decide between own and inherited, or walk two hops, or clamp
//! a negative length, is answered correctly by all 34 implemented methods.
//!
//! # The NAMED GAP of batch 4 is CLOSED, and it was load-bearing
//!
//! Batch 4 recorded a gap in this table rather than fixing it: "`visible_extent`
//! short-circuits every index at or past the receiver's highest visible index
//! to 'hole' without dispatching, so the `index_get` presence branch only runs
//! BELOW the extent. **A hole strictly below the visible extent of a non-`Array`
//! receiver is exercised at NO index in 351 rows.**"
//!
//! `holey_object` (`{0:"a", 2:"c", length:3}`) closes it, and the claim is now
//! measured rather than argued:
//!
//! * Instrumenting `read_slots` shows that across all 39 rows of all NINE
//!   pre-Batch-6 receivers there are **30 dispatched index reads and 0 of them
//!   answer `present == false`** — the branch's false arm was genuinely
//!   unreachable.
//! * On `holey_object`, `visible_extent` returns 3 (one past the highest
//!   integer key, 2), index 1 is inside the dispatch range, and the instrumented
//!   read reports `index 1 -> present=false`.
//! * Mutation: drop the `present` bit at `read_slots`'s `index_get` call. The
//!   nine old receivers stay at **exactly 54** divergent rows — the mutation is
//!   invisible, as batch 4 said. `holey_object` goes 11 → 13 divergent rows,
//!   the two new ones being `slice` and `reverse`, and six rows that already
//!   diverged for the delete-count reason change content as well.
//!
//! Two new receivers then found two defects no earlier receiver could see, and
//! they are unrelated to the index/length model, so they are named apart:
//! **E1** an object-literal accessor is created non-enumerable (so `Object.keys`
//! omits it — a key-count difference charged to 47 rows) and **E2**
//! `invoke_accessor_getter` answers `undefined` for a bytecode getter, so
//! `array_like_len`/`index_get` read 0/`undefined` where the interpreter's own
//! `[[Get]]` on the same object reads the accessor. E2 is visible as a split
//! *inside one receiver*: on `length_getter` the callback-seam methods read
//! `length` 3 and agree with node, and the native-path methods read 0. Both
//! texts are on [`KNOWN_DIVERGENCES`].
//!
//! Adding `holey_object` to the **oracle-free** half of this file
//! ([`self_consistency_over_the_whole_corpus`]) then found a third, **E3**: a
//! `delete`d own shape-backed property keeps its descriptor with a hole in the
//! cell, and reading it hands JS the hole tag instead of `undefined` — so
//! `String(o[k])` raises an internal error and `o[k] === undefined` is `false`,
//! where node answers cleanly. It is the same delete as the
//! `Object.keys`-after-delete cluster (one cause, two symptoms) and it was
//! invisible to both halves until a receiver had an own index that a mutator
//! deletes *and* that stays inside the post-mutation `length`. The four affected
//! pairs are listed, and ratcheted, in [`KNOWN_HOLE_READ_ERRORS`].
//!
//! None of the three is fixed here: this batch measures, the next one fixes.
//!
//! # "Holes materialised as present `undefined`" — FIXED, and it was one cause
//!
//! Six methods (`slice`, `map`, `concat`, `splice`, `sort`, `reverse`) used to
//! report a hole as a present own `undefined`, in the array they return or in
//! the receiver they write back. The shared hypothesis was that this was one
//! root cause, and it holds — with the interesting part being *how many sites
//! it needed*, which was not one:
//!
//! 1. `builtins/array.rs`'s range reader returned `Vec<JsValue>`, in which a
//!    hole and a present `undefined` are the same value, and the write-back
//!    helpers had no way to say "delete this index" instead of "set it to
//!    `undefined`".
//! 2. `map` does not run there at all — it is a callback method and runs at
//!    the interpreter seam, which had its own copy of the mistake
//!    (`vec![undefined; len]` as the pre-sized result).
//! 3. `sort` does not run there either. `NativeId::ArraySort` is always
//!    intercepted by `Interp::run_array_callback`, so
//!    `builtins/array::array_sort` never sees a JS call and the seam's
//!    `array_sort_callback` was the live copy.
//!
//! Three sites, one invariant, and the invariant is now expressed as a type in
//! `builtins/array.rs` (`type Slot = Option<JsValue>`) rather than as a comment
//! at each of them. `Array.from` is the deliberate exception — its spec step is
//! an unconditional `CreateDataProperty` per index — and says so where it
//! flattens.
//!
//! The invariant is pinned from the outside by
//! [`a_hole_is_never_materialised_as_an_own_undefined`], which is also the
//! place that records which of the six can *observe* the difference at all:
//! `slice`, `concat`, `map` and `splice`'s removed array can, because `i in
//! result` is false; `sort` and `reverse` cannot in their return value (both
//! return the receiver) and are observable only through `hasOwnProperty` on the
//! receiver afterwards.
//!
//! # The NAMED GAP batch 4 found in the table — CLOSED, see the header
//!
//! Kept as the record of what was true when it was written, because the header
//! section that closes it cites these numbers. Mutation: flatten the
//! generic-receiver branch of the range reader (drop the `present` bit on the
//! `index_get` answer). On the 351-row / 9-receiver table this did **not**
//! move the divergence count at all — 80/14 at batch 4, 54/10 at batch 5 — and
//! only the focused test failed.
//!
//! The reason was structural: `visible_extent` short-circuits every index at or
//! past the receiver's highest visible index to "hole" without dispatching, so
//! the `index_get`-presence branch only ran for indices BELOW the extent, and
//! every one of those was present. **A hole strictly below the visible extent of
//! a non-`Array` receiver was exercised at no index at all in 351 rows.** (One
//! correction to the note batch 4 left: `{0:"a", 2:"c", length:3}` has extent
//! **3**, not 2 — `visible_extent` returns one *past* the highest integer key.
//! The conclusion is unaffected, and index 1 is below 3, which is the whole
//! reason the receiver works.)
//!
//! `holey_object` is that receiver, added in batch 6; the measured proof is in
//! the header.
//!
//! # Deliberately NOT covered here
//!
//! * The **direct** call form (`a.forEach(…)` on a real array) is a different
//!   code path from `Array.prototype.forEach.call(a, …)` and has its own
//!   divergences — the fast path visits holes that node skips. Every row here
//!   uses the `call` form, which is what ADR-0004 is about.
//! * `JSON.stringify` is deliberately NOT the observer (json.rs renders an
//!   array from its element lattice rather than its `length`, which would
//!   charge one pre-existing defect to every row returning an array); and
//!   `to_display_string` is not either, because v12 renders arrays
//!   comma-separated. The harness describes values through property reads only.
//! * The `sixth_reader` marker this file used to carry in its header —
//!   "the callback seam is a sixth copy of the straddle, out of scope" — is
//!   **STALE and removed**. Batch 2 landed it: `callback_dense_bound` is deleted
//!   and `callback_elem` became `callback_index`, and both `callback_len` and
//!   `callback_index` now read through `Interp::get_property`/`Interp::op_in`,
//!   the interpreter's own `[[Get]]`/`[[HasProperty]]`, instead of a private
//!   store probe. (Verified at this commit: `callback_len` survives at
//!   `v12-interp/src/call_setup.rs:2962` — the *name* was kept, its body was
//!   rewritten — so "all three were deleted" would be the wrong claim to leave
//!   behind.) What remained at that seam was a *different* defect, and it is
//!   **now FIXED** — see the find-family section below.
//!
//! # The find family: direction and holes — FIXED, and it was TWO causes
//!
//! `findLast`/`findLastIndex` walked `0..len` like their forward twins, and all
//! four of `find`/`findIndex`/`findLast`/`findLastIndex` *skipped holes* where
//! their spec step has no `HasProperty` guard. That is why all four were
//! tracked as one cluster and why the batch could not take them one at a time:
//! the two causes are separable per method but not per cluster, and on 5 of the
//! 9 receivers a `findLast` row needed **both** fixed before it could agree —
//! so fixing only the direction would have left 10 of the 18 rows still
//! diverging and the cause still unidentified.
//!
//! Three details were wrong *together*, which is what made this look like one
//! small bug for so long:
//!
//! 1. **Direction.** Only `reduceRight` got `.rev()`.
//! 2. **Early exit.** Only `find`/`findIndex` broke out of the loop;
//!    `findLast`/`findLastIndex` fell through and kept the LAST match. Those two
//!    mistakes *cancelled*: keeping the last FORWARD match is the highest
//!    index, which is what a backward search returns first. So on any receiver
//!    where the predicate matched at most one index the **return value was
//!    already correct** and only the recorded visitation order revealed it —
//!    which is why the generated table needed an always-true predicate to catch
//!    `findLast` at all, and why the old ledger entry had to confess that in
//!    prose.
//! 3. **Hole visiting**, scoped to these four methods and NOT to the seam:
//!    `forEach`/`map`/`filter`/`every`/`some`/`reduce`/`reduceRight` are all
//!    "if `k` is present" in the spec, `map` in particular writes an own
//!    property only at an index it visited, and the seven are pinned as still
//!    skipping holes.
//!
//! Pinned from the outside by
//! [`find_last_walks_backward_and_stops_at_the_first_match`],
//! [`the_find_family_visits_holes_and_nothing_else_does`] and
//! [`a_present_undefined_is_not_a_hole_for_the_find_family`]. The last of those
//! exists because a hole and a present `undefined` are indistinguishable from
//! inside the callback by any test on the *value* — `node` answers the same for
//! both receivers under `typeof x === "undefined"`, verified by running it — so
//! the predicate has to ask `!(i in o)` of the receiver it was handed.
use std::collections::{BTreeMap, BTreeSet};

use v12_engine::Engine;

include!("generated/array_receiver_table.rs");

fn eval_json(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// Runs `Array.prototype.<method>.call(o, pred)` on a receiver declared by
/// `decl`, and reports BOTH the return value and the predicate's visit order as
/// `["<String(result)>", "i:i:i"]`.
///
/// The two halves are reported separately on purpose: on a dense receiver a
/// forward `findLast` walk returns the right *value* (the ledger's
/// `walks FORWARD` entry says exactly this) and only the visit order exposes
/// it, so a test written on the return value alone would pass against the bug.
///
/// The result goes through `String`, so it is always a JSON string and `nil`
/// never appears — `JSON.stringify([undefined])` is `[null]` in `node` and this
/// harness must not depend on that.
fn probe(decl: &str, method: &str, pred: &str) -> String {
    eval_json(&format!(
        r#"
        {decl}
        var vis = [];
        var ret = Array.prototype.{method}.call(o, function (x, i) {{
            vis.push(i);
            return ({pred});
        }});
        JSON.stringify([String(ret), vis.join(":")]);
        "#
    ))
}

/// `0..len` in the order the method is specified to walk it, comma-joined.
///
/// Computed from the spec's direction, never copied from an oracle: `findLast`
/// and `findLastIndex` are specified to iterate `k` from `len-1` **down to**
/// `0`, and the other two from `0` up to `len-1`.
fn walk_order(method: &str, len: u32) -> String {
    let idx = walk(method, len);
    idx.iter().map(u32::to_string).collect::<Vec<_>>().join(":")
}

/// The one index a backward walk visits first, and the only one a forward walk
/// visits first. Split out because "which index does the walk START at" is the
/// half of the direction invariant an always-true predicate can see.
fn first_index(method: &str, len: u32) -> String {
    walk(method, len)
        .first()
        .expect("a non-empty walk")
        .to_string()
}

fn walk(method: &str, len: u32) -> Vec<u32> {
    if method.starts_with("findLast") {
        (0..len).rev().collect()
    } else {
        (0..len).collect()
    }
}

/// Compares one observation against the same observation's value in `node`.
fn assert_is(actual: &str, expected: &str, what: &str) {
    assert_eq!(actual, expected, "{what}");
}

// ---------------------------------------------------------------------------
// 1. Generator linkage — the anti-staleness gate.
// ---------------------------------------------------------------------------

/// FNV-1a/64. A byte length alone would let a same-length edit through, and a
/// real hash needs no dependency; the generator computes the identical value.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[test]
fn the_generated_table_matches_its_row_source() {
    let source = include_str!("../../../scripts/array-receiver-table.txt");
    assert_eq!(
        TABLE_SOURCE_BYTES,
        source.len(),
        "scripts/array-receiver-table.txt changed size since the table was \
         generated — re-run ./scripts/gen-array-receiver-table.py"
    );
    assert_eq!(
        TABLE_SOURCE_FNV1A,
        fnv1a64(source.as_bytes()),
        "scripts/array-receiver-table.txt changed since the table was generated \
         — re-run ./scripts/gen-array-receiver-table.py. A stale table compares \
         v12 against node's answers for rows v12 no longer runs, which is the \
         exact failure mode this gate exists to stop."
    );
}

// ---------------------------------------------------------------------------
// 2. The node-differential table, with its divergences named.
// ---------------------------------------------------------------------------

/// Every receiver, in row-source order. **Seventeen**, not nine: the seven added
/// in Batch 6 are the seven spec decision points the first nine could not
/// reach — a hole strictly below the visible extent, a getter at an index, a
/// getter as the length, an own index shadowing an inherited one, a prototype
/// chain two hops deep, a fractional `length`, and a negative `length` — and
/// `frozen_object` (Batch 7) is the one *integrity level*: every other receiver
/// has a **writable** `length` and **writable** indices, so no row had ever
/// reached the branch where a required `[[Set]]`/`[[Delete]]` must be REFUSED.
/// The justification for each is in `scripts/array-receiver-table.txt`; the
/// measured consequence is in the header.
const ALL_RECEIVERS: &[&str] = &[
    "own_numeric_length",
    "own_string_length",
    "inherited_indices",
    "inherited_length",
    "arguments_exotic",
    "array_dense",
    "array_holey",
    "sparse_ctor",
    "length_assigned",
    "holey_object",
    "accessor_index",
    "length_getter",
    "shadowed_index",
    "proto_chain_two_deep",
    "length_float",
    "length_negative",
    "frozen_object",
];

/// The seven mutating rows `frozen_object` diverges on, and what it found.
///
/// **Not fixed in this batch, by instruction** — the receiver was added to
/// *measure* the integrity-level rung, and the answer is worse than "a missing
/// `TypeError`":
///
/// * `Array.prototype.reverse.call(Object.freeze({0:"a",length:3}))` **deleted
///   the non-configurable own property `0`** — `o[0]` became `undefined` and
///   `Object.keys(o)` dropped it — while `Object.isFrozen(o)` still answered
///   `true`. That is a state-corrupting violation, not just an unthrown one:
///   the object is left reporting an integrity level it no longer has.
///   `copyWithin` and `shift` do the same delete. `index_delete`
///   (`builtins/array.rs`) holes the cell with no descriptor check at all.
/// * `fill` refuses to write (the read-only index keeps its value) but returns
///   the receiver instead of throwing, because `index_set` discards
///   `dispatch_set`'s `false`.
/// * `unshift` computed `length` 4, **wrote** nothing, and **returned 4**.
/// * `splice`/`pop` computed a new `length`, could not write it, and returned
///   normally.
///
/// `push` is the one mutator that agrees: it throws, because adding index 3 hits
/// the non-extensible check. So the refusal machinery exists and is reachable —
/// it is the two steps that must *observe* it (`DeletePropertyOrThrow`,
/// `Set(O, k, v, true)`) that are missing.
const FROZEN_REFUSAL_UNOBSERVED: &str = "FROZEN: on an integrity-level receiver every mutating method whose spec step \
     requires a write to succeed proceeds as if it had. reverse/copyWithin/shift additionally DELETE the \
     non-configurable own index (Object.isFrozen still answers true afterwards), fill returns the receiver \
     after a refused index write, unshift returns a length it could not write, and splice/pop return normally \
     after a refused length write. push is correct (it throws), so the refusal is reachable — it is the \
     DeletePropertyOrThrow / Set(...,true) observation of it that is missing.";

/// The two receivers that carry an accessor property. Both diverge on almost
/// every row, for two reasons that have nothing to do with each other and are
/// therefore almost always named separately in the ledger below.
const ACCESSOR_RECEIVERS: &[&str] = &["accessor_index", "length_getter"];

/// Cause **E1**: an accessor written in an object literal is registered
/// **non-enumerable**, so `Object.keys` omits it while node reports it.
///
/// **STILL OPEN after Batch 7 — the fix is in a crate this batch may not
/// touch.** `Interp::op_define_accessor` hardcodes `Attrs::BUILTIN`, and
/// `Attrs::BUILTIN` is `WRITABLE | CONFIGURABLE`, i.e. `enumerable == false`.
/// That is *correct* for a class accessor (`class C { get g(){} }` must be
/// non-enumerable — verified against node) and wrong for an object-literal one
/// (`{ get g(){} }` must be enumerable). Both reach the same opcode
/// (`DefineAccessor = 66`), from `v12-bccompiler`'s `expr.rs:771` (literal) and
/// `class.rs:173`/`:180` (class), and `Opcode::DefineAccessor`'s three operands
/// are `Obj`, `Key`, `Pair` — there is **no attribute operand**, so the
/// interpreter cannot tell the two spellings apart. The fix is one attribute
/// carried from the compiler: an attribute operand on `DefineAccessor`, or a
/// sibling opcode for the literal spelling. Either needs
/// `crates/v12-bytecode/**` and `crates/v12-bccompiler/**`.
///
/// Nothing depends on the old value. The *only* consumer of the literal's
/// enumerability in this table is `Object.keys`, and the class accessor stays
/// non-enumerable either way because it takes the same code path with a
/// different attribute. Measured at this commit: `{ m(){} }` (a method
/// shorthand) IS enumerable, because `expr.rs`'s `object_prop` routes `Init`
/// through `SetProperty` and never reaches `op_define_accessor` — so this is
/// the accessor spelling only.
///
/// This is NOT an index/length defect and it is charged to every row of both
/// accessor receivers through the harness's `Object.keys(o).length` column —
/// the same conflation the harness docstring avoids for `JSON.stringify`. It
/// is reported rather than hidden, because suppressing it would mean the table
/// quietly stopped being a differential table on two of seventeen receivers.
/// Verified at this commit: `Object.getOwnPropertyNames({0:"a", get length(){…}})`
/// answers `0,length` in v12 (the key IS in the shape) while
/// `getOwnPropertyDescriptor(…).enumerable` answers `false` where node answers
/// `true`, which is what makes `Object.keys` drop it.
const E1_ACCESSOR_NON_ENUMERABLE: &str = "E1: an object-literal accessor is created non-enumerable, so `Object.keys` omits it \
     (v12 `getOwnPropertyDescriptor(o, k).enumerable` is false where node's is true) and the \
     harness's key count is one low. An `Object.keys` defect, not an index/length one. OPEN: \
     `DefineAccessor` carries no attribute operand, so the fix is in v12-bytecode + v12-bccompiler.";

/// Cause **E2**: `internal_methods::invoke_accessor_getter` cannot run a
/// **bytecode** getter — it matches `FunctionTarget::Native`/`Host` and returns
/// `undefined` for `Bytecode`, with a comment saying the interpreter's
/// `get_property` path provides it. So the engine's own `[[Get]]` and the
/// interpreter's `[[Get]]` disagree on the same object at the same key.
///
/// **STILL OPEN after Batch 7, and this batch established that it is NOT a
/// patch-shaped defect — it is the `NativeHandler` signature.** There are
/// exactly **two** call sites of `invoke_accessor_getter` and both are inside
/// `ordinary_get` (its dictionary rung and its shape rung); `ordinary_set` has
/// the mirror-image hole for a *setter*, recorded there as "v1: setter
/// invocation is a no-op beyond acknowledgement". Neither site is repairable in
/// place. `FunctionTarget::Bytecode(u32)` is a bare index into a
/// `Program.functions` table that lives in `Interp` / `NativeRegistry` and
/// **not in the heap**, while every native reaches `[[Get]]` through
/// `Ctx { heap: &'a mut Heap, .. }` — one exclusive borrow, no machine.
/// `CONTEXT.md` already states the constraint ("natives cannot re-enter the
/// interpreter"); `Ctx` makes it structural, because a re-entrant call
/// capability would need a second `&mut Heap`.
///
/// The two fixes the architecture does sanction are both large and neither is
/// this batch's:
///
/// 1. **Route the read through the seam.** `Interp::dispatch_native` already
///    intercepts every built-in that must re-enter the machine, and
///    `ArrayJoin`/`ArrayPush` carry hand-written `*_fallback` bodies in
///    `v12-interp/src/object_ops.rs` for exactly this reason. Twenty-five array
///    methods would need one.
/// 2. **Change the handler type.** `NativeHandler` is
///    `fn(&mut Heap, JsValue, &[JsValue])` across every registration; giving it
///    machine access is a signature change plus a re-entrancy audit.
///
/// A third possibility — rebuilding a nested `Interp` from the registry, the way
/// `NativeRegistry::eval` does — is *not* a fix and was rejected: a getter body
/// would run with no job-queue checkpoint and no access to the caller's frame
/// chain, so a `Promise.then` inside a getter would deadlock the drain rather
/// than run it.
///
/// That is observable as a split INSIDE one receiver: on `length_getter`, the
/// seven callback-seam methods (`map`, `forEach`, `filter`, `every`, `some`,
/// `find*`, `flatMap`, `reduce*`) read `length` 3 and agree with node on both
/// the visit list and the returned array, while the native-path methods
/// (`slice`, `fill`, `indexOf`, `at`, `reverse`, `pop`, `shift`, `splice`,
/// `unshift`, `copyWithin`) read `length` 0 — because `array_like_len` and
/// `index_get` reach the getter through `dispatch_get`. Same receiver, same
/// property, two answers; no receiver before Batch 6 could show it.
const E2_ACCESSOR_GETTER_UNREADABLE: &str = "E2: `invoke_accessor_getter` answers `undefined` for a bytecode getter, so \
     `array_like_len`/`index_get` (which reach [[Get]] via `dispatch_get`) read 0/undefined where \
     the interpreter's own [[Get]] on the same object reads the accessor. The callback seam, which \
     reads through `Interp::get_property`, agrees with node on these receivers and the native path \
     does not. OPEN: not patch-shaped — `FunctionTarget::Bytecode` indexes a program table that \
     `Ctx`'s bare `&mut Heap` cannot reach; it needs seam interception or a handler-signature change.";

/// One row of the divergence ledger.
struct Known<'a> {
    /// Method (or row) name, as it appears in the row source.
    method: &'a str,
    /// Receivers on which it diverges. Must be the COMPLETE set: a receiver
    /// that starts (or stops) diverging flips this test.
    receivers: &'a [&'a str],
    /// The method does not exist in v12; the excuse is checked, not trusted.
    absent: bool,
    /// What is actually wrong. `absent: true` rows are excluded here.
    why: &'a str,
}

/// The complete, current divergence set: **177 of 663 rows, 39 methods**
/// (17 receivers × 39 rows).
///
/// A `Known` entry is one (method, receivers, cause) triple, and **a method may
/// appear more than once** when it diverges on different receivers for
/// different reasons — [`declared_divergences`] unions the receiver sets
/// instead of letting a later entry overwrite an earlier one, which is what
/// makes per-cause attribution possible without a second table.
///
/// How the 177 rows divide, because the raw total is close to meaningless on
/// its own:
///
/// * **85 rows** are the five `absent` methods on all seventeen receivers. They
///   say nothing about the index/length model; they were 45 of 351 before
///   Batch 6 and are still the same five methods.
/// * **17 rows** are the pre-existing "a deleted own index stays in
///   `Object.keys`" cluster, now on six receivers instead of four (the six
///   `holey_object` rows and the three `length_float` rows are new; the eight
///   `own_*`/`array_*` rows are the ones earlier batches recorded). **Batch 7
///   did not move this cluster**, deliberately: `Object.keys`,
///   `Object.getOwnPropertyNames` and `for-in` read the *descriptor list*
///   through `collect_own_string_keys`, which consults liveness nowhere, so
///   they are untouched by a fix to the `[[Get]]`/`[[HasProperty]]` cells.
/// * **21 rows** are genuinely new behavioural divergences, on the two accessor
///   receivers, from E1 and E2 above.
/// * The remaining rows on the accessor receivers (47 of the 64 that differ
///   ONLY in the harness's key count) are E1 alone.
/// * **12 rows** are the `frozen_object` receiver: five `absent` methods
///   re-counted on the new receiver, and seven mutating methods that proceed
///   after a refused write (`FROZEN_REFUSAL_UNOBSERVED`).
///
/// Pre-Batch-6, the same table said 54 of 351 across 10 methods, of which 9
/// rows were real. **Every batch number recorded against 351 is stated against
/// a denominator this table no longer has**; see the header.
const KNOWN_DIVERGENCES: &[Known] = &[
    // -- unimplemented: five rows below cover all seventeen receivers each -----
    Known {
        method: "toLocaleString",
        receivers: ALL_RECEIVERS,
        absent: true,
        why: "Array.prototype.toLocaleString is not registered",
    },
    Known {
        method: "with",
        receivers: ALL_RECEIVERS,
        absent: true,
        why: "Array.prototype.with is not registered (ES2023 change-array-by-copy)",
    },
    Known {
        method: "toReversed",
        receivers: ALL_RECEIVERS,
        absent: true,
        why: "Array.prototype.toReversed is not registered (ES2023 change-array-by-copy)",
    },
    Known {
        method: "toSorted",
        receivers: ALL_RECEIVERS,
        absent: true,
        why: "Array.prototype.toSorted is not registered (ES2023 change-array-by-copy)",
    },
    Known {
        method: "toSpliced",
        receivers: ALL_RECEIVERS,
        absent: true,
        why: "Array.prototype.toSpliced is not registered (ES2023 change-array-by-copy)",
    },
    // -- deleted keys stay visible to Object.keys ---------------------------
    //
    // ONE defect behind these entries, which the generated table separates for
    // us: `hasOwnProperty(o, i)` says false but `Object.keys(o)` still counts
    // the index, because `index_delete`/`delete_property` hole the storage cell
    // and leave the shape descriptor behind. Every entry below is a case where
    // that makes the `Object.keys` column differ from node's — and it is the
    // ONLY field that differs on all of them.
    Known {
        method: "pop",
        receivers: &[
            "own_numeric_length",
            "own_string_length",
            "holey_object",
            "length_float",
        ],
        absent: false,
        why: "`hasOwnProperty(o, i)` is false but `Object.keys(o)` still counts the \
              deleted index: the shape descriptor survives the delete",
    },
    Known {
        method: "shift",
        receivers: &[
            "own_numeric_length",
            "own_string_length",
            "holey_object",
            "length_float",
        ],
        absent: false,
        why: "same as pop: a deleted own index stays in Object.keys",
    },
    Known {
        method: "splice",
        receivers: &[
            "own_numeric_length",
            "own_string_length",
            "holey_object",
            "length_float",
        ],
        absent: false,
        why: "same as pop/shift, and it is the ONLY remaining defect on these four \
              receivers: the hole materialisation in the removed-element array \
              this entry used to also carry is fixed (see `a_hole_is_never_\
              materialised_as_an_own_undefined`)",
    },
    // `holey_object` extends the same cluster to three more methods, because
    // deleting its index 1 is the case the deleted-key defect is worst at: the
    // index is a hole, so every reader agrees it is absent and only the key
    // count can still be wrong.
    Known {
        method: "copyWithin",
        receivers: &["holey_object"],
        absent: false,
        why: "holey_object: same deleted-key-counts defect — copyWithin deletes index 1 and \
              the shape descriptor survives",
    },
    Known {
        method: "sort",
        receivers: &["holey_object"],
        absent: false,
        why: "holey_object: same deleted-key-counts defect — sort deletes the hole at index 1",
    },
    Known {
        method: "unshift",
        receivers: &["holey_object"],
        absent: false,
        why: "holey_object: same deleted-key-counts defect — unshift deletes the hole at index 1",
    },
    Known {
        method: "length_lower",
        receivers: &["array_dense", "array_holey"],
        absent: false,
        why: "`o.length = 1` on a real array drops the element but not its own key",
    },
    // -- arguments exotic ----------------------------------------------------
    Known {
        method: "index_assign_past",
        receivers: &["arguments_exotic"],
        absent: false,
        why: "`arguments[7] = x` grows `arguments.length` to 8; the exotic's length \
              is fixed at creation and an index write must not touch it",
    },
    // -- E2: the accessor receivers, one entry per method --------------------
    //
    // `accessor_index` and `length_getter` are the two receivers with an
    // accessor property, and between them they diverge on 38 and 39 of their 39
    // rows. Every entry below is `ACCESSOR_RECEIVERS`; the two `why` texts say
    // which of E1/E2 the row carries, because they are separable and only one
    // of them is an index/length defect.
    //
    // E2 BEYOND THE KEY COUNT — the native path really does misread the receiver.
    Known {
        method: "fill",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "copyWithin",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "slice",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "reverse",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "shift",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "unshift",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "splice",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "pop",
        receivers: &["length_getter"],
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "indexOf",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "lastIndexOf",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "includes",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "at",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    Known {
        method: "flat",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E2_ACCESSOR_GETTER_UNREADABLE,
    },
    // E1 ALONE — the method's own answer agrees with node on both receivers and
    // only the harness's `Object.keys` column differs. Twenty-six rows' worth,
    // and NOT one index/length defect: this is the cost of putting an accessor
    // in a receiver and is stated here so nobody reads these as 26 method bugs.
    //
    // The seam methods (`map`, `forEach`, `filter`, `some`, `every`, `find*`,
    // `flatMap`, `reduce`, `reduceRight`) are in this group and that is the
    // point: they read `length` through `Interp::get_property`, so they get the
    // getter right and would have been the only rows in the table that could
    // have shown the split between the two readers.
    Known {
        method: "join",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "sort",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "concat",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "toString",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "push",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "flatMap",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "map",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "forEach",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "filter",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "reduce",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "reduceRight",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "some",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "every",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "find",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "findIndex",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "findLast",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "findLastIndex",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "index_assign_low",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "index_assign_mid",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "index_assign_past",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    Known {
        method: "length_lower",
        receivers: ACCESSOR_RECEIVERS,
        absent: false,
        why: E1_ACCESSOR_NON_ENUMERABLE,
    },
    // -- FROZEN: the integrity-level receiver, seven mutating rows ------------
    //
    // One cause behind all seven, and the worst finding of this batch: a
    // mutating method whose spec step requires a write to SUCCEED proceeds as if
    // it had. `push` is correct and is therefore NOT listed — it throws,
    // because adding index 3 hits the non-extensible check — so the refusal
    // machinery is reachable and it is the two steps that must *observe* it
    // (`DeletePropertyOrThrow`, `Set(O, k, v, true)`) that are missing.
    Known {
        method: "reverse",
        receivers: &["frozen_object"],
        absent: false,
        why: FROZEN_REFUSAL_UNOBSERVED,
    },
    Known {
        method: "copyWithin",
        receivers: &["frozen_object"],
        absent: false,
        why: FROZEN_REFUSAL_UNOBSERVED,
    },
    Known {
        method: "shift",
        receivers: &["frozen_object"],
        absent: false,
        why: FROZEN_REFUSAL_UNOBSERVED,
    },
    Known {
        method: "fill",
        receivers: &["frozen_object"],
        absent: false,
        why: FROZEN_REFUSAL_UNOBSERVED,
    },
    Known {
        method: "unshift",
        receivers: &["frozen_object"],
        absent: false,
        why: FROZEN_REFUSAL_UNOBSERVED,
    },
    Known {
        method: "splice",
        receivers: &["frozen_object"],
        absent: false,
        why: FROZEN_REFUSAL_UNOBSERVED,
    },
    Known {
        method: "pop",
        receivers: &["frozen_object"],
        absent: false,
        why: FROZEN_REFUSAL_UNOBSERVED,
    },
];

/// Divergences this file knows about, folded into the same shape the test
/// builds from the table, so one `assert_eq!` compares them.
///
/// **Unions** receiver sets across entries naming the same method: a method
/// that diverges on `own_numeric_length` for one reason and on
/// `length_getter` for another needs two ledger entries, and a map that let
/// the second overwrite the first would silently drop the first.
fn declared_divergences() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let mut out: BTreeMap<&'static str, BTreeSet<&'static str>> = BTreeMap::new();
    for k in KNOWN_DIVERGENCES {
        out.entry(k.method)
            .or_default()
            .extend(k.receivers.iter().copied());
    }
    out
}

/// Runs every generated program once and returns, per receiver, the rows v12
/// produced. One eval per receiver, not one per row.
fn observe() -> Vec<(&'static ReceiverTable, Vec<String>)> {
    RECEIVERS
        .iter()
        .map(|receiver: &'static ReceiverTable| {
            let observed = eval_json(receiver.program);
            let rows: Vec<String> = observed.split(ROW_SEPARATOR).map(str::to_string).collect();
            let expected_rows = receiver.expected.split(ROW_SEPARATOR).count();
            assert_eq!(
                rows.len(),
                expected_rows,
                "{}: the program produced {} observations but node's committed \
                 answer has {} — one of them contains the {ROW_SEPARATOR:?} row \
                 separator",
                receiver.name,
                rows.len(),
                expected_rows,
            );
            (receiver, rows)
        })
        .collect()
}

#[test]
fn the_generated_table_agrees_with_node_except_where_named() {
    let observed = observe();

    let mut actual: BTreeMap<&'static str, BTreeSet<&'static str>> = BTreeMap::new();
    let mut rows_by_key: BTreeMap<(&'static str, &'static str), &str> = BTreeMap::new();
    for (receiver, rows) in &observed {
        for (got, want) in rows.iter().zip(receiver.expected.split(ROW_SEPARATOR)) {
            let method = want.split('|').next().unwrap_or("");
            rows_by_key.insert((method, receiver.name), got.as_str());
            if got != want {
                actual.entry(method).or_default().insert(receiver.name);
            }
        }
    }

    // The `absent` excuses are checked, not trusted: if one of those methods
    // stops throwing, the row is no longer "not implemented" and the ledger
    // entry has to change (and the method is now a real divergence).
    for known in KNOWN_DIVERGENCES.iter().filter(|k| k.absent) {
        for receiver in ALL_RECEIVERS {
            let got = rows_by_key.get(&(known.method, receiver));
            assert!(
                got.is_some_and(|line| line.contains("threw:TypeError")),
                "{} claims `absent` on {} but v12 answered {:?}; either the method \
                 exists now or the row is a real divergence",
                known.method,
                receiver,
                got.copied().unwrap_or("<row missing>"),
            );
        }
    }

    let declared = declared_divergences();
    let total: usize = actual.values().map(BTreeSet::len).sum();
    let mut ledger = String::new();
    for known in KNOWN_DIVERGENCES {
        ledger.push_str(&format!(
            "\n  {:<18} {:>2} receiver(s)  {}{}",
            known.method,
            known.receivers.len(),
            if known.absent { "[absent] " } else { "" },
            known.why,
        ));
    }
    assert_eq!(
        actual,
        declared,
        "the divergence set moved. {} rows diverge today ({} methods); the ledger \
         names {}. A NEW divergence and a FIXED one both fail here — that is the \
         point of the ledger, so update KNOWN_DIVERGENCES in the same commit that \
         changes the behaviour.\nledger:{ledger}",
        total,
        actual.len(),
        declared.values().map(BTreeSet::len).sum::<usize>(),
    );
    assert_eq!(
        total, 177,
        "the divergence count moved without the ledger's shape changing"
    );
}

/// The same table, re-answered by a **live** `node` instead of the committed
/// literals. Ignored by default because CI may have no `node`; run it with
///
/// ```text
/// cargo nextest run -p v12-engine --run-ignored -E 'test(live_node)'
/// ```
///
/// after touching the harness in the generator, to prove the committed
/// literals are still what this `node` says.
#[test]
#[ignore = "needs `node` on PATH; the committed literals are the CI-visible oracle"]
fn the_committed_literals_still_match_a_live_node() {
    // `node -e … -` does not mean "read the program from stdin" (it means "load
    // the file named `-`"), so the program goes through a real temp file —
    // the same path the generator takes.
    let mut path = std::env::temp_dir();
    path.push(format!("v12-array-receiver-row-{}.js", std::process::id()));
    for receiver in RECEIVERS {
        if std::fs::write(&path, receiver.program).is_err() {
            panic!("cannot write {} for node", path.display());
        }
        let out = std::process::Command::new(
            std::env::var("V12_NODE").unwrap_or_else(|_| "node".to_string()),
        )
        .arg("-e")
        .arg(
            "process.stdout.write(String(require('vm').runInThisContext(\
             require('fs').readFileSync(process.argv[1],'utf8'),{filename:'row.js'})))",
        )
        .arg(&path)
        .output();
        let done = match out {
            Ok(d) => d,
            Err(e) => panic!("cannot run node: {e}"),
        };
        assert!(
            done.status.success(),
            "node failed on {}: {}",
            receiver.name,
            String::from_utf8_lossy(&done.stderr)
        );
        let live = String::from_utf8_lossy(&done.stdout).into_owned();
        assert_eq!(
            live, receiver.expected,
            "{}: the committed literals no longer match this node — \
             re-run ./scripts/gen-array-receiver-table.py",
            receiver.name
        );
    }
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// 3. The self-consistency assertion — the mutation-sensitive one, and the one
//    that needs no oracle at all.
// ---------------------------------------------------------------------------

/// After a mutating call, every index `i < ToLength(receiver)` must read back
/// through an ordinary property get exactly what the method left there.
///
/// The comparison runs **inside** the engine: it snapshots the receiver the way
/// a plain read sees it and then reads each index back through a second, fresh
/// property get, so any reader that disagrees with `ordinary_get` shows up as a
/// mismatch instead of being compared through a display string.
#[test]
fn self_consistency_over_the_whole_corpus() {
    let corpus = [
        ("{length:3}", "var r = {length: 3};"),
        (
            "own numeric length",
            r#"var r = {0:"a",1:"b",2:"c",length:3};"#,
        ),
        (
            "own string length",
            r#"var r = {0:"a",1:"b",2:"c",length:"3"};"#,
        ),
        (
            "inherited index",
            "var r = Object.create({0:'inh', length:3});",
        ),
        ("inherited length", "var r = Object.create({length:3});"),
        // A hole strictly below the visible extent. Added with the
        // `holey_object` row: the oracle-free half of this test should cover
        // every receiver shape the differential half does, or a receiver can be
        // differentially pinned and self-consistency-unpinned at the same time.
        (
            "hole below the extent",
            r#"var r = {0:"a",2:"c",length:3};"#,
        ),
    ];
    let mutators = [
        r#"Array.prototype.fill.call(r, "q", 0, 3)"#,
        r#"Array.prototype.copyWithin.call(r, 0, 1, 3)"#,
        r#"Array.prototype.reverse.call(r)"#,
        r#"Array.prototype.shift.call(r)"#,
        r#"Array.prototype.unshift.call(r, "z")"#,
        r#"Array.prototype.splice.call(r, 1, 1)"#,
        r#"Array.prototype.push.call(r, "d")"#,
        r#"Array.prototype.pop.call(r)"#,
        r#"Array.prototype.sort.call(r)"#,
    ];
    for (label, decl) in corpus {
        for m in mutators {
            let src = format!(
                r#"
                {decl}
                var thrown = null;
                try {{ {m}; }} catch (e) {{ thrown = String(e); }}
                var out = "";
                var L = r.length;
                if (typeof L !== "number" || L < 0 || L > 8) {{ L = 0; }}
                for (var n = 0; n < L; n++) {{
                    // Two independent reads of the same index. If any reader
                    // disagrees with `ordinary_get` they diverge here.
                    var x = r[n];
                    var y = r[n];
                    out += n + ":" + String(x) + "/" + String(y)
                         + (x === y ? "=" : "!") + ";";
                }}
                if (thrown !== null) {{ out += "threw:" + thrown + ";"; }}
                out;
                "#
            );
            let got = eval_json(&src);
            let known = KNOWN_HOLE_READ_ERRORS
                .iter()
                .any(|(l, mm)| *l == label && *mm == m);
            if got.contains("threw:") || got.contains("ReferenceError") {
                assert!(
                    known,
                    "{label} + {m} must not throw: {got}\n\
                     If this is the E3 hole-read defect, add the pair to \
                     KNOWN_HOLE_READ_ERRORS with its reason — do not widen the \
                     corpus's mutator list to avoid it."
                );
                continue;
            }
            assert!(
                !got.contains('!'),
                "{label} + {m}: an index reads differently twice in a row, so \
                 the method and the property path disagree ({got})"
            );
            let entries: Vec<&str> = got.split(';').filter(|e| !e.is_empty()).collect();
            assert!(
                entries.iter().all(|e| e.ends_with('=')),
                "{label} + {m}: malformed read log ({got})"
            );
        }
    }

    // The ratchet's other direction: a listed pair that has become clean is as
    // much a failure as an unlisted pair that has become broken, because the
    // list is the only place E3 is written down.
    for (label, m) in KNOWN_HOLE_READ_ERRORS {
        let decl = corpus
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, d)| *d)
            .expect("a listed pair names a receiver in the corpus above");
        let src = format!(
            r#"{decl}
               try {{ {m}; }} catch (e) {{}}
               var L = r.length;
               if (typeof L !== "number" || L < 0 || L > 8) {{ L = 0; }}
               var out = "";
               for (var n = 0; n < L; n++) {{ out += String(r[n]) + ";"; }}
               out;
               "#
        );
        let got = eval_json(&src);
        assert!(
            got.contains("threw:") || got.contains("ReferenceError"),
            "{label} + {m} is listed in KNOWN_HOLE_READ_ERRORS but reads cleanly now \
             ({got}) — E3 is fixed for this pair, so DELETE the entry"
        );
    }
}

/// **E3 — FIXED.** A `delete`d own shape-backed property keeps its descriptor
/// with a `hole` in the storage cell, and every `[[Get]]`/`[[HasProperty]]`
/// rung that read the cell straight out of `properties` handed the **hole tag**
/// to JavaScript instead of `undefined`.
///
/// `CONTEXT.md` calls this a breach of a documented invariant: "`Hole` — the
/// internal marker for an absent array element; **never observable from
/// conforming JavaScript**." The observable failure was worse than a wrong
/// value: `String(hole)` fell through to the unimplemented BigInt arm and
/// raised `InternalError: BigInt ToString is not supported yet`, naming a
/// subsystem that has nothing to do with the defect.
///
/// The cause is one rule the readers did not all implement: **a `hole` cell
/// means the property is ABSENT**, which is what ES 10.1.8 `OrdinaryGet` step 2
/// says when it consults `[[GetOwnProperty]]`. Seven rungs disagreed about it
/// — the interpreter's `get_property` read the cell raw at four of them (the
/// StubCache arm, the IC arm, the dictionary rung and the shape rung) and
/// `internal_methods`' `ordinary_get`/`ordinary_has_property` at three more.
/// `object_get_own_property` and `ordinary_own_keys` already collapsed it,
/// which is why `Reflect.ownKeys` and `JSON.stringify` were right while
/// `Object.keys`, `in` and every plain read were not.
///
/// **The list is now empty and must stay empty.** It is kept rather than
/// deleted because `self_consistency_over_the_whole_corpus` uses it in BOTH
/// directions: an unlisted pair that throws fails, and a listed pair that has
/// become clean also fails. The four pairs it used to name — every one of them
/// `(hole below the extent, <mutator>)` — are covered again by
/// [`a_deleted_property_never_hands_javascript_the_hole_sentinel`], which
/// asserts the invariant directly instead of inferring it from "did it throw".
///
/// **What is deliberately NOT fixed here:** `Object.keys`,
/// `Object.getOwnPropertyNames`, `for-in` and `in` still report a deleted own
/// index, because they read the *descriptor list* (`collect_own_string_keys`
/// and `op_in` consult liveness nowhere) rather than a cell. That is the
/// separate `Object.keys`-after-delete cluster in [`KNOWN_DIVERGENCES`] and it
/// is the next batch's work; nothing in this fix touches it.
const KNOWN_HOLE_READ_ERRORS: &[(&str, &str)] = &[];

/// The E3 invariant, stated so that reading it does not need `node`.
///
/// Every observable of "this property is absent" has to agree, and the two that
/// matter are independent: `typeof` already classified the hole as
/// `undefined` (`Interp::type_tag` has a defensive arm for it) while
/// `=== undefined` did **not**, so a test on `typeof` alone passes with the
/// sentinel still in place. `String(...)` is the arm that raised. All four
/// observables are asserted on both sides of a `delete` — before, where the
/// index is present, and after, where it is absent — so the assertions cannot
/// pass vacuously on a receiver that never had the property.
#[test]
fn a_deleted_property_never_hands_javascript_the_hole_sentinel() {
    // `key` is a JS *expression* (a quoted string), not a bare name, so the
    // same template works for `"a"` and for `"0"`.
    //
    // `before` is the CONTROL read: the same expressions against the same
    // receiver BEFORE the `delete`. A harness that cannot read the key at all
    // fails here instead of passing the absent case vacuously.
    for (label, decl, key, before) in [
        (
            "own index, then deleted",
            r#"var o = {0:"a",1:"b",length:2};"#,
            r#""0""#,
            r#"["string",true,true,"a"]"#,
        ),
        (
            "own string key, then deleted",
            r#"var o = {a:"x",b:"y"};"#,
            r#""a""#,
            r#"["string",true,true,"x"]"#,
        ),
        (
            // Never had a descriptor at all — a hole by construction. `before`
            // is already the absent answer, so this case controls the control.
            "index that was never present",
            r#"var o = {0:"a",2:"c",length:3};"#,
            r#""1""#,
            r#"["undefined",false,false,"undefined"]"#,
        ),
    ] {
        let absent = r#"["undefined",false,false,"undefined"]"#;
        assert_is(
            &eval_json(&format!(
                r#"{decl}
                   JSON.stringify([typeof o[{key}], {key} in o,
                                   Object.prototype.hasOwnProperty.call(o, {key}),
                                   String(o[{key}])]);"#
            )),
            before,
            &format!("{label}: the control read, before the delete"),
        );
        // `delete` must make it absent on EVERY observable.
        assert_is(
            &eval_json(&format!(
                r#"{decl}
                   delete o[{key}];
                   JSON.stringify([typeof o[{key}], {key} in o,
                                   Object.prototype.hasOwnProperty.call(o, {key}),
                                   String(o[{key}])]);"#
            )),
            absent,
            &format!("{label}: a deleted property reads as absent, not as an internal tag"),
        );
    }

    // The other half of the same rule, and the one a "collapse to `undefined`"
    // fix gets WRONG: ES 10.1.8 step 3 says a deleted own property is answered
    // by the PROTOTYPE, not by `undefined`. So the fix cannot be "return
    // `undefined` when the cell is a hole" — that is what a first attempt did,
    // and it turned `{a:"inh"}`-backed receivers that had an own `a` deleted
    // into objects reporting `undefined` where node reports the inherited
    // value. This is the observable that distinguishes the two.
    assert_is(
        &eval_json(
            r#"var p = {a:"inh", 0:"inh0"};
               var o = Object.create(p);
               o.a = "own"; o[0] = "own0";
               delete o.a; delete o[0];
               JSON.stringify(["a" in o, 0 in o, o.a, o[0]]);"#,
        ),
        r#"[true,true,"inh","inh0"]"#,
        "a deleted own property is answered by the prototype, not by `undefined`",
    );

    // The same continue-the-walk rule on the path that decides PRESENCE, which
    // is a different reader again (`dispatch_has`, not `dispatch_get`) and was
    // a second survivor until this case existed: with no receiver in the
    // generated table having a *deleted own* index that the prototype
    // re-supplies, answering `false` instead of continuing produced no
    // divergence anywhere. `indexOf` needs the presence answer before the value
    // answer, so it is the probe that sees `ordinary_has_property`.
    assert_is(
        &eval_json(
            r#"var p = {0:"inh0", length:1};
               var o = Object.create(p);
               o[0] = "own0";
               delete o[0];
               JSON.stringify([o[0], Array.prototype.indexOf.call(o, "own0"),
                               Array.prototype.indexOf.call(o, "inh0"),
                               Array.prototype.at.call(o, 0)]);"#,
        ),
        r#"["inh0",-1,0,"inh0"]"#,
        "`dispatch_has` continues to the prototype for a deleted own index too",
    );

    // …and the same rule through a **cold** key, which is the only shape that
    // reaches `ordinary_has_property`'s walk rungs at all. The StubCache rung is
    // consulted first and a key that was read while it was still own is already
    // recorded there, so the walk is skipped and mutating it is invisible — a
    // third survivor, found by measuring rather than by reasoning. Here the
    // index is written and deleted without ever being read, so nothing is
    // recorded, and the walk has to answer for itself.
    //
    // It also shows the StubCache being *poisoned* by the mutation: with the
    // walk answering `false`, the second call is served from the stub instead,
    // so the two rows disagree with each other rather than both being uniformly
    // wrong. A single-row assertion would not have shown that.
    assert_is(
        &eval_json(
            r#"var p = {length:1};
               var o = Object.create(p);
               o[0] = "own0";
               delete o[0];
               p[0] = "inh0";
               JSON.stringify([Array.prototype.indexOf.call(o, "own0"),
                               Array.prototype.indexOf.call(o, "inh0"),
                               Array.prototype.at.call(o, 0)]);"#,
        ),
        r#"[-1,0,"inh0"]"#,
        "`dispatch_has`'s walk continues to the prototype on a cold deleted key",
    );

    // The reader that raised the internal error, on its own: `String(o[0])`
    // after `delete o[0]`. Named separately because it is the observable that
    // produced an `InternalError` naming BigInt, and a fix that special-cased
    // `to_string` instead of collapsing the cell at the storage boundary would
    // pass the four observables above and fail this.
    assert_is(
        &eval_json(
            r#"var o = {0:"a",1:"b",length:2};
               delete o[0];
               String(o[0]);"#,
        ),
        "undefined",
        "String() of a deleted property is `undefined`, not an internal error",
    );

    // …and the SEAM/NATIVE SPLIT: the *same* key read through the
    // interpreter's own `[[Get]]` and through `internal_methods::dispatch_get`
    // — two separate implementations of the same internal method, in two
    // crates. A fix applied to only one of them passes everything above.
    //
    // `Reflect.get` is the probe, and choosing it is the whole content of this
    // assertion — three plausible probes were measured to be BLIND to
    // `internal_methods::ordinary_get`, and the reasons generalise:
    //
    //   * `indexOf`/`includes`/`at`/`slice` ask `dispatch_has` first, so for an
    //     index already decided absent they never reach the cell read at all;
    //     and `at` cannot tell a hole from `undefined` in its return value.
    //   * `Object.values`/`Object.entries` DO call `dispatch_get` per key, but
    //     the result lands in an **element store**, whose reader filters holes
    //     (`ElementsStorage::get` → `.filter(|v| !v.is_hole())`). The sentinel
    //     is swallowed one step later.
    //   * `JSON.stringify` collapses holes explicitly (`json.rs`).
    //
    // `Reflect.get` hands the `[[Get]]` result straight back to JavaScript, so
    // it is the only one of the four that can see the cell read at all. With
    // `ordinary_get` mutated back to serving `properties` raw, this line is the
    // assertion that fails.
    assert_is(
        &eval_json(
            r#"var o = {0:"a",1:"b",length:2};
               delete o[0];
               JSON.stringify([o[0], Reflect.get(o, "0"),
                               Reflect.get(o, "0") === undefined,
                               Array.prototype.indexOf.call(o, "a"),
                               Array.prototype.includes.call(o, "a"),
                               Array.prototype.join.call(o, "-")]);"#,
        ),
        r#"[null,null,true,-1,false,"-b"]"#,
        "the interpreter's [[Get]] and the engine's `dispatch_get` agree on a deleted index",
    );
}

// ---------------------------------------------------------------------------
// 4. `ArraySetLength`: the defect this batch fixed, pinned directly.
// ---------------------------------------------------------------------------

/// ES `ArraySetLength` step 2.e: an index write sets `length` to
/// `max(oldLength, index + 1)`. `Interp::array_set_element`
/// (`v12-interp/src/property.rs`) derived the new value from the
/// element-store EXTENT instead, so on a sparse receiver — where the store is
/// below the declared length — writing one index *lowered* `length` to the
/// extent. `new Array(3)` followed by `a[0] = "z"` reported `length` 1.
///
/// The generated table's `index_assign_low` / `index_assign_mid` rows are the
/// differential form; this is the same invariant stated so that reading it does
/// not require running `node`.
#[test]
fn an_index_write_never_lowers_a_declared_length() {
    for (what, src) in [
        (
            "new Array(3), write index 0",
            r#"var a = new Array(3); a[0] = "z"; JSON.stringify([a.length, a[0]])"#,
        ),
        (
            "new Array(3), write index 1",
            r#"var a = new Array(3); a[1] = "z"; JSON.stringify([a.length, a[0], a[1]])"#,
        ),
        (
            "new Array(10), write index 0",
            r#"var a = new Array(10); a[0] = "z"; JSON.stringify([a.length, a[0]])"#,
        ),
        (
            "assigned length 10, write index 3",
            r#"var a = []; a.length = 10; a[3] = "z"; JSON.stringify([a.length, a[3]])"#,
        ),
        (
            "a dense write still raises length",
            r#"var a = ["a"]; a[3] = "d"; JSON.stringify([a.length, a[3]])"#,
        ),
        (
            "a write past the end raises it to index + 1",
            r#"var a = new Array(3); a[4] = "z"; JSON.stringify([a.length, a[4]])"#,
        ),
        (
            "a second write does not move it",
            r#"var a = new Array(3); a[0] = "z"; a[0] = "y"; JSON.stringify([a.length, a[0]])"#,
        ),
    ] {
        // The expectation is computed from the declared length, never copied:
        // `ArraySetLength` is `max(old, i+1)`, so that is what is asserted.
        let (declared, index): (f64, f64) = match what {
            "new Array(3), write index 0" => (3.0, 0.0),
            "new Array(3), write index 1" => (3.0, 1.0),
            "new Array(10), write index 0" => (10.0, 0.0),
            "assigned length 10, write index 3" => (10.0, 3.0),
            "a dense write still raises length" => (1.0, 3.0),
            "a write past the end raises it to index + 1" => (3.0, 4.0),
            "a second write does not move it" => (3.0, 0.0),
            other => unreachable!("unlisted case {other}"),
        };
        let observed = eval_json(src);
        let json = observed.trim_start_matches('[');
        let want = declared.max(index + 1.0);
        assert!(
            json.starts_with(&format!("{want},")),
            "{what}: length is {observed}, want {} (= max({declared}, {} + 1))",
            want,
            index
        );
    }
}

// ---------------------------------------------------------------------------
// 5. The hole invariant, stated so reading it does not need `node`.
// ---------------------------------------------------------------------------

/// The receiver used by every case below: a hole at index 1, present elements
/// at 0 and 2. `{0:"a", 2:"c", length:3}` is the smallest object on which a
/// hole and a present `undefined` are both expressible, so a method that
/// confuses them cannot pass by accident.
const HOLEY_DECL: &str = r#"var r = {0:"a", 2:"c", length:3};"#;

/// Every own index of `r`, as a `-`-for-hole string, read through the spec's
/// `HasProperty` (`i in r`) rather than through a value comparison. `in` is
/// the only thing that can tell a hole from a present `undefined`.
const OWN_INDEXES: &str = r#"
    var s = [];
    for (var i = 0; i < r.length; i++) {
        s.push(Object.prototype.hasOwnProperty.call(r, i) ? String(r[i]) : "-");
    }
    s.join(",");
"#;

/// The same read on an arbitrary array, for the methods whose *return value* is
/// the thing under test.
const INDEXES_OF: &str = r#"
    (function (a) {
        var s = [];
        for (var i = 0; i < a.length; i++) {
            s.push(i in a ? String(a[i]) : "-");
        }
        return s.join(",");
    })(RESULT)
"#;

/// Six methods used to report a hole as a present own `undefined`. Which of
/// them can *observe* the difference, and how:
///
/// * `slice`, `concat`, `map` and `splice`'s removed array RETURN a fresh
///   array, and `i in result` is false at every index the source did not have.
///   That is the whole assertion: a materialised `undefined` reads `"undefined"`
///   where a hole reads `"-"`.
/// * `sort` and `reverse` RETURN THE RECEIVER, so their return value cannot
///   distinguish anything — the observable is `Object.keys(receiver)`, i.e.
///   whether the method created own properties at indices that were holes.
///   A test written on their return value could not fail, so none is.
///
/// The expected strings are derived from the spec's own rule (a hole in, a hole
/// out) rather than copied from `node`; `node` agrees with all of them
/// (verified in the batch's own `node` diff, which also showed the only
/// remaining disagreement on these receivers is the separate
/// `Object.keys`-after-delete defect listed under `pop`).
#[test]
fn a_hole_is_never_materialised_as_an_own_undefined() {
    // `slice`: one hole in the receiver, one hole in the result.
    assert_is(
        &eval_json(&format!(
            r#"{HOLEY_DECL}
               Array.prototype.slice.call(r);
               {INDEXES_OF}"#,
            INDEXES_OF = INDEXES_OF.replace("RESULT", "Array.prototype.slice.call(r)"),
        )),
        "a,-,c",
        "slice leaves the receiver's hole absent in the result",
    );
    // `concat`: the receiver is an Array, so it IS spread — and its hole must
    // survive the spread into the same relative position.
    assert_is(
        &eval_json(&format!(
            r#"var r = (function () {{ var x = ["a"]; x[2] = "c"; x.length = 3; return x; }})();
               {INDEXES_OF}"#,
            INDEXES_OF = INDEXES_OF.replace("RESULT", "Array.prototype.concat.call(r)"),
        )),
        "a,-,c",
        "concat spreads a hole as a hole",
    );
    // `map`: the seam's result is pre-sized to the receiver's length, so the
    // pre-size is the only thing that can go wrong here.
    assert_is(
        &eval_json(&format!(
            r#"{HOLEY_DECL}
               var m = Array.prototype.map.call(r, function (v) {{ return v; }});
               var s = [];
               for (var i = 0; i < m.length; i++) {{ s.push(i in m ? String(m[i]) : "-"); }}
               s.join(",");"#
        )),
        "a,-,c",
        "map's pre-sized result is holey, not filled with `undefined`",
    );
    // `splice`: the REMOVED array is the result under test. Deleting all three
    // indices of a 3-element receiver returns a length-3 array whose entries
    // are the receiver's own — `a`, hole, `c`.
    assert_is(
        &eval_json(&format!(
            r#"{HOLEY_DECL}
               var out = Array.prototype.splice.call(r, 0, 3);
               var s = [];
               for (var i = 0; i < out.length; i++) {{ s.push(i in out ? String(out[i]) : "-"); }}
               s.join(",");"#
        )),
        "a,-,c",
        "splice's removed array keeps the hole it removed",
    );
    // `sort` and `reverse` return the receiver, so their return value cannot
    // distinguish a hole from a present `undefined` — the observable is
    // `hasOwnProperty` on the receiver afterwards. Neither may leave an own
    // property holding `undefined` at an index it did not have to write.
    //
    // `Object.keys` would be the natural next check and is deliberately NOT
    // used here: on these two receivers it still counts a deleted own index,
    // which is the separate `pop`/`shift` defect in `KNOWN_DIVERGENCES`.
    for (what, call, want) in [
        // `sort` packs the two present values into `0,1` and deletes the tail,
        // so index 2 ends up a hole rather than a present `undefined`.
        ("sort", r#"Array.prototype.sort.call(r)"#, "a,c,-"),
        // `reverse` permutes presence: `"a"` lands at 2, `"c"` at 0, and the
        // hole at 1 stays a hole.
        ("reverse", r#"Array.prototype.reverse.call(r)"#, "c,-,a"),
    ] {
        assert_is(
            &eval_json(&format!(
                r#"{HOLEY_DECL}
                   {call};
                   {OWN_INDEXES}"#
            )),
            want,
            &format!("{what} must not leave a present `undefined` where the receiver had a hole"),
        );
    }
}

/// The flip side, and the reason this fix could not have been a blanket
/// "never write `undefined`": `Array.from` has NO hole semantics. Its spec step
/// is an unconditional `CreateDataProperty(O, k, Get(next, k))` for every
/// `k < len`, so it must MATERIALISE. `slice(new Array(3))` is three holes and
/// `Array.from(new Array(3))` is three own `undefined`s — and this test fails if
/// a reader that is shared between them stops flattening.
#[test]
fn array_from_still_materialises_where_slice_does_not() {
    assert_is(
        &eval_json(
            r#"
            function own(a) {
                var s = [];
                for (var i = 0; i < a.length; i++) { s.push(i in a ? String(a[i]) : "-"); }
                return s.join(",");
            }
            JSON.stringify([
                own(Array.prototype.slice.call(new Array(3))),
                own(Array.from(new Array(3))),
                Object.keys(Array.from(new Array(3))).length,
                Object.keys(Array.prototype.slice.call(new Array(3))).length,
            ]);
            "#,
        ),
        r#"["-,-,-","undefined,undefined,undefined",3,0]"#,
        "Array.from materialises holes; slice does not",
    );
}

// ---------------------------------------------------------------------------
// 6. The find family: direction, and holes as ordinary `undefined`.
// ---------------------------------------------------------------------------

/// ES 22.1.3.15 `Array.prototype.findLast` and 22.1.3.14 `findLastIndex`
/// iterate `k` **downward** from `len-1` to `0` and return at the *first*
/// match; `find`/`findIndex` iterate upward. Both halves were wrong together at
/// the callback seam: the loop walked `0..len` for all four, and only
/// `find`/`findIndex` broke out of it — so `findLast` kept the LAST FORWARD
/// match, which is the highest index, and therefore returned the right answer on
/// a dense receiver by accident.
///
/// That accident is why this test exists in two halves. The return value alone
/// cannot see the defect (`x === "c"` matches one index, so forward and backward
/// agree), so the always-true predicate is used to force the whole range to be
/// walked, and the **visit order** is asserted alongside the result.
#[test]
fn find_last_walks_backward_and_stops_at_the_first_match() {
    const DENSE: &str = r#"var o = {0:"a",1:"b",2:"c",length:3};"#;
    let len = 3;

    // Two predicates, because they observe different halves of the invariant:
    // an ALWAYS-TRUE predicate stops at the first index visited, so it reports
    // only WHICH index the walk starts at — which is the direction, and is the
    // half that is invisible in the return value on a dense receiver. An
    // ALWAYS-FALSE predicate never returns early, so it reports the WHOLE walk
    // in order. Neither alone can see a walk that starts at the right index and
    // then gets the direction of the remainder wrong.

    for method in ["findLast", "findLastIndex"] {
        let is_index = method.ends_with("Index");
        // Always true ⇒ the first index visited is `len-1` and the answer is it.
        // A forward walk visits `0` and answers `0` / `"a"`.
        assert_is(
            &probe(DENSE, method, "true"),
            &format!(
                r#"["{want}","{first}"]"#,
                want = if is_index { "2" } else { "c" },
                first = first_index(method, len)
            ),
            &format!("{method} starts at the LAST index"),
        );
        // Always false ⇒ the whole range, in the method's direction.
        assert_is(
            &probe(DENSE, method, "false"),
            &format!(
                r#"["{want}","{order}"]"#,
                want = if is_index { "-1" } else { "undefined" },
                order = walk_order(method, len)
            ),
            &format!("{method} visits all {len} indices in its own direction"),
        );
    }

    // Only the LAST index matches: backward and forward agree on the value, so
    // this half is the guard that the fix did not change *which* match wins —
    // only the direction of the search for it.
    assert_is(
        &probe(DENSE, "findLast", r#"x === "c""#),
        r#"["c","2"]"#,
        "findLast returns the last match when only the last index matches",
    );

    // Only the FIRST index matches. The result is `"a"` either way — a forward
    // walk with the old fall-through overwrite also returned `"a"` — so the
    // visit order is the entire difference, and it is descending.
    assert_is(
        &probe(DENSE, "findLast", r#"x === "a""#),
        r#"["a","2:1:0"]"#,
        "findLast visits 2,1,0 when only index 0 matches",
    );
    // The forward half, unchanged by this batch: `find` stops at index 0 and
    // never looks at 1 or 2.
    assert_is(
        &probe(DENSE, "find", r#"x === "a""#),
        r#"["a","0"]"#,
        "find still stops at the first forward match",
    );
}

/// The find family is the ONLY callback method with no `HasProperty` step in its
/// spec step: all four run the predicate once per index in `[0, len)` and pass
/// `undefined` where the index is a hole. `forEach`/`map`/`filter`/`every`/
/// `some`/`reduce`/`reduceRight` are all guarded by "if `k` is present" and skip
/// it — so this is a per-method property, not a property of the seam, and the
/// fix that adds it must not leak to the seven.
///
/// `node` v24.21.0, verified by running it, on the two holey receivers below:
/// `findLastIndex` over `[a,_,_,d,_]` is `4` with the predicate seeing
/// `undefined` at index 4, and over `Object.create({length:3})` it is `2`.
#[test]
fn the_find_family_visits_holes_and_nothing_else_does() {
    // A hole at the TOP of the range is the case that cannot pass by accident:
    // a hole-skipping walk finds the next PRESENT element instead, and a
    // hole-VISITING walk that also assumed "holes are absent" reports the same
    // thing — only "a hole is `undefined`" returns the hole's own index.
    //
    // `top` is `String(Get(o, len-1))`: what the predicate sees at the index the
    // backward walk starts on. Stated per receiver because it IS the property
    // under test.
    const HOLEY_ARRAY: &str =
        r#"var o = (function () { var x = ["a"]; x[3] = "d"; x.length = 5; return x; })();"#;
    // The smallest receiver reaching a hole strictly below its visible extent:
    // `{0:"a", 2:"c", length:3}` has extent 2 and a hole at 1. The generated
    // table has no such receiver (see the header's NAMED GAP), which is why
    // this is a focused test rather than a row.
    const HOLEY_OBJECT: &str = r#"var o = {0:"a",2:"c",length:3};"#;
    const ALL_HOLES: &str = r#"var o = Object.create({length:3});"#;

    for (label, decl, len, top) in [
        ("holey array, hole at 4", HOLEY_ARRAY, 5, "undefined"),
        ("holey object, hole at 1", HOLEY_OBJECT, 3, "c"),
        ("all holes", ALL_HOLES, 3, "undefined"),
    ] {
        for method in ["findLast", "findLastIndex"] {
            let is_index = method.ends_with("Index");
            assert_is(
                &probe(decl, method, "true"),
                &format!(
                    r#"["{want}","{first}"]"#,
                    want = if is_index {
                        (len - 1).to_string()
                    } else {
                        top.to_string()
                    },
                    first = first_index(method, len)
                ),
                &format!(
                    "{method} on {label} starts at index {}, hole or not",
                    len - 1
                ),
            );
            // Always false is the only way to see the FULL range, and so the
            // only way to prove EVERY hole was visited rather than just the one
            // that happened to be visited first.
            assert_is(
                &probe(decl, method, "false"),
                &format!(
                    r#"["{want}","{order}"]"#,
                    want = if is_index { "-1" } else { "undefined" },
                    order = walk_order(method, len)
                ),
                &format!("{method} on {label} visits all {len} indices"),
            );
        }
        // `find`/`findIndex` upward: index 0 is the first visited, so the hole
        // at the top is never reached — this asserts the *forward* half of the
        // invariant and that the two directions really are different code.
        assert_is(
            &probe(decl, "findIndex", "true"),
            r#"["0","0"]"#,
            &format!("findIndex on {label} stops at index 0"),
        );
        // …and the full forward range, so the hole-visiting invariant is
        // asserted in the forward direction too rather than only backward.
        assert_is(
            &probe(decl, "find", "false"),
            &format!(
                r#"["undefined","{order}"]"#,
                order = walk_order("find", len)
            ),
            &format!("find on {label} visits all {len} indices"),
        );
    }

    // The guard: the seven methods that DO skip holes must still skip them. If
    // the hole-visit ever becomes a property of the seam rather than of the
    // find family, this is what fails.
    //
    // The index is the THIRD argument for `reduce`/`reduceRight`
    // (`accumulator, value, index`) and the SECOND for the rest
    // (`value, index, array`) — the same arity split
    // `the_reduce_callback_receives_the_index` pins, and getting it wrong here
    // would silently record the receiver instead of the index.
    for (method, is_reduce, want) in [
        ("forEach", false, "0:3"),
        ("map", false, "0:3"),
        ("filter", false, "0:3"),
        ("every", false, "0:3"),
        // `some` returns at the first match, so it visits only index 0 — the
        // hole at 4 is never reached either way, which is the point.
        ("some", false, "0"),
        ("reduce", true, "0:3"),
        ("reduceRight", true, "3:0"),
    ] {
        let extra = if is_reduce { r#","seed""# } else { "" };
        let idx = if is_reduce { "p2" } else { "p1" };
        let got = eval_json(&format!(
            r#"
            var o = (function () {{ var x = ["a"]; x[3] = "d"; x.length = 5; return x; }})();
            var vis = [];
            var f = function (p0, p1, p2) {{ vis.push({idx}); return true; }};
            Array.prototype.{method}.call(o, f{extra});
            vis.join(":");
            "#
        ));
        assert_is(&got, want, &format!("{method} still skips holes"));
    }
}

/// A hole is `undefined` to the predicate, but a **present** `undefined` is a
/// different thing, and the find family is the only place the difference is
/// observable at all: it is the only callback method that visits holes, so only
/// it can be handed an index the receiver does not have.
///
/// The predicate here closes over the receiver and asks `!(i in o)` — "is this
/// index a hole?" — which is the only way to tell the two apart from inside the
/// callback. `typeof x === "undefined"` cannot: `node` answers the same for both
/// receivers with that predicate (verified by running it), because a hole is
/// *passed* as `undefined` and the callee never learns it was absent. An
/// implementation that visited holes by testing `x === undefined` would pass
/// that weaker predicate and fail this one.
#[test]
fn a_present_undefined_is_not_a_hole_for_the_find_family() {
    // A hole at 1: the predicate matches at index 1 and the walk stops.
    const HOLEY: &str = r#"var o = {0:"a",2:"c",length:3};"#;
    // A PRESENT `undefined` at 1: no index is a hole, so the predicate never
    // matches and the whole range is walked.
    const DENSE: &str = r#"var o = {0:"a",1:undefined,2:"c",length:3};"#;
    let is_hole = "!(i in o)";

    for (method, stop_at_hole) in [("find", "0:1"), ("findLast", "2:1")] {
        assert_is(
            &probe(HOLEY, method, is_hole),
            &format!(r#"["undefined","{stop_at_hole}"]"#),
            &format!("{method} visits the hole at 1, so a hole-detecting predicate stops there"),
        );
        assert_is(
            &probe(DENSE, method, is_hole),
            &format!(r#"["undefined","{order}"]"#, order = walk_order(method, 3)),
            &format!("{method} finds no hole in {{0:a,1:undefined,2:c}}, so it walks all 3"),
        );
    }
}

// ---------------------------------------------------------------------------
// 7. The declared deviations, pinned by name.
// ---------------------------------------------------------------------------

/// `MAX_WRITE_SPAN` is a permanent deviation: `fill`/`copyWithin` will not
/// materialize billions of entries. Pinned because the silent narrowing it
/// replaced (`dense_bound`) is what made `fill`'s range *wrong* rather than
/// merely capped, and without this test someone will "fix" the cap away.
#[test]
fn max_write_span_is_a_declared_deviation() {
    // No `length` anywhere on the chain: `LengthOfArrayLike` is 0, so neither
    // `fill` nor `copyWithin` writes anything, and the receiver gains no own
    // property. At `b0c3353e` `fill` wrote a hole into the flat `elements`
    // store here — a property the receiver never had and never reported.
    assert_is(
        &eval_json(
            r#"
            var o = {};
            Array.prototype.fill.call(o, "q");
            Array.prototype.fill.call(o, "q", 0, 10);
            Array.prototype.copyWithin.call(o, 0, 1);
            JSON.stringify([o.length === undefined, Object.keys(o).length]);
            "#,
        ),
        r#"[true,0]"#,
        "a receiver with no `length` is length 0, not an implicit store",
    );
}

#[test]
fn a_huge_length_stays_a_bounded_object() {
    // `length` near 2^32 must not become a 2^32-entry store, and the last
    // valid array index (2^32-2) is the highest one that can be written:
    // `4294967295` is a plain string key, which is why exactly one own
    // property appears. Both `node` and v12 report `[4294967295, 1]`.
    //
    // The expensive half is deliberately NOT exercised here: filling
    // `{length: 4294967295}` from 0 writes `MAX_WRITE_SPAN` (1e6) own
    // properties and costs ~6 s, because a generic receiver has no
    // representation for an index other than one property per index. That cost
    // IS the deviation, and a unit test that takes 6 s would hide it rather
    // than pin it.
    assert_is(
        &eval_json(
            r#"
            var o = {length: 4294967295};
            Array.prototype.fill.call(o, "q", 4294967295, 4294967296);
            Array.prototype.copyWithin.call(o, 4294967295, 4294967296);
            JSON.stringify([o.length, Object.keys(o).length]);
            "#,
        ),
        r#"[4294967295,1]"#,
        "a 2^32 length stays a bounded object",
    );
}

/// `MAX_CALLBACK_SCAN` is a declared deviation, like `MAX_SCAN_SPAN` in
/// `builtins/array.rs`: `ToLength` can be 2^53-1 and a generic receiver has no
/// enumerable store, so the walk is capped.
#[test]
fn max_callback_scan_is_a_declared_deviation() {
    assert_is(
        &eval_json(
            r#"
            var o = {};
            var n = 0;
            Array.prototype.forEach.call(o, function(){ n++; });
            Array.prototype.some.call(o, function(){ return true; });
            JSON.stringify([o.length === undefined, n]);
            "#,
        ),
        r#"[true,0]"#,
        "a receiver with no `length` is length 0 to the callback seam too",
    );
}

/// `reduce`'s callback takes **four** arguments — `(accumulator, value, index,
/// array)`. The seam passed three, so a `reduce` callback read the receiver
/// where the index belongs. The expectations are the ARITY and the index
/// ORDER, computed here rather than transcribed, so this cannot drift into
/// asserting node's output for a seam that no longer exists.
#[test]
fn the_reduce_callback_receives_the_index() {
    assert_is(
        &eval_json(
            r#"
            var arity = 0; var v = [];
            Array.prototype.reduce.call(["a","b","c"], function(acc, c, i, arr) {
                arity = arguments.length; v.push(i);
                return acc;
            }, "");
            JSON.stringify([arity, v.join(","), v.join(",") === [0,1,2].join(",")]);
            "#,
        ),
        r#"[4,"0,1,2",true]"#,
        "reduce's callback gets (acc, value, index, array)",
    );
    assert_is(
        &eval_json(
            r#"
            var v = [];
            Array.prototype.reduceRight.call(["a","b","c"], function(acc, c, i) {
                v.push(i); return acc;
            }, "");
            v.join(",");
            "#,
        ),
        &[2, 1, 0]
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(","),
        "reduceRight visits the indices in reverse",
    );
    assert_is(
        &eval_json(
            r#"
            var seen = 0;
            Array.prototype.reduce.call(["a"], function(acc, c, i, arr) {
                seen = arr.length; return acc;
            }, "");
            String(seen);
            "#,
        ),
        "1",
        "the fourth argument is the receiver",
    );
}

/// The Batch-1 `array_layout_len`/`write_array_layout_len` workaround existed
/// only because `Heap::add_property` handed out slot 0 while the write site
/// pushed at index 1 for a `JsObject::array`-born array. The cause is fixed in
/// `internal_methods::store_shape_extension`, so the length must now be an
/// ordinary property: writing it through `[[Set]]` and reading it back through
/// a plain `o.length` has to agree, on an array built by the raw heap
/// constructor. The `JsObject::array` + `heap.alloc` shape is the unit test in
/// `crates/v12-engine/src/tests.rs::builtin_tests::array_push_and_pop_length`,
/// which is where the defect was originally observed.
#[test]
fn a_heap_built_array_length_is_an_ordinary_property() {
    assert_is(
        &eval_json(
            r#"
            var a = ["a", "b"];
            Array.prototype.push.call(a, "c");
            Array.prototype.pop.call(a);
            JSON.stringify([a.length, a[0], a[1], a[2]]);
            "#,
        ),
        r#"[2,"a","b",null]"#,
        "push then pop on a real array",
    );
}
