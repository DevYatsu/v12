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
//! Measured at this commit: **107 of 663 rows diverge, across 20 of the 39
//! methods** (17 receivers × 39 rows). 85 of those are five unimplemented
//! methods on all seventeen receivers and say nothing about the index/length
//! model. The rest is two clusters, itemised at [`KNOWN_DIVERGENCES`]: E2, the
//! native path's inability to invoke a bytecode getter (19 rows), and five
//! rows that are neither E1 nor E2 — two `length_lower` rows on real arrays
//! and the arguments-exotic `index_assign_past` row.
//!
//! **Batch 9 fixed E1 — the last of the three defects this file found — and it
//! is fixed in the bytecode, which is why the count fell by 49 rather than
//! 25.** E1 was that an accessor written in an *object literal* is created
//! **non-enumerable** (`Object.keys({0:"a", get length(){…}})` answered `["0"]`
//! where node answers `["0","length"]`), because `DefineAccessor` carried no
//! attribute operand and its interpreter arm hardcoded the class spelling's
//! `Attrs::BUILTIN`. It is now `DefineAccessorEnumerable = 79`, emitted only
//! by the literal lowering, and the class spelling still emits
//! `DefineAccessor = 66` — verified by a byte-identical disassembly of a class
//! program before and after the change, not only by the answers agreeing with
//! node. See [`E1_ACCESSOR_NON_ENUMERABLE`] and ADR-0005.
//!
//! E2 is untouched and remains 19 rows on the two accessor receivers. The two
//! E2 entries that had claimed `accessor_index` as well as `length_getter`
//! (`fill`, `splice`, `indexOf`, `lastIndexOf`, `includes`, `at`) were narrowed:
//! `accessor_index`'s accessor sits at an *index* key, which the native path
//! reads from the property store rather than through `[[Get]]`, so E2 never
//! applied there and E1's removal exposed the difference.
//!
//! # `frozen_object` — the integrity-level rung (found in Batch 7, FIXED in Batch 8)
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
//! `push` was correct, so the refusal machinery was reachable and it was the
//! observation of it that was missing.
//!
//! **Batch 8 fixed all seven rows.** The rule was never missing — the
//! interpreter's own `[[Delete]]` refused correctly and left the receiver
//! bit-identical all along; `builtins/array.rs` was reaching the operation
//! through a **second implementation** with no descriptor check and no failure
//! channel, and was dropping `[[Set]]`'s refusal boolean on the floor. Adding
//! `internal_methods::dispatch_delete` (the table slot had existed since the
//! beginning, with `Reflect.deleteProperty` as its only caller) and routing
//! through it fixed it. See [`FROZEN_REFUSAL_UNOBSERVED`] and
//! [`a_refused_delete_leaves_the_receiver_uncorrupted`].
//!
//! What Batch 8 could **not** fix, and says so rather than papering over: a
//! frozen real ARRAY's index delete is still unreffusable, because v12 models no
//! per-index attributes for `Kind::Array` at all. That is a `v12-heap` gap; see
//! [`a_frozen_array_index_delete_is_a_declared_deviation`].
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
//! | batch 7 | 17 × 663 | 177 | 39 |
//! | batch 8 | 17 × 663 | 156 | 39 |
//! | **batch 9 (this one)** | **17 × 663** | **107** | **20** |
//!
//! The 24 → 108 step is the one that matters: it is what a stronger instrument
//! found when a transcribed table was replaced by a generated one. The
//! 108 → 165 step is the same thing one level further out — seven more spec
//! decision points — and 165 → 177 is the same thing again: ONE receiver
//! (`frozen_object`) added twelve rows, five of them the unimplemented methods
//! re-counted on the new receiver and seven of them a defect no earlier
//! receiver could reach. In both steps **no row an earlier batch claimed fixed
//! has come back**
//!
//! 177 → **156** is the first DOWNWARD step, and it is the first one where a
//! batch fixed two defects that had been found by *different* batches and
//! measured in the *same* instrument. Batch 8 removed 22 rows and **added one**:
//! `pop` on `accessor_index`, where the deleted-key bug's `+1` and E1's `−1` had
//! been cancelling to zero for as long as the row existed. See
//! [`E1_UNMASKED_BY_BATCH_8`] — it is the most transferable thing in this
//! batch.
//!
//! **In both prior up-steps no row an earlier batch claimed fixed has come back**
//! (`slice`/`map`/`concat`/`sort`/`reverse`/`splice` are still clean on every
//! receiver without an accessor, and the six "hole materialised as an own
//! `undefined`" rows stay fixed on `holey_object`, which is a holey receiver no
//! earlier batch had).
//!
//! 156 → **107** is the largest single drop in the table's history, and the
//! per-receiver accounting is the part worth keeping: on `accessor_index` 39
//! divergent rows → 11, on `length_getter` 39 → 18, and on the fifteen
//! receivers with no accessor the count is **unchanged at 78** — which is the
//! floor this file can no longer argue down without fixing E2. That is also
//! the check on the claim that the fix was surgical: a change to how
//! accessors are created that moved a row on a receiver with no accessor would
//! have shown up here immediately.
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

