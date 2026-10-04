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
//! Measured at this commit: **80 of 351 rows diverge, across 14 methods**
//! (9 receivers × 39 rows). 45 of those are five unimplemented methods on all
//! nine receivers. The last committed claim, ADR-0004's "24", was measured on
//! 306 rows with a single receiver and one observation per row; it is not
//! comparable and is superseded by [`KNOWN_DIVERGENCES`].
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
//! # A NAMED GAP this batch found in the table itself
//!
//! Mutation: flatten the generic-receiver branch of the range reader (drop the
//! `present` bit on the `index_get` answer). The 351-row table does **not**
//! notice — it stays at 80/14 — and only the focused test fails.
//!
//! The reason is structural, and it is worth stating because it bounds what the
//! table can ever claim: `visible_extent` short-circuits every index at or past
//! the receiver's highest visible index to "hole" without dispatching, so the
//! `index_get`-presence branch only runs for indices BELOW the extent. Of the
//! table's four generic receivers, `inherited_indices` has extent 1 (its only
//! index, `0`, is *present*) and `inherited_length`/`own_*` have extent 0 or 3
//! with every index present. **A hole strictly below the visible extent of a
//! non-`Array` receiver is therefore exercised at no index at all in 351 rows.**
//! `{0:"a", 2:"c", length:3}` — extent 2, hole at 1 — is the smallest receiver
//! that reaches it, and it is the focused test's, not a table row's. Adding it
//! is one `receiver` stanza in the row source; it is not added here because it
//! would change the 351-row denominator the batch accounting is stated in.
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
//!   behind.) What remains at that seam is a *different* defect, listed here:
//!   `find`/`findIndex` skip holes where ES (and test262's
//!   `predicate-called-for-each-array-property`) require the predicate to run
//!   on every index, and `findLast`/`findLastIndex` walk forward instead of
//!   backward.
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

/// Every receiver, in row-source order.
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
];

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

/// The complete, current divergence set: **80 of 351 rows, 14 methods**.
///
/// Each entry is one *defect*, not one symptom, so the count is the number of
/// things to fix rather than the number of places they show. Read with the
/// caveat in the module header: 45 of the 80 rows are the five `absent`
/// entries and say nothing about the index/length model.
const KNOWN_DIVERGENCES: &[Known] = &[
    // -- unimplemented: five rows below cover all nine receivers each --------
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
    // ONE defect behind four entries, which the generated table separates for
    // us: `hasOwnProperty(o, i)` says false but `Object.keys(o)` still counts
    // the index, because `index_delete`/`delete_property` hole the storage
    // cell and leave the shape descriptor behind. Every entry below is a case
    // where that makes the `Object.keys` column differ from node's.
    Known {
        method: "pop",
        receivers: &["own_numeric_length", "own_string_length"],
        absent: false,
        why: "`hasOwnProperty(o, i)` is false but `Object.keys(o)` still counts the \
              deleted index: the shape descriptor survives the delete",
    },
    Known {
        method: "shift",
        receivers: &["own_numeric_length", "own_string_length"],
        absent: false,
        why: "same as pop: a deleted own index stays in Object.keys",
    },
    Known {
        method: "splice",
        receivers: &["own_numeric_length", "own_string_length"],
        absent: false,
        why: "same as pop/shift, and it is the ONLY remaining defect on these two \
              receivers: the hole materialisation in the removed-element array \
              this entry used to also carry is fixed (see `a_hole_is_never_\
              materialised_as_an_own_undefined`)",
    },
    Known {
        method: "length_lower",
        receivers: &["array_dense", "array_holey"],
        absent: false,
        why: "`o.length = 1` on a real array drops the element but not its own key",
    },
    // -- the callback seam's hole handling and direction --------------------
    Known {
        method: "find",
        receivers: &[
            "inherited_length",
            "array_holey",
            "sparse_ctor",
            "length_assigned",
        ],
        absent: false,
        why: "the seam SKIPS holes; ES requires the predicate to run once per index \
              (test262 find/predicate-called-for-each-array-property asserts the \
              predicate is called 4 times for [undefined, , , 'foo'])",
    },
    Known {
        method: "findIndex",
        receivers: &[
            "inherited_length",
            "array_holey",
            "sparse_ctor",
            "length_assigned",
        ],
        absent: false,
        why: "same seam hole-skip as find",
    },
    Known {
        method: "findLast",
        receivers: ALL_RECEIVERS,
        absent: false,
        why: "walks FORWARD: the visitation order is 0,1,2 rather than 2,1,0. The \
              return value happens to agree on a dense receiver, so only the \
              recorded invocations expose it",
    },
    Known {
        method: "findLastIndex",
        receivers: ALL_RECEIVERS,
        absent: false,
        why: "same forward-walking seam as findLast",
    },
    // -- arguments exotic ----------------------------------------------------
    Known {
        method: "index_assign_past",
        receivers: &["arguments_exotic"],
        absent: false,
        why: "`arguments[7] = x` grows `arguments.length` to 8; the exotic's length \
              is fixed at creation and an index write must not touch it",
    },
];

/// Divergences this file knows about, folded into the same shape the test
/// builds from the table, so one `assert_eq!` compares them.
fn declared_divergences() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    KNOWN_DIVERGENCES
        .iter()
        .map(|k| (k.method, k.receivers.iter().copied().collect()))
        .collect()
}

/// Runs every generated program once and returns, per receiver, the rows v12
/// produced. Nine evals total — one per receiver, not one per row.
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
        total, 80,
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
            assert!(
                !got.contains("threw:"),
                "{label} + {m} must not throw: {got}"
            );
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
// 6. The declared deviations, pinned by name.
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