/// Cause **A/FROZEN**: a mutating method that must delete a NON-CONFIGURABLE
/// own property deleted it anyway, and separately every mutating method
/// discarded `[[Set]]`/`[[Delete]]`'s refusal.
///
/// **FIXED in Batch 8.** The rule was never missing — `Interp::delete_property`
/// and `Reflect.deleteProperty` both refused correctly and left the receiver
/// bit-identical, all along — the array builtins were reaching the operation
/// through a **second implementation** (`array.rs::index_delete`, no descriptor
/// check, no failure channel) and through `.map_err(Throw::Value)?` on
/// `dispatch_set`, which keeps the `Err` and drops the `Ok(false)`. Batch 8
/// added `internal_methods::dispatch_delete` (the `[[Delete]]` table slot had
/// existed since the beginning with `Reflect.deleteProperty` as its only
/// caller), routed the delete through it, and made `index_set`/`set_array_len`
/// observe the boolean as `Set(O, k, v, true)` requires. All seven rows below
/// now match node.
///
/// `set_array_len` also had to be reordered to write the `length` property
/// BEFORE trimming the element store: trimming first meant a refused length
/// write on a frozen array threw *after* it had already shortened the store,
/// which is the "throws the right error and corrupts the object anyway" shape.
///
/// What is left, and is NOT fixed here: **a frozen real ARRAY's index delete is
/// still unreffusable**, because v12 models no per-index attributes for
/// `Kind::Array` at all (`Object.freeze([1,2,3])` leaves every element
/// reporting `writable:true, configurable:true`). That needs per-element
/// attribute storage in `v12-heap/src/elements.rs` — a different crate — and is
/// pinned as a canary by
/// [`a_frozen_array_index_delete_is_a_declared_deviation`].
const FROZEN_REFUSAL_UNOBSERVED: &str = "FROZEN (FIXED in Batch 8 — kept as the \
     audit trail for the fix, no longer attached to any row): on an integrity-level \
     receiver every mutating method whose spec step requires a write to succeed proceeded \
     as if it had. reverse/copyWithin/shift additionally DELETEd the non-configurable own \
     index while Object.isFrozen still answered true, fill returned the receiver after a \
     refused index write, unshift returned a length it could not write, and splice/pop \
     returned normally after a refused length write. Cause: array.rs re-implemented \
     [[Delete]] without the configurable check and discarded [[Set]]'s refusal boolean; \
     the interpreter's own machinery was correct throughout and merely bypassed.";

/// Cause **B/DELETED-KEY**: `Object.keys` reported an own key that had been
/// deleted, because the descriptor list and the cell disagreed.
///
/// **FIXED in Batch 8.** A `delete` stores `hole` in a data slot and leaves the
/// shared shape descriptor, so the descriptor list is not the own-key set. Every
/// other observer already filtered on liveness — `hasOwnProperty`,
/// `Object.hasOwn` and `getOwnPropertyDescriptor` through `descriptor_is_live`
/// in `builtins/object.rs`, `[[Get]]`/`[[HasProperty]]` in `ordinary_get`,
/// `Reflect.ownKeys` by an inline copy of the same test — and
/// `collect_own_string_keys` was the ONE reader that consulted nothing. It is
/// now the single place the filter is applied, which is what brought
/// `Object.keys`, `for-in`, `Object.getOwnPropertyNames`, `Object.entries` and
/// `Object.values` into agreement with `[[Get]]` at the same key in the same
/// tick.
///
/// This is the fourth observer census finding worth recording: the `[[Delete]]`
/// table slot and the own-key reader were wrong in the SAME way, in two crates,
/// for the same one-line reason. "Seven rungs across two crates" was the shape
/// of the earlier Hole defect and this batch found its mirror image.
const DELETED_KEY_IN_DESCRIPTOR_LIST: &str = "FIXED in Batch 8 — no row carries this any more.";

/// The one row Batch 8 **added**: `pop` on `accessor_index`.
///
/// Not a new defect — an old one that two other defects were hiding behind each
/// other, and the clearest argument in this file's history for fixing causes
/// instead of tolerating them.
///
/// On `accessor_index` (`{0:"a", get 1(){return "acc";}, 2:"c", length:3}`),
/// `pop` deletes index 2 and lowers `length` to 2. Three independent things are
/// true of the resulting own-key count:
///
/// * **node: 3** — keys `0`, `1`, `length`.
/// * **v12 before Batch 8: 3** — keys `0`, `2`, `length`.
/// * **v12 after Batch 8: 2** — keys `0`, `length`.
///
/// The middle answer matched the first for two unrelated wrong reasons. v12's
/// extra key was B's bug: index 2's cell was holed and its descriptor left
/// behind, so the descriptor list still counted a deleted key. v12's *missing*
/// key was E1: the literal accessor at index 1 is registered non-enumerable
/// (`DefineAccessor` carries no attribute operand), so `Object.keys` omits it —
/// `getOwnPropertyDescriptor(o,"1")` reports `enumerable:false` where node
/// reports `true`. `+1` and `−1` cancelled to `0`, and the generated table's
/// `Object.keys(o).length` column could not see either.
///
/// Batch 8 fixed B, so the `+1` went away and the `−1` became visible for the
/// first time. The row was attributed to E1, whose fix was in
/// `v12-bytecode` + `v12-bccompiler` and therefore not that batch's. **Batch 9
/// fixed E1** and `pop` on `accessor_index` is clean: v12 now reports the same
/// 3 own keys as node, for the right reason.
///
/// The transferable lesson, recorded because this table has now been wrong three
/// different ways: **an observable that two defects can cancel in is an
/// observable that will report a pass for a wrong reason.** The harness's
/// `snap` is a single number, and a single number is the worst possible probe
/// for a two-defect receiver. `a_deleted_own_key_is_invisible_to_every_observer`
/// asserts the per-key composition instead, which is what made the +1 visible
/// in the first place.
const E1_UNMASKED_BY_BATCH_8: &str = "E1, UNMASKED by the Batch-8 fix to the deleted-key defect: \
     the literal accessor at index 1 is registered non-enumerable, so Object.keys omits it. \
     Before Batch 8 this row's key count coincided with node's because a DELETED index 2 \
     was still being counted on the other side; the two errors cancelled.";

/// The two receivers that carry an accessor property. Both diverge on almost
/// every row, for two reasons that have nothing to do with each other and are
/// therefore almost always named separately in the ledger below.
const ACCESSOR_RECEIVERS: &[&str] = &["accessor_index", "length_getter"];

/// Cause **E1**: an accessor written in an object literal was registered
/// **non-enumerable**, so `Object.keys` omitted it while node reported it.
///
/// **FIXED in Batch 9, in `v12-bytecode` + `v12-bccompiler` (ADR-0005).**
/// `Interp::op_define_accessor` hardcoded `Attrs::BUILTIN`, and
/// `Attrs::BUILTIN` is `WRITABLE | CONFIGURABLE`, i.e. `enumerable == false`.
/// That is *correct* for a class accessor (`class C { get g(){} }` must be
/// non-enumerable — verified against node) and wrong for an object-literal one
/// (`{ get g(){} }` must be enumerable). Both reached the same opcode
/// (`DefineAccessor = 66`), from `v12-bccompiler`'s `expr.rs` (literal) and
/// `class.rs` (class), and `Opcode::DefineAccessor`'s three operands are
/// `Obj`, `Key`, `Pair` — there was **no attribute operand**, so the
/// interpreter could not tell the two spellings apart.
///
/// The fix is a sibling opcode, `DefineAccessorEnumerable = 79`, emitted only
/// by the literal lowering; the class spelling still emits `DefineAccessor =
/// 66`, so no class-emitted byte changed (proved by a byte-identical
/// disassembly of a class program across the change, not only by the answers).
/// The attributes are now a parameter of `op_define_accessor`:
/// `Attrs::BUILTIN` for the class spelling — bit for bit what it hardcoded —
/// and the new `Attrs::ENUMERABLE_ACCESSOR` for the literal one.
///
/// Nothing depended on the old value. The *only* consumer of the literal's
/// enumerability in this table was `Object.keys`. `{ m(){} }` (a method
/// shorthand) was and remains enumerable and never reached
/// `op_define_accessor`: `expr.rs`'s `object_prop` routes `Init` through
/// `SetProperty`. That was true before the fix and is still true, which is why
/// the ledger could say "the accessor spelling only" and be right.
///
/// At the time it was written the diagnosis also recorded what was NOT wrong:
/// `getOwnPropertyNames({0:"a", get length(){…}})` answered `0,length` in v12
/// — the key was in the shape all along — while
/// `getOwnPropertyDescriptor(…).enumerable` answered `false`. One attribute,
/// one observer.
const E1_ACCESSOR_NON_ENUMERABLE: &str = "FIXED in Batch 9 — no row carries this any more.";

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

/// The complete, current divergence set: **107 of 663 rows, 20 methods**
/// (17 receivers × 39 rows).
///
/// A `Known` entry is one (method, receivers, cause) triple, and **a method may
/// appear more than once** when it diverges on different receivers for
/// different reasons — [`declared_divergences`] unions the receiver sets
/// instead of letting a later entry overwrite an earlier one, which is what
/// makes per-cause attribution possible without a second table.
///
/// # How the 107 rows divide
///
/// * **85 rows** are the five `absent` methods on all seventeen receivers. They
///   say nothing about the index/length model; they were 45 of 351 before
///   Batch 6 and are still the same five methods.
/// * **19 rows** are E2 — 6 on `accessor_index` (`copyWithin`, `slice`,
///   `reverse`, `shift`, `unshift`, `flat`) and 13 on `length_getter` (those
///   six plus `fill`, `splice`, `pop`, `indexOf`, `lastIndexOf`, `includes`,
///   `at`). The callback-seam methods read `length` through the interpreter
///   and agree with node on these receivers; the native path does not.
/// * **2 rows** are `length_lower` on `array_dense` and `array_holey` — a real
///   array's element store and its shape disagree after `o.length = 1`. The
///   accessor receivers' `length_lower` rows were E1's and are now fixed.
/// * **1 row** is `index_assign_past` on `arguments_exotic`.
///
/// 85 + 19 + 2 + 1 = 107, and 20 of the 39 methods carry at least one row.
/// **No row is charged to E1 any more**: Batch 9 removed all 24 of its ledger
/// entries, including its two `length_lower` ones — the accessor receivers'
/// `length_lower` rows agree with node without any further claim.
///
/// # Batch 8: 177 → 156, and the one row it ADDED
///
/// 22 rows were fixed (all seven `frozen_object` mutators, and the
/// deleted-key cluster on `own_numeric_length`, `own_string_length`,
/// `holey_object` and `length_float`) and **one row was added**: `pop` on
/// `accessor_index`.
///
/// That addition is worth reading twice, because it is the argument for fixing
/// defects rather than tolerating them. Before Batch 8, `pop` on
/// `accessor_index` reported 3 own keys and node reported 3 — and **both were
/// wrong for unrelated reasons**: v12's +1 was B's bug (a deleted index left in
/// the descriptor list) and node's real answer was also 3 because `pop` *writes*
/// index 1 (which v12 could not see as enumerable) while *deleting* index 2
/// (whose stale key v12 was still counting). The two errors cancelled to zero.
/// Fixing B removed the +1 and exposed E1's −1 underneath it. A tolerance for
/// "the count matches" would have hidden this permanently; a test asserting the
/// composition would not have. See [`E1_UNMASKED_BY_BATCH_8`]. Batch 9 removed
/// the −1, and `pop` on `accessor_index` is now clean.
///
/// # Batch 9: 156 → 107, and six E2 entries narrowed
///
/// E1's 24 entries were deleted outright — every one of them a method whose
/// divergence was E1 *alone* or E1 unioned with a cause that survives. Six E2
/// entries had listed `ACCESSOR_RECEIVERS` while E2 in fact only ever applied
/// to `length_getter` (`fill`, `splice`, `indexOf`, `lastIndexOf`, `includes`,
/// `at`), and are now narrowed to it. The union in [`declared_divergences`]
/// had been masking that: E1 claimed both receivers for those methods, so the
/// over-broad E2 claim was never the *observed* set, and removing E1 exposed
/// it. `accessor_index`'s accessor is at an integer key, which the native
/// reader takes from the property store; `length_getter`'s accessor is
/// `length` itself, which every native path must go through `[[Get]]` to
/// learn. That is the whole reason the two receivers need separate claims.
///
/// Every batch number recorded against 351 is stated against a denominator this
/// table no longer has; see the header.
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
    // -- deleted keys stayed visible to Object.keys -------------------------
    //
    // ONE defect behind these entries, which the generated table separated for
    // us: `hasOwnProperty(o, i)` said false but `Object.keys(o)` still counted
    // the index. **Batch 8 FIXED this**; **Batch 9 FIXED the E1 residue**, so
    // this whole section is gone. `pop` and `shift` below survived Batch 8
    // carrying E1 alone, and `splice` carried E1 on `length_getter` — all
    // three now agree with node (`pop` on `accessor_index` was the row whose
    // two cancelling errors Batch 8 unmasked; see
    // [`E1_UNMASKED_BY_BATCH_8`]). The ratchet fails in both directions, so
    // leaving the entries would have been a failure, not a harmless leftover.
    // `holey_object` had extended the same cluster to `copyWithin`, `sort` and
    // `unshift` before Batch 8. See [`DELETED_KEY_IN_DESCRIPTOR_LIST`].
    //
    // `length_lower` on `array_dense`/`array_holey` below is NOT part of that
    // cluster: it is `o.length = 1` on a real array dropping the element but
    // not its own key, and it has nothing to do with `Object.keys` seeing a
    // deleted key.
    // `o.length = 1` drops the element but not its own key: the element store
    // and the shape descriptor disagree after the write.
    //
    // This entry did NOT grow in Batch 9, and that is worth stating because it
    // was expected to: E1's `length_lower` entries on both accessor receivers
    // were deleted as FIXED, and it would have been tidy to widen this one to
    // cover them instead. The measured set says otherwise — on both accessor
    // receivers `length_lower` now agrees with node, so the two rows here are
    // the only two, and the cause is the real-array store/shape disagreement
    // alone.
    // -- arguments exotic ----------------------------------------------------
    // -- FROZEN: the integrity-level receiver ------------------------------
    //
    // **Batch 8 FIXED all seven of these.** `frozen_object` now diverges on
    // exactly five rows, all of them the `absent` methods above: every mutating
    // method agrees with node, throwing the spec's `TypeError` and leaving the
    // receiver bit-identical. See [`FROZEN_REFUSAL_UNOBSERVED`] for the cause
    // and [`a_refused_delete_leaves_the_receiver_uncorrupted`] for the
    // surviving-state assertions that hold it there.
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
        total, 85,
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

// ---------------------------------------------------------------------------
// 5. Defect A — a mutating method that must delete a NON-CONFIGURABLE own
//    property has to throw AND leave the object uncorrupted.
//
//    Asserting only "it throws" is the WEAKER test, and it is the one that
//    reads like a fix: an implementation that throws `TypeError` and *then*
//    holes the cell passes it while still corrupting the receiver — and it
//    corrupts it SILENTLY, because the descriptor is gone while
//    `Object.isFrozen` keeps answering `true` over a property that no longer
//    exists. That breaches the integrity-level invariant CONTEXT.md states.
//    Every assertion below is therefore on the SURVIVING STATE too.
//
//    Every `expected` here is node's verbatim answer, produced by
//    `/tmp`-equivalent `node -e` of the identical program; none is authored by
//    hand.
// ---------------------------------------------------------------------------

/// The frozen control receiver's state, as node reports it. Used as a prefix so
/// one assertion sees the throw AND the corruption in the same JSON array.
const FROZEN_CONTROL: &str = r#"[true,"a","{\"value\":\"a\",\"writable\":false,\"enumerable\":true,\"configurable\":false}",3]"#;

/// The receiver after every one of these methods threw: `isFrozen`, `"0" in o`,
/// the value, the own-key answer, the descriptor, the key list, the length.
/// Identical for all seven methods because in node none of them writes.
const FROZEN_AFTER: &str = r#",true,true,"a",true,"{\"value\":\"a\",\"writable\":false,\"enumerable\":true,\"configurable\":false}","0,length",3"#;

#[test]
fn a_refused_delete_leaves_the_receiver_uncorrupted() {
    for method in [
        "Array.prototype.reverse.call(o)",
        "Array.prototype.copyWithin.call(o,0,1,3)",
        "Array.prototype.shift.call(o)",
        "Array.prototype.fill.call(o,\"q\",0,3)",
        "Array.prototype.unshift.call(o,\"z\")",
        "Array.prototype.splice.call(o,1,1)",
        "Array.prototype.pop.call(o)",
    ] {
        let probe = |body: &str| {
            eval_json(&format!(
                r#"var o = Object.freeze({{0:"a",length:3}});
                   // Control read first, on the untouched receiver: a probe that
                   // failed to build the frozen object (and so had nothing to
                   // corrupt) fails HERE rather than passing the loop vacuously.
                   var control = [Object.isFrozen(o), String(o[0]),
                                  JSON.stringify(Object.getOwnPropertyDescriptor(o,"0")),
                                  o.length];
                   var r = {{}};
                   try {{ {body}; r.threw = false; }} catch (e) {{ r.threw = true; r.name = e.name; }}
                   JSON.stringify([r.threw ? "threw:"+r.name : "no-throw",
                                   control,
                                   Object.isFrozen(o),
                                   "0" in o,
                                   String(o[0]),
                                   Object.prototype.hasOwnProperty.call(o,"0"),
                                   JSON.stringify(Object.getOwnPropertyDescriptor(o,"0")),
                                   Object.keys(o).join(","),
                                   o.length]);"#
            ))
        };
        assert_is(
            &probe("var unusedProbe = 0;"),
            &format!(r#"["no-throw",{FROZEN_CONTROL}{FROZEN_AFTER}]"#),
            "control: a frozen receiver no method has touched",
        );
        assert_is(
            &probe(method),
            &format!(r#"["threw:TypeError",{FROZEN_CONTROL}{FROZEN_AFTER}]"#),
            method,
        );
    }
}

/// # DECLARED DEVIATION — a frozen ARRAY's index delete is still unreffusable
///
/// The one part of defect A this batch does **not** fix, and it is out of the
/// file set rather than out of reach: **v12 models no per-index attributes for a
/// real array.** `Object.defineProperty(a, 2, {configurable: false})` is
/// silently dropped and `Object.freeze([1,2,3])` leaves every element
/// reporting `writable:true, configurable:true` — so there is no attribute for
/// `ordinary_delete`'s element-store rung to read and none for
/// `array.rs::delete_index_or_throw`'s. Closing it needs per-element attribute
/// storage in `v12-heap/src/elements.rs`, which is a different crate from the
/// one this batch owns.
///
/// So `pop()` on a frozen array throws (the `length` write is refused and now
/// throws *before* the store trim) but has already deleted element 2, where
/// node refuses at the `DeletePropertyOrThrow` step and touches nothing.
///
/// Asserted as a CANARY, not as correctness: this pins v12's current answer so
/// that closing the gap in `v12-heap` turns this test RED and forces the
/// expectation to be rewritten, rather than the fix landing silently. The
/// `gopd` row in the expectation is the direct evidence for the diagnosis —
/// v12 reports `writable:true, configurable:true` for an index `node` reports
/// `writable:false, configurable:false`.
#[test]
fn a_frozen_array_index_delete_is_a_declared_deviation() {
    assert_is(
        &eval_json(
            r#"var a = [1,2,3];
               Object.freeze(a);
               var r = "";
               try { Array.prototype.pop.call(a); r = "no-throw"; }
               catch (e) { r = "threw:" + e.name; }
               JSON.stringify([r, [a.length, a[0], a[1], a[2]],
                               2 in a,
                               JSON.stringify(Object.getOwnPropertyDescriptor(a,"2")),
                               Object.isFrozen(a),
                               JSON.stringify(Object.getOwnPropertyDescriptor(a,"0"))]);"#,
        ),
        r#"["threw:TypeError",[3,1,2,null],false,null,true,"{\"value\":1,\"writable\":true,\"enumerable\":true,\"configurable\":true}"]"#,
        "DECLARED DEVIATION: v12 models no per-index attributes, so a frozen \
         array's index delete cannot be refused — see this test's doc comment",
    );
}

/// `Set(O, k, v, true)` must OBSERVE the refusal, which is the half of A that
/// has nothing to do with `delete`. The interpreter's machinery already answers
/// the boolean (`Reflect.set` on a frozen key is `false` in v12); `index_set`
/// discarded it.
///
/// `fill` is the isolating row and `sort` is deliberately NOT: `sort` is
/// intercepted at the seam into `Interp::array_sort_callback`, which goes
/// through the interpreter's own strict `set_property` and therefore already
/// throws. An earlier version of this test used `sort` and PASSED against the
/// unfixed engine — a false pass, because v12 also failed to reorder, so the
/// two answers coincided for the wrong reason. `fill` stays native, and every
/// index it touches is present-or-absent, so no delete can corrupt anything and
/// the write refusal is the only thing under test.
#[test]
fn a_refused_write_is_observed_by_the_method_that_must_throw() {
    assert_is(
        &eval_json(
            r#"var o = Object.freeze({0:"a",length:3});
               var r = "";
               try { Array.prototype.fill.call(o,"q",0,3); r = "no-throw"; }
               catch (e) { r = "threw:" + e.name; }
               JSON.stringify([r, String(o[0]), Object.keys(o).join(","), o.length,
                               JSON.stringify(Object.getOwnPropertyDescriptor(o,"0"))]);"#,
        ),
        r#"["threw:TypeError","a","0,length",3,"{\"value\":\"a\",\"writable\":false,\"enumerable\":true,\"configurable\":false}"]"#,
        "fill() on a frozen receiver throws and writes nothing",
    );
}

/// Guards the obvious over-correction: a fix that made EVERY delete refuse
/// would pass every row above. This one deletes an index the receiver does not
/// own (a successful no-op per ES) and then still hits the refused `length`
/// write, so it pins "refusal is per property", not "refuse everything".
#[test]
fn a_refusal_is_per_property_not_a_blanket_refusal() {
    assert_is(
        &eval_json(
            r#"var o = Object.freeze({0:"a",1:"b",length:3});
               var r = [];
               try { Array.prototype.pop.call(o); r.push("pop-no-throw"); }
               catch (e) { r.push("pop-threw:"+e.name); }
               try { Array.prototype.splice.call(o, 2, 0); r.push("splice-no-throw"); }
               catch (e) { r.push("splice-threw:"+e.name); }
               JSON.stringify([r, Object.keys(o).join(","), String(o[0]), String(o[1]), o.length]);"#,
        ),
        r#"[["pop-threw:TypeError","splice-threw:TypeError"],"0,1,length","a","b",3]"#,
        "a frozen receiver refuses the index delete and the length write, and nothing else changes",
    );
}

// ---------------------------------------------------------------------------
// 6. Defect B — a deleted own key must be invisible to EVERY observer.
//
//    `Object.keys`/`for-in`/`entries` read the DESCRIPTOR LIST; `[[Get]]`/
//    `hasOwnProperty` read the CELL. `delete` holes the cell and leaves the
//    descriptor, so the two families disagree — and the descriptor list is the
//    one that is wrong. Every observer is asserted here, INCLUDING the ones
//    that were already right (`Reflect.ownKeys`, `gopd`, `JSON.stringify`), so
//    a fix that only patches `Object.keys` and leaves `for-in` or `entries`
//    wrong fails here rather than reading as done.
// ---------------------------------------------------------------------------

#[test]
fn a_deleted_own_key_is_invisible_to_every_observer() {
    // Written by the INTERPRETER's delete, and written by a METHOD's
    // `index_delete` — two different writers, so two rows.
    assert_is(
        &eval_json(
            r#"var o = {0:"a",1:"b",length:2};
               // Control: the CELL-reading observers, BEFORE the delete. They
               // already answered "absent" after it, and they are what the
               // descriptor-list readers have to agree WITH — not merely with
               // each other.
               var control = ["0" in o, Object.prototype.hasOwnProperty.call(o,"0"),
                              String(o[0]),
                              Object.getOwnPropertyDescriptor(o,"0") === undefined];
               delete o[0];
               var forin = []; for (var k in o) { forin.push(k); }
               JSON.stringify([control,
                               Object.keys(o), forin, Object.getOwnPropertyNames(o),
                               Object.entries(o), Object.values(o), Reflect.ownKeys(o),
                               ["0" in o, Object.prototype.hasOwnProperty.call(o,"0"),
                                Object.hasOwn(o,"0"), Reflect.has(o,"0"), String(o[0]),
                                Object.getOwnPropertyDescriptor(o,"0") === undefined,
                                "0" in Object.getOwnPropertyDescriptors(o),
                                JSON.stringify(o)]]);"#,
        ),
        r#"[[true,true,"a",false],["1","length"],["1","length"],["1","length"],[["1","b"],["length",2]],["b",2],["1","length"],[false,false,false,false,"undefined",true,false,"{\"1\":\"b\",\"length\":2}"]]"#,
        "after `delete o[0]`, the descriptor-list observers agree with [[Get]]",
    );
    assert_is(
        &eval_json(
            r#"var o = {0:"a",1:"b",2:"c",length:3};
               Array.prototype.pop.call(o);
               var forin = []; for (var k in o) { forin.push(k); }
               JSON.stringify([Object.keys(o), forin, Object.getOwnPropertyNames(o),
                               "2" in o, Object.prototype.hasOwnProperty.call(o,"2"),
                               String(o[2]), Object.entries(o).length]);"#,
        ),
        r#"[["0","1","length"],["0","1","length"],["0","1","length"],false,false,"undefined",3]"#,
        "index_delete's own hole is invisible to the descriptor list too",
    );
}

/// The descent case the generated table measures: `sort` on `holey_object`
/// vacates index 2 (the only index `node` deletes here — the ledger used to say
/// index 1, which is the index `node` *writes*), so the own-key count must fall
/// from 3 to 3-becoming-2-plus-the-written-one and never exceed node's.
#[test]
fn a_delete_performed_by_a_method_lowers_the_own_key_count() {
    assert_is(
        &eval_json(
            r#"var o = {0:"a",2:"c",length:3};
               var before = Object.keys(o).length;
               Array.prototype.sort.call(o);
               JSON.stringify([before, Object.keys(o).length,
                               Object.keys(o).join(","),
                               Object.prototype.hasOwnProperty.call(o,"1"),
                               Object.prototype.hasOwnProperty.call(o,"2")]);"#,
        ),
        r#"[3,3,"0,1,length",true,false]"#,
        "sort writes index 1 and deletes the vacated index 2",
    );
}

// ---------------------------------------------------------------------------
// E1: the two accessor spellings, pinned separately
// ---------------------------------------------------------------------------

/// An accessor in an **object literal** is enumerable, and both key readers
/// agree — this is E1, the defect that made `Object.keys({0:"a", get
/// length(){return 3}})` answer `["0"]` where node answers `["0","length"]`.
///
/// Every expectation here was read off `node` for this commit, not off v12:
///
/// ```text
/// $ node -e 'console.log(JSON.stringify(Object.keys({0:"a", get length(){return 3}})))'
/// ["0","length"]
/// ```
///
/// The descriptor is checked as well as the key set, because the two are
/// separate facts and E1 was a descriptor fact that only *showed up* through
/// `Object.keys`: `Object.getOwnPropertyNames` already answered `0,length`
/// before the fix, which is what narrowed the diagnosis to one attribute.
#[test]
fn an_object_literal_accessor_is_enumerable_and_node_agrees() {
    assert_is(
        &eval_json(
            r#"var o = {0:"a", get length(){return 3}};
               var forin = []; for (var k in o) { forin.push(k); }
               var d = Object.getOwnPropertyDescriptor(o, "length");
               JSON.stringify([Object.keys(o), forin, Object.getOwnPropertyNames(o),
                               d.enumerable, d.configurable, String(o.length),
                               Object.keys({get g(){return 1}, set g(v){}}).join(","),
                               Object.getOwnPropertyNames({get g(){return 1}}).join(",")]);"#,
        ),
        r#"[["0","length"],["0","length"],["0","length"],true,true,"3","g","g"]"#,
        "a literal accessor is enumerable to every own-key reader (node agrees)",
    );
}

/// The other half of the same decision: a **class** accessor is
/// **non-enumerable**, and it must stay that way.
///
/// This is the negative control for the fix. `DefineAccessorEnumerable` is
/// emitted only by the literal lowering, so a change that made accessors
/// enumerable *uniformly* — the naive reading of E1 — turns this test red. The
/// mutation is run and confirmed red in the batch notes; `node` is the oracle:
///
/// ```text
/// $ node -e 'console.log(JSON.stringify(Object.keys(new (class C { get g(){return 3} })())))'
/// []
/// ```
///
/// `static` is included because the class emitter routes both the instance and
/// the static spelling through the same `DefineAccessor` (class.rs), so a
/// half-applied fix would show up on exactly one of them.
#[test]
fn a_class_accessor_stays_non_enumerable_and_node_agrees() {
    assert_is(
        &eval_json(
            r#"class C { get g(){return 3} set g(v){} static get s(){return 4} m(){} }
               var i = new C();
               var di = Object.getOwnPropertyDescriptor(C.prototype, "g");
               var ds = Object.getOwnPropertyDescriptor(C, "s");
               JSON.stringify([Object.keys(i), Object.keys(C.prototype), Object.keys(C),
                               di.enumerable, di.configurable, ds.enumerable,
                               Object.keys(C.prototype).join(","),
                               i.g, C.s, typeof i.m]);"#,
        ),
        r#"[[],[],[],false,true,false,"",3,4,"function"]"#,
        "a class accessor is non-enumerable on both the prototype and the ctor",
    );
}

/// Method shorthand is a third spelling and was never E1's problem: it routes
/// through `SetProperty`, never reached `op_define_accessor`, and must still be
/// enumerable. Pinning it separately is what stops a future "make all accessors
/// enumerable" fix from being written as "make all literal properties
/// enumerable".
#[test]
fn a_method_shorthand_is_still_enumerable_and_node_agrees() {
    assert_is(
        &eval_json(
            r#"var o = {m(){}, 0:"a", b:2, get g(){return 1}};
               var d = Object.getOwnPropertyDescriptor(o, "m");
               JSON.stringify([Object.keys(o), d.enumerable, d.writable, d.configurable,
                               typeof o.m, Object.keys({m(){}}).join(",")]);"#,
        ),
        r#"[["0","m","b","g"],true,true,true,"function","m"]"#,
        "a method shorthand keeps all three attributes; node reports the same",
    );
}
