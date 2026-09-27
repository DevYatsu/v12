# Test262 built-ins Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Raise `built-ins` Test262 conformance from 8 314 / 24 061 (35.4 %) to a core-scoped ~67 %, by closing twelve measured root causes worth ~2 000 tests.

**Architecture:** No new subsystems. Every task is a bug fix or a surface materialization inside the existing `Ctx` builtin architecture. The plan is ordered by measured tests-passed-per-unit-of-effort, and the first task makes the scoreboard honest so progress is measurable.

**Tech Stack:** Rust (workspace: `v12-heap`, `v12-interp`, `v12-bccompiler`, `v12-engine`, `v12-native`, `test262-runner`), `cargo nextest`, the in-tree Test262 runner at `conformance/run.sh`.

**Spec:** `CONTEXT.md` (ubiquitous language + gates), `ROADMAP.md` §1/§3, `conformance/fix-log.md` (done-history + before/after convention). The root-cause evidence is the measured baseline in this plan's Baseline section; every claim there is reproduced by a minimal repro in the task that fixes it.

---

## ⚠ METHOD CORRECTION (2026-09-27, after Task 5) — the estimates below are not predictions

Tasks 1, 4 and 5 are executed. Task 5 was estimated at **535 tests and delivered 112**.

**The plan sized every task by how many tests shared a failure message. That measures symptom frequency, not fix effort.** Where one missing thing masks a deeper set of gaps, the cluster overstates the yield by a large factor. Installing the `Iterator` global fixed 112 of the 410 tests that reported `ReferenceError: Iterator is not defined`; the other ~298 unblocked and immediately failed one layer deeper — `built-ins/Iterator`'s top cause is now **232 × `callee is not a function`**, the same tests shifted.

**Therefore:**
- **Re-cluster after every task lands** (`./conformance/faildiff.sh <before> <after>`, then group the residual failures by message). Never predict a task's yield from the pre-fix cluster.
- **The ~67 % core projection is not credible on this evidence.** Treat it as a loose bound and re-measure. Actual: core 54.5 % → 57.2 % → 57.9 % after three tasks.
- **Overestimation risk is highest** where the blocking error is a `ReferenceError` or a "not a function" that hides unimplemented behaviour — **Task 6 (RegExp.prototype, est. ~300) is the biggest exposure and should be re-scoped on evidence first.** Risk is lowest where each test exercises one self-contained method or one specific thrown error — Tasks 2, 3, 7, 8, 11, 12.
- **A second, already-identified item is missing from the task list:** the eight `Iterator` callback helpers (`map`/`filter`/`flatMap`/`reduce`/`forEach`/`some`/`every`/`find`) already had bodies before Task 5, but eager, and `map`/`filter`/`flatMap` return an **Array** rather than an iterator (`Interp::run_iterator_callback`, `crates/v12-interp/src/call_setup.rs:4406`). That is ≈232 tests and is the true cause of the 125 `callee is not a function` failures in this area.

---

## Baseline (measured, not estimated)

Measured on `cdb2cb04` with `./conformance/run.sh --filter built-ins --jobs 0 --format human,json --json-out /tmp/bi-head.json`:

```
TOTAL   24061   8314 pass   15176 fail   571 skip   35.4 %
```

**The denominator is the problem.** 8 794 of the 24 061 tests (37 %) belong to features
that do not exist in v12 at all and therefore score 0 %:

| Deferred feature | Failing tests |
|---|---|
| `Temporal` | 4 603 |
| `TypedArray` + `TypedArrayConstructors` + `DataView` + `ArrayBuffer` + `Uint8Array` | 2 718 |
| `Atomics` + `SharedArrayBuffer` | 332 |
| `WeakMap` + `WeakSet` + `WeakRef` + `FinalizationRegistry` | 302 |
| `DisposableStack` + `AsyncDisposableStack` + `SuppressedError` | 219 |
| `BigInt` | 75 |
| `ShadowRealm` | 64 |

Scope decision (user, 2026-09-27): **core only — mega-features stay deferred.** So the
plan tracks two numbers:

- **Core %** = passes / tests in non-deferred topics. Today **8 314 / 15 187 = 54.7 %**. This is the real number.
- **Raw %** = passes / 24 061. Today 35.4 %. Task 1 moves 8 794 tests from `fail` to `skip`, which lifts raw % without a single engine change — that is the point: it makes the denominator tell the truth.

Remaining in-scope failures: **6 863**. The twelve tasks below target ~2 000 of them.

| # | Task | Root cause | Est. tests |
|---|---|---|---|
| 1 | Honest feature gating | out-of-scope tests counted as failures | 0 (metric only) |
| 2 | Array indexed accessor poisons shape transitions | accessor at an array index breaks later transitions | 317 |
| 3 | `ToPropertyDescriptor` validation | non-object descriptors accepted | 103 |
| 4 | Lone surrogates in `String.fromCodePoint` | `char::from_u32` rejects 0xD800–0xDFFF | 422 |
| 5 | `Iterator` global + prototype helpers | global absent; 11 helpers absent | 535 |
| 6 | `RegExp.prototype` materialization | prototype installs zero methods | ~300 |
| 7 | Array ES2023 methods + Set operations | 9 methods absent | ~200 |
| 8 | `length` on every installed builtin | 81 NativeIds install no `length` | ~60 |
| 9 | Array literals > 255 elements | `NewArray` length is `u8` | 34+ |
| 10 | `Number.prototype` brand checks | over-strict `this` validation | 37 |
| 11 | `escape` / `unescape` | globals absent | 27 |
| 12 | `String.prototype[Symbol.iterator]` | absent; for-of uses a special case | ~10 |

## Global Constraints

- `cargo nextest run --workspace` green after every task. Never `cargo test` as the gate.
- Conformance reads use default `--format human`. Machine-comparable data goes to a file via `--json-out`, never stdout. Full-suite runs are `--jobs 0` (~25 min); per-topic slices are seconds.
- No lane runs `git stash`, `git reset`, `git checkout`, or a workspace-wide `cargo fmt`. Use `cargo fmt -p <crate>` at most.
- `cargo clippy --workspace --all-targets`: 0 errors. The accepted unwrap/expect/panic policy set is unchanged.
- Commit each task immediately after its gate passes. Append a before/after entry to `conformance/fix-log.md` in the same commit.
- `CONTEXT.md` terms are canonical: JsValue, Smi, HeapRef, Handle, Shape, transition tree, Frame, Interned string, Holes, Ctx, NativeId, builtin.

## Lane ownership (read before dispatching anything in parallel)

The 2026-09-02 incident destroyed a day of work: parallel write lanes shared files. Every
engine file this plan touches is listed here. **Two tasks may run in parallel only if their
file sets are disjoint.**

| Task | Files touched | Group |
|---|---|---|
| 1 | `conformance/harness/src/runner.rs` | **H** (harness — disjoint from all engine work) |
| 2 | `crates/v12-heap/src/shape.rs`, `crates/v12-interp/src/property.rs` | **P** (property machinery) |
| 3 | `crates/v12-interp/src/property.rs`, `crates/v12-engine/src/builtins/object.rs` | **P** |
| 4 | `crates/v12-engine/src/builtins/string.rs` | **S** (string builtins) |
| 5 | `crates/v12-engine/src/builtins/mod.rs`, `realm.rs`, `iterator.rs` | **M** (surface) |
| 6 | `crates/v12-engine/src/builtins/mod.rs`, `realm.rs`, `regexp.rs` | **M** |
| 7 | `crates/v12-engine/src/builtins/mod.rs`, `realm.rs`, `array.rs`, `map.rs` | **M** |
| 8 | `crates/v12-engine/src/builtins/mod.rs`, `realm.rs` | **M** |
| 9 | `crates/v12-bccompiler/src/expr.rs`, `model.rs`, `crates/v12-bytecode/src/lib.rs` | **C** (compiler) |
| 10 | `crates/v12-engine/src/builtins/number.rs` | **N** |
| 11 | `crates/v12-engine/src/builtins/global.rs`, `number.rs`, `mod.rs` | **M** |
| 12 | `crates/v12-engine/src/builtins/mod.rs`, `realm.rs`, `string.rs` | **M** |

Safe parallel pairs: **H** with anything; **P** with **S**/**N**/**C**; **S** with **N**.
Tasks 5, 6, 7, 8, 11, 12 all touch `mod.rs` and `realm.rs` and are therefore **strictly
serial**. The critical path is M: 5 → 6 → 7 → 8 → 11 → 12.

---

### Task 1: Feature-gate the deferred built-ins in the runner

> **EXECUTED 2026-09-27 — Steps 3 and 4 as written were WRONG; do not follow them.**
> The gate is keyed on the **topic directory**, not the `features:` tag, and the
> 7 reason constants are reused as-is. Reasons, with measurements:
> - 8 of the 11 tags in Step 3 match **zero** files. Real spellings are CamelCase
>   (`Temporal`, `TypedArray`, `WeakRef`, `Atomics`, `ShadowRealm`,
>   `FinalizationRegistry`, `SharedArrayBuffer`); `weak-collections` and
>   `shared-memory` do not exist at all.
> - The Step 4 verification command is itself broken: `grep "features:.*$t"` only
>   matches the inline `features: [a, b]` spelling, so it reports `0` for tags
>   declared as block arrays.
> - Correcting the spellings is still not enough. `BigInt` is declared by 1 257
>   files but only 75 live in `built-ins/BigInt`; a tag gate would skip ~1 165
>   *in-scope* failures that tasks 2-12 exist to attack. And 7 tests in
>   `built-ins/Date/prototype/toTemporalInstant/` declare `features: [Temporal]`
>   and **pass because `Temporal` is absent** — a tag gate destroys 7 real passes.
> - `built-ins/BigInt/prototype/toString` is excepted: v12 implements its radix
>   argument and 2 tests pass there.
>
> Step 6's predicted numbers were also wrong. 8 794 is the **total** test count of
> the 17 deferred topics, not the failing count; 8 313 fail, 479 were already
> skips, 2 pass. Actual result: **pass 8 314 (unchanged) / fail 6 873 / skip 8 874**;
> core = 8 314 / 15 187 = **54.7 %**. See the `conformance/fix-log.md` entry dated
> 2026-09-27 for the full record. The implementation is `DEFERRED_TOPICS` +
> `DEFERRED_TOPIC_EXCEPTIONS` in `conformance/harness/src/runner.rs`.
>
> **Also added:** `conformance/faildiff.sh`, a fail-set regression gate. Every
> remaining task must run it against `/tmp/bi-head.json` (or the latest
> `bi-*.json`) — a net pass-rate rise can hide new failures, and the gate
> negative-test proved it reports `net delta +0` while correctly flagging
> injected regressions.

**Files:**
- Modify: `conformance/harness/src/runner.rs:738-777` (`skip_reason_for`)
- Test: `conformance/harness/src/runner.rs` (existing unit tests at the bottom of the file)

**Interfaces:**
- Consumes: `Frontmatter::has_feature(&str) -> bool` (already exists, `frontmatter.rs:51`).
- Produces: a `DEFERRED_FEATURES: &[(&str, &str)]` table (feature tag, skip reason) consulted by `skip_reason_for`, and a `core` summary field. No engine API changes.

**Why:** 8 794 tests currently report as failures for features v12 has deliberately not
implemented. That is 37 % of the denominator lying. `resizable-arraybuffer` and
`tail-call-optimization` are already gated this way — this extends the existing pattern.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `conformance/harness/src/runner.rs`:

```rust
#[test]
fn deferred_features_produce_skip_reasons() {
    let fm = Frontmatter {
        features: vec!["temporal".to_string()],
        ..Frontmatter::default()
    };
    let reason = skip_reason_for(&fm, "");
    assert_eq!(reason, Some(DEFERRED_TEMPORAL.to_string()));
}

#[test]
fn temporal_test_file_is_skipped_before_execution() {
    // The real file, so the tag list and the corpus cannot drift apart.
    let src = std::fs::read_to_string(
        "../test262/test/built-ins/Temporal/PlainDate/prototype/monthCode.js",
    )
    .expect("pinned test262 checkout must be present");
    let fm = crate::frontmatter::parse(&src).expect("frontmatter parses");
    assert!(
        skip_reason_for(&fm, &src).is_some(),
        "Temporal tests must be skipped, not failed"
    );
}

#[test]
fn core_builtin_is_not_skipped() {
    let fm = Frontmatter { features: vec![], ..Frontmatter::default() };
    assert!(skip_reason_for(&fm, "var x = [1,2,3].map(function(v){return v;});").is_none());
}
```

If `Frontmatter` has no `Default`, construct it field-by-field from the struct at
`frontmatter.rs:20-40` instead of using `..Default::default()`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p test262-runner deferred_features`
Expected: FAIL — `skip_reason_for` has no `DEFERRED_TEMPORAL` and a `Temporal` frontmatter falls through to `None`.

- [ ] **Step 3: Add the deferred-feature table**

In `conformance/harness/src/runner.rs`, immediately above `skip_reason_for`:

```rust
/// Features v12 has deliberately not implemented, mapped to the reason a test
/// is skipped instead of failed. `Temporal`, the TypedArray family, the weak
/// collections, explicit resource management, `Atomics` and `ShadowRealm` are
/// 8 794 tests (37 % of the `built-ins` corpus) that cannot pass and would
/// otherwise dominate the failure report. Adding an entry here changes a test
/// from `fail` to `skip` — it never changes pass/fail semantics.
const DEFERRED_TEMPORAL: &str =
    "requires temporal (Temporal not implemented; deferred by scope decision 2026-09-27)";
const DEFERRED_TYPED_ARRAY: &str =
    "requires typedarray (TypedArray/ArrayBuffer/DataView not implemented; deferred by scope decision 2026-09-27)";
const DEFERRED_WEAK: &str =
    "requires weak-collections (WeakMap/WeakSet/WeakRef/FinalizationRegistry not implemented; deferred by scope decision 2026-09-27)";
const DEFERRED_DISPOSABLE: &str =
    "requires explicit-resource-management (DisposableStack not implemented; deferred by scope decision 2026-09-27)";
const DEFERRED_ATOMICS: &str =
    "requires atomics (Atomics/SharedArrayBuffer need a shared mutator; out of v1 scope)";
const DEFERRED_SHADOW_REALM: &str =
    "requires shadow-realm (v1 is a single realm)";
const DEFERRED_BIG_INT: &str =
    "requires BigInt (BigInt arithmetic not implemented; deferred by scope decision 2026-09-27)";

/// Order matters only for the reason string; the first match wins.
const DEFERRED_FEATURES: &[(&str, &str)] = &[
    ("temporal", DEFERRED_TEMPORAL),
    ("typedarray", DEFERRED_TYPED_ARRAY),
    ("resizable-arraybuffer", DEFERRED_TYPED_ARRAY),
    ("weak-ref", DEFERRED_WEAK),
    ("weak-collections", DEFERRED_WEAK),
    ("finalization-registry", DEFERRED_WEAK),
    ("explicit-resource-management", DEFERRED_DISPOSABLE),
    ("atomics", DEFERRED_ATOMICS),
    ("shared-memory", DEFERRED_ATOMICS),
    ("shadow-realm", DEFERRED_SHADOW_REALM),
    ("BigInt", DEFERRED_BIG_INT),
];
```

Replace the existing `resizable-arraybuffer` arm (the block at `runner.rs:771-775`) with:

```rust
    for (tag, reason) in DEFERRED_FEATURES {
        if fm.has_feature(tag) {
            return Some(reason.to_string());
        }
    }
    None
```

- [ ] **Step 4: Verify the tags actually match the corpus**

Test262 declares these features with specific spellings. Confirm before trusting the
numbers, and correct the table above for any tag that matches nothing:

Run:
```bash
cd conformance/test262/test
for t in temporal typedarray resizable-arraybuffer weak-ref weak-collections \
         finalization-registry explicit-resource-management atomics shared-memory \
         shadow-realm BigInt; do
  printf '%-32s %s\n' "$t" "$(grep -rl "features:.*$t" --include='*.js' built-ins annexB 2>/dev/null | wc -l)"
done
```

Expected: every tag except a deliberate alias must report a non-zero count. A tag
reporting `0` is a typo — fix the table before proceeding. Record the counts in the
`fix-log.md` entry.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p test262-runner`
Expected: PASS (35 existing + 3 new).

- [ ] **Step 6: Re-measure the baseline**

Run:
```bash
./conformance/run.sh --filter built-ins --jobs 0 --format human,json --json-out /tmp/bi-core.json
```
Expected: `skip` rises from 571 to roughly 9 365; `fail` drops from 15 176 to roughly
6 383; `pass` unchanged at 8 314. **Core % = 8 314 / 15 267 = 54.5 %** is now the
headline number. Paste the summary line into `fix-log.md`.

- [ ] **Step 7: Commit**

```bash
git add conformance/harness/src/runner.rs conformance/fix-log.md
git commit -m "harness: skip deferred-feature built-ins instead of failing them"
```

---

### Task 2: An indexed accessor on an array poisons later shape transitions

> **DEFERRED 2026-09-27 — root cause below is WRONG, and the yield is 57, not 317.**
> Measured with a throwaway patch, then reverted. Nothing landed. The lane correctly
> declined to ship a partial fix: 3 of the 4 files a correct fix must touch were outside
> its write scope. Do not attempt the steps below as written.
>
> **Real root cause:** not a shape transition, and not `IsCompatiblePropertyDescriptor` —
> `crates/v12-heap/src/shape.rs` **is not on the code path at all.** It is a blanket 2-line
> reject in `crates/v12-engine/src/internal_methods.rs:379-383`, in `apply_property_descriptor`
> (the 10.1.6.3 driver behind `Object.defineProperty`/`defineProperties`/
> `__defineGetter__`/`Reflect.defineProperty`):
> ```rust
> if (kind == Arguments || kind == Array) && let Some(idx) = prop_key_as_index(heap, key) {
>     if desc.is_accessor() { return Ok(false); }   // <-- the whole bug
> ```
> All three throw sites (`object.rs:174, 1368, 1582`) funnel through this arm.
> `is_compatible_property_descriptor` as sketched above is inapplicable to the real code.
>
> **Yield: 57 of 317 (18 %).** The other 260 fail immediately afterwards on a *different*
> real assertion — 58 `testResult !== true` (the getter is never invoked by the builtin),
> 31 `should not be enumerable`/`configurable`, 12 `newArr.length`, 11 `some(...) !== true`,
> 10 `callee is not a function`, 10 `Expected obj[N] to be writable`, 10 `Expected TypeError`.
> Only 55 (34 Object + 21 Array) pass outright.
>
> **The 4-file change, measured not guessed** (only 2 are in the old scope, and 1 of those
> is inert without the others):
> | file | change |
> |---|---|
> | `crates/v12-engine/src/internal_methods.rs:379` | accept an accessor at an index: file a dict-rung accessor, `delete_element` to hole the element, **and validate the data branch** |
> | `crates/v12-interp/src/property.rs:205` (`get_property`), `:1963` (`ordinary_own_descriptor`) | the array-index fast path answers from the element store unconditionally; a holed index with a dict entry must fall through to the walk so the getter is invoked |
> | `crates/v12-interp/src/call_setup.rs:2690` (`callback_elem`) | reads the element store directly, so `forEach`/`map`/`every`/`some`/`filter`/`reduce`/`sort`/`find*` never see an index accessor |
> | `crates/v12-engine/src/builtins/array.rs:271` (`read_index`) | same, for `indexOf`/`lastIndexOf`/`includes`/`reverse`/`concat`/`toReversed`/`with`/`toSpliced` |
>
> **Two traps:** the `enumerable` default for a *new* index accessor is `false` (seeding from
> `Attrs::DEFAULT` yields `true` — that is the source of the 31 `should not be enumerable`
> messages), and `delete_element` sets a hole without shrinking `length`, which is correct
> for a define but grows `element_len` on an empty array.
>
> **Separate genuine spec bug found in passing:** the data-descriptor branch at an index does
> **zero validation**, so `Object.defineProperty(arr, "0", {configurable: true})` on an
> existing non-configurable index accessor is *accepted* when it must throw. The blanket
> accessor reject was accidentally masking this. Relaxing the arm surfaces 8 such regressions.
> Fixworthy on its own merits and much cheaper than the accessor work.
>
> **Decision:** ~57 tests does not justify that much surgery into the array element-store
> fast paths. Re-filed here as a characterized task. If someone wants a slice, the 31-test
> enumerable/configurable attribute-modelling group is the cheapest self-contained one.
> Full record: `conformance/fix-log.md` 2026-09-27 "Task 2: STOPPED".

**Files:**
- Modify: `crates/v12-heap/src/shape.rs` (transition validation), `crates/v12-interp/src/property.rs` (array index `[[DefineOwnProperty]]` / `[[Set]]` path)
- Test: `crates/v12-engine/tests/array_index_accessor.rs` (create)

**Interfaces:**
- Consumes: existing `dense_bound` helper (`crates/v12-engine/src/builtins/array.rs:303`) — unchanged by this task.
- Produces: no new public API. Fix is a validation change inside the array-index property path.

**Root cause (reproduced):** an array that carries an **accessor** at an index can never
take another index property again.

```js
var arr = [0, , 2];
Object.defineProperty(arr, "0", { get: function() { return 0; }, configurable: true });
Array.prototype[1] = 1;   // TypeError: Cannot redefine property: Invalid property definition
```

Both statements succeed in isolation; only the combination throws. 317 tests fail this
way (203 in `built-ins/Array`, 111 in `built-ins/Object`). The array's indexed accessor
is stored as a shape descriptor, and the next index write takes a transition that
re-validates every index descriptor as if it were a data property, so the accessor fails
the data-property check and the transition is rejected.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/array_index_accessor.rs`:

```rust
//! An array carrying an indexed accessor must still accept later index
//! properties (ECMA-262 10.4.5 / 10.1.6). Regression: the shape transition
//! re-validated the accessor as a data property and threw
//! "Cannot redefine property: Invalid property definition".

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn index_write_after_index_accessor_on_the_array() {
    assert_eq!(
        run("var arr = [0, , 2]; \
             Object.defineProperty(arr, '0', { get: function() { return 0; }, configurable: true }); \
             Array.prototype[1] = 1; \
             throw 'ok';"),
        "ok"
    );
}

#[test]
fn accessor_at_index_survives_a_later_sibling_accessor() {
    assert_eq!(
        run("var arr = [0, 1, 2]; \
             Object.defineProperty(arr, '0', { get: function() { return 7; }, configurable: true }); \
             Object.defineProperty(arr, '1', { get: function() { return 8; }, configurable: true }); \
             throw [arr[0], arr[1], arr.length].join(',');"),
        "7,8,3"
    );
}

#[test]
fn every_skips_the_index_the_accessor_deleted_from_the_prototype() {
    // built-ins/Array/prototype/every/15.4.4.16-7-b-11.js
    assert_eq!(
        run("var accessed = false; \
             function cb(v, i) { accessed = true; return i !== 1; } \
             var arr = [0, , 2]; \
             Object.defineProperty(arr, '0', { \
               get: function() { delete Array.prototype[1]; return 0; }, \
               configurable: true }); \
             Array.prototype[1] = 1; \
             throw [arr.every(cb), accessed].join(',');"),
        "true,true"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine array_index_accessor`
Expected: FAIL — `Cannot redefine property: Invalid property definition`.

- [ ] **Step 3: Locate the rejecting validation**

Run:
```bash
grep -rn "Invalid property definition" crates/
```
Read the arm it sits in. Confirm the hypothesis: the arm validates an existing index
descriptor with a data-property-only check and rejects an accessor. Note whether the
check is in the shape-transition path (`crates/v12-heap/src/shape.rs`) or the interpreter
property path (`crates/v12-interp/src/property.rs`).

- [ ] **Step 4: Relax the validation to the spec rule**

Per ECMA-262 10.1.6 `[[DefineOwnProperty]]` step 3: when a property already exists, a
requested change is permitted if the existing descriptor is **configurable**, or if every
attribute in `Desc` matches the existing one. The fix is to accept a descriptor whose
`configurable` is `true` (or whose attributes are unchanged) instead of demanding that
the existing descriptor be a data property. Concretely, in the rejecting arm, replace the
data-only precondition with the spec's `IsCompatiblePropertyDescriptor(extensible, Desc,
current)` check — if that helper does not exist in `crates/v12-heap/src/shape.rs`, add it
with this exact signature and semantics:

```rust
/// ECMA-262 6.2.6.5 IsCompatiblePropertyDescriptor, reduced to the question the
/// array-index transition path asks: may `desc` replace `current`?
pub fn is_compatible_property_descriptor(
    extensible: bool,
    desc_configurable: bool,
    desc_is_accessor: bool,
    desc_writable: Option<bool>,
    desc_value: bool,
    current_configurable: bool,
    current_writable: bool,
    current_is_accessor: bool,
) -> bool {
    // A configurable property may be redefined in any way.
    if current_configurable {
        return true;
    }
    // A non-configurable property on a non-extensible object is frozen.
    if !extensible {
        return false;
    }
    // Making a non-configurable property configurable is never allowed.
    if desc_configurable {
        return false;
    }
    // A data property may not become an accessor, or the reverse.
    if desc_is_accessor != current_is_accessor {
        return false;
    }
    // Accessors: only a change of get/set matters, and the caller compares the
    // function identities; the attributes are already fixed.
    if current_is_accessor {
        return true;
    }
    // Non-writable data property: only a no-op is allowed.
    if !current_writable {
        if desc_writable == Some(false) {
            return true;
        }
        if desc_writable == Some(true) || desc_value {
            return false;
        }
    }
    true
}
```

Call it from the arm found in Step 3. Do not change the `[[DefineOwnProperty]]` semantics
for non-configurable, non-writable data properties — that must still throw.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine array_index_accessor`
Expected: PASS (3/3).

- [ ] **Step 6: Check for regressions and measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/Array --jobs 0
./conformance/run.sh --filter built-ins/Object --jobs 0
```
Expected: nextest green. `Array` 1 234 → ~1 437+; `Object` 2 334 → ~2 445+. If either
topic *drops*, diff the fail sets before committing — a pass-rate gain that hides new
failures is a regression, not a fix.

- [ ] **Step 7: Append the fix-log entry and commit**

```bash
git add crates/v12-heap/src/shape.rs crates/v12-interp/src/property.rs \
        crates/v12-engine/tests/array_index_accessor.rs conformance/fix-log.md
git commit -m "fix: an indexed accessor no longer poisons later array shape transitions"
```

---

### Task 3: `ToPropertyDescriptor` validation for non-object descriptors

**Files:**
- Modify: `crates/v12-engine/src/builtins/object.rs` (`create`, `defineProperties`, `defineProperty` descriptor readers)
- Test: `crates/v12-engine/tests/property_descriptor_validation.rs` (create)

**Interfaces:**
- Consumes: `Ctx::type_error` (the established error channel).
- Produces: no new public API.

**Root cause (reproduced):** a non-object property descriptor is accepted instead of
throwing `TypeError`.

```js
Object.create({}, 5);            // → object, spec says TypeError
Object.defineProperty({}, 'x', 5); // → likewise accepted
```

103 tests in `built-ins/Object/create` alone report `Property description must be an
object` — the engine *has* that message, so a code path raises it and the main one does
not. The `ToPropertyDescriptor` step (spec 19.1.1.5, 10.1.6.3) must reject any argument
that is not an Object before reading `.enumerable`/`.configurable`/`.value`/`.get`/`.set`.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/property_descriptor_validation.rs`:

```rust
//! `ToPropertyDescriptor` must reject non-Object arguments (spec 19.1.1.5 /
//! 10.1.6.3). Regression: Object.create / Object.defineProperty accepted them.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn create_rejects_a_primitive_descriptor() {
    for desc in ["5", "'x'", "true", "null", "undefined", "Symbol('s')"] {
        let out = run(&format!("try {{ Object.create({{}}, {}); throw 'no-throw'; }} \
                                catch (e) {{ throw e instanceof TypeError ? 'TypeError' : 'other'; }}", desc));
        assert_eq!(out, "TypeError", "descriptor {} must be rejected", desc);
    }
}

#[test]
fn define_property_rejects_a_primitive_descriptor() {
    assert_eq!(
        run("try { Object.defineProperty({}, 'x', 5); throw 'no-throw'; } \
             catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"),
        "TypeError"
    );
}

#[test]
fn create_with_a_null_prototype_and_valid_descriptor_still_works() {
    assert_eq!(
        run("var o = Object.create(null, { a: { value: 1, enumerable: true } }); \
             throw [o.a, Object.getPrototypeOf(o) === null].join(',');"),
        "1,true"
    );
}

#[test]
fn missing_descriptor_fields_default_per_spec() {
    // Absent fields default to false / undefined, and an accessor descriptor
    // with neither get nor set has both undefined.
    assert_eq!(
        run("var o = {}; Object.defineProperty(o, 'x', {}); \
             var d = Object.getOwnPropertyDescriptor(o, 'x'); \
             throw [d.value, d.get, d.set, d.writable, d.enumerable, d.configurable].join(',');"),
        ",,,,,false,false"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine property_descriptor_validation`
Expected: the first two FAIL (`no-throw`), the last two PASS.

- [ ] **Step 3: Locate every descriptor read site**

Run:
```bash
grep -rn "Property description must be an object" crates/
grep -n "fn .*descriptor\|fn to_property_descriptor" crates/v12-engine/src/builtins/object.rs
```
The existing raise site is the reference for the correct behaviour. Every other reader of
a user-supplied descriptor must route through one shared `to_property_descriptor` helper.

- [ ] **Step 4: Add the shared helper and route the readers through it**

In `crates/v12-engine/src/builtins/object.rs` add:

```rust
/// ToPropertyDescriptor (spec 19.1.1.5). Every reader of a user-supplied
/// descriptor must go through this: a non-Object argument is a TypeError, and
/// absent fields take the spec defaults (false / undefined) rather than being
/// read off the primitive.
pub(crate) fn to_property_descriptor(
    ctx: &mut Ctx,
    value: JsValue,
) -> Result<PropertyDescriptor, Throw> {
    if !ctx.is_object(value) {
        return Err(ctx.type_error("Property description must be an object"));
    }
    let mut desc = PropertyDescriptor::default();
    if ctx.has_property(value, "enumerable") {
        desc.enumerable = Some(ctx.to_boolean(ctx.get(value, "enumerable")?));
    }
    if ctx.has_property(value, "configurable") {
        desc.configurable = Some(ctx.to_boolean(ctx.get(value, "configurable")?));
    }
    if ctx.has_property(value, "value") {
        desc.value = Some(ctx.get(value, "value")?);
    }
    if ctx.has_property(value, "writable") {
        desc.writable = Some(ctx.to_boolean(ctx.get(value, "writable")?));
    }
    if ctx.has_property(value, "get") {
        let g = ctx.get(value, "get")?;
        if !ctx.is_callable(g) && !g.is_undefined() {
            return Err(ctx.type_error("Getter must be a function"));
        }
        desc.get = Some(g);
    }
    if ctx.has_property(value, "set") {
        let s = ctx.get(value, "set")?;
        if !ctx.is_callable(s) && !s.is_undefined() {
            return Err(ctx.type_error("Setter must be a function"));
        }
        desc.set = Some(s);
    }
    Ok(desc)
}
```

Adapt the names to the helpers that actually exist on `Ctx` in
`crates/v12-engine/src/builtins/ctx.rs` — if `has_property` does not exist, use the
equivalent own-or-inherited lookup the file already uses elsewhere. Reuse the
`PropertyDescriptor` type already defined in this module; do not introduce a second one.
Then replace the ad-hoc reads in `create`, `define_properties`, and `define_property`
with calls to this helper.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine property_descriptor_validation`
Expected: PASS (4/4).

- [ ] **Step 6: Check for regressions and measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/Object --jobs 0
```
Expected: nextest green; `Object` 2 334 → ~2 437+ with no new failures.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/object.rs \
        crates/v12-engine/tests/property_descriptor_validation.rs conformance/fix-log.md
git commit -m "builtins: reject non-object property descriptors in ToPropertyDescriptor"
```

---

### Task 4: Lone surrogates in `String.fromCodePoint`

**Files:**
- Modify: `crates/v12-engine/src/builtins/string.rs:955-972` (`string_from_code_point`)
- Test: `crates/v12-engine/tests/lone_surrogates.rs` (create)

**Interfaces:**
- Consumes: `V12Str::utf16(Vec<u16>)` (`crates/v12-heap/src/string.rs:196`) and `Heap::intern_string(V12Str) -> Handle<V12Str>` (`crates/v12-heap/src/gc.rs:1337`).
- Produces: no new public API. Uses existing UTF-16 string storage — no representation change.

**Root cause (reproduced):** `String.fromCodePoint` rejects every surrogate code point.

```js
String.fromCodePoint(0xDC00);   // RangeError: Invalid code point
```

Spec 22.1.3.15 requires only the range check `0 ≤ cp ≤ 0x10FFFF`; `0xDC00` is valid and
the result is a one-code-unit string holding U+DC00. The implementation converts with
`char::from_u32` (`string.rs:966`), and Rust's `char` excludes 0xD800–0xDFFF, so it
returns `None` and the function raises. **422 tests** fail this way, virtually all of them
generated RegExp `\d`/`\w`/`\s` tests whose `buildString` helper feeds surrogate ranges
through `String.fromCodePoint.apply`. The heap already has a UTF-16 storage variant, so
the fix is local — there is no need to change the string representation.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/lone_surrogates.rs`:

```rust
//! `String.fromCodePoint` must accept surrogate code points (spec 22.1.3.15:
//! the only precondition is the 0..=0x10FFFF range). Regression: the
//! implementation used `char::from_u32`, which cannot represent a surrogate.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

fn units(source: &str) -> Vec<u16> {
    // Report code units as hex so lone surrogates survive the test channel.
    let js = format!(
        "var s = {}; var out = []; \
         for (var i = 0; i < s.length; i++) {{ \
           out.push(s.charCodeAt(i).toString(16)); }} \
         throw out.join(' ');",
        source
    );
    let mut engine = Engine::new();
    match engine.eval(&js) {
        Ok(_) => panic!("expected the throw channel to carry the code units"),
        Err(t) => engine
            .to_display_string(t)
            .split_whitespace()
            // Tolerate any quoting the display channel adds around the value.
            .map(|w| w.trim_matches(|c: char| !c.is_ascii_hexdigit()))
            .map(|h| u16::from_str_radix(h, 16).expect("hex code unit"))
            .collect(),
    }
}

#[test]
fn high_lone_surrogate_is_accepted() {
    assert_eq!(units("String.fromCodePoint(0xD800)"), vec![0xD800]);
}

#[test]
fn low_lone_surrogate_is_accepted() {
    assert_eq!(units("String.fromCodePoint(0xDC00)"), vec![0xDC00]);
}

#[test]
fn range_boundaries_still_hold() {
    assert_eq!(units("String.fromCodePoint(0x10FFFF)"), vec![0xDBFF, 0xDFFF]);
    assert_eq!(
        run("try { String.fromCodePoint(0x110000); throw 'no-throw'; } \
             catch (e) { throw e instanceof RangeError ? 'RangeError' : 'other'; }"),
        "RangeError"
    );
    assert_eq!(
        run("try { String.fromCodePoint(-1); throw 'no-throw'; } \
             catch (e) { throw e instanceof RangeError ? 'RangeError' : 'other'; }"),
        "RangeError"
    );
}

#[test]
fn non_integers_are_still_rejected() {
    assert_eq!(
        run("try { String.fromCodePoint(1.5); throw 'no-throw'; } \
             catch (e) { throw e instanceof RangeError ? 'RangeError' : 'other'; }"),
        "RangeError"
    );
}

#[test]
fn many_surrogates_survive_a_large_apply() {
    // The shape the generated RegExp tests use.
    assert_eq!(
        run("var a = []; for (var i = 0; i < 10000; i++) { a[i] = 0xDC00 + (i % 1024); } \
             var s = String.fromCodePoint.apply(null, a); \
             throw s.length;"),
        "10000"
    );
}

#[test]
fn astral_pair_is_two_units_not_two_surrogates() {
    // A well-formed pair must still encode as a real astral char, i.e. the
    // surrogate-pair path must not regress while the lone-surrogate path is added.
    assert_eq!(units("String.fromCodePoint(0x1F600)"), vec![0xD83D, 0xDE00]);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine lone_surrogates`
Expected: `high_lone_surrogate_is_accepted` and `low_lone_surrogate_is_accepted` FAIL with
`RangeError: Invalid code point`; the rest PASS.

- [ ] **Step 3: Rewrite the conversion to build UTF-16 code units**

Replace the body of `string_from_code_point` in
`crates/v12-engine/src/builtins/string.rs:955-972` with:

```rust
pub fn string_from_code_point(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let mut units: Vec<u16> = Vec::with_capacity(args.len());
    for &v in args {
        let n = ctx.to_number(v);
        // Spec 22.1.3.15 step 3: the only precondition is the range check.
        // Surrogates are valid code points and must round-trip, so the result
        // is built as UTF-16 code units rather than Rust `char`s.
        if n.fract() != 0.0 || n < 0.0 || n > 0x10FFFF as f64 {
            return Err(ctx.range_error("Invalid code point"));
        }
        let cp = n as u32;
        if cp <= 0xFFFF {
            units.push(cp as u16);
        } else {
            let c = cp - 0x1_0000;
            units.push(0xD800 + ((c >> 10) as u16));
            units.push(0xDC00 + ((c & 0x3FF) as u16));
        }
    }
    Ok(JsValue::string(ctx.heap.intern_string(V12Str::utf16(units))))
}
```

Use the actual `Ctx` range-error constructor (grep `fn range_error` in
`crates/v12-engine/src/builtins/ctx.rs`; if only `type_error` exists, add
`range_error` alongside it following the same shape) and import `V12Str` at the top of
`string.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine lone_surrogates`
Expected: PASS (6/6).

- [ ] **Step 5: Verify the round-trip through the other UTF-16 entry points**

`fromCharCode` already produced UTF-16 storage, so it is the regression risk. Confirm
the two paths agree:

Run: `cargo nextest run -p v12-engine string_construct`
Expected: PASS. If it fails, the `flatten`/`intern` path needs to treat a UTF-16 string
containing lone surrogates as already-flattened rather than trying to re-encode it —
see `Heap::flatten` at `crates/v12-heap/src/string.rs`.

- [ ] **Step 6: Check for regressions and measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/RegExp --jobs 0
./conformance/run.sh --filter built-ins/String --jobs 0
```
Expected: nextest green; `RegExp` 801 → ~1 200+; `String` unchanged or better.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/string.rs \
        crates/v12-engine/tests/lone_surrogates.rs conformance/fix-log.md
git commit -m "builtins: String.fromCodePoint accepts surrogate code points"
```

---

### Task 5: Install the `Iterator` global and its prototype helpers

**Files:**
- Modify: `crates/v12-engine/src/builtins/mod.rs` (`GLOBAL_INTRINSICS` region + `define_builtins!` invocation near `:795`), `crates/v12-engine/src/realm.rs`, `crates/v12-engine/src/builtins/iterator.rs`
- Modify: `crates/v12-native/src/id.rs` (new ids in the free `2920-2999` block)
- Test: `crates/v12-engine/tests/iterator_surface.rs` (create)

**Interfaces:**
- Consumes: `Ctx::define_method(target, name, f, Some(arity))` (`ctx.rs:492`), `Ctx::install_ctor_link` (`ctx.rs:723`), `define_builtins!` (`mod.rs:746`).
- Produces: global `Iterator` with prototype `Iterator.prototype` carrying `map`, `filter`, `take`, `drop`, `flatMap`, `reduce`, `toArray`, `forEach`, `some`, `every`, `find`; each returns an `Iterator` wrapping the source iterator.

**Root cause (reproduced):** `ReferenceError: Iterator is not defined` on 410 tests, and
125 more fail with `callee is not a function` because the prototype helpers are absent.
Confirmed absent: `Iterator`, `Iterator.prototype.map/filter/take/drop/flatMap/reduce/
toArray/forEach/some/every/find`, and `AsyncIterator.prototype.map/filter`.

`Iterator` is a global in the intrinsic table; adding it means extending the single-copy
intrinsic list in `crates/v12-bytecode` (`GLOBAL_INTRINSICS`) and its compiler superset
`GLOBAL_ACCESS_INTRINSICS`, with `GLOBAL_VAR_OFFSET` derived from the former — do not
hand-maintain the offset.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/iterator_surface.rs`:

```rust
//! The `Iterator` global (ES2025 iterator helpers) and its prototype methods.
//! Regression: the global was never installed, so 410 tests raised
//! `ReferenceError: Iterator is not defined` and 125 more called a missing helper.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn iterator_is_a_global_with_a_prototype() {
    assert_eq!(
        run("throw [typeof Iterator, typeof Iterator.prototype, \
             Object.getPrototypeOf(Iterator.prototype) === Object.prototype].join(',');"),
        "function,object,true"
    );
}

#[test]
fn every_helper_is_installed_on_the_prototype() {
    assert_eq!(
        run("var names = ['map','filter','take','drop','flatMap','reduce','toArray', \
                    'forEach','some','every','find']; \
             var missing = names.filter(function(n) { return typeof Iterator.prototype[n]; }); \
             throw missing.length;"),
        "undefined"
    );
}

#[test]
fn map_and_toArray_are_lazy_then_realized() {
    assert_eq!(
        run("var out = [1,2,3].values().map(function(v) { return v * 2; }).toArray(); \
             throw out.join(',');"),
        "2,4,6"
    );
}

#[test]
fn take_and_drop_bound_the_iteration() {
    assert_eq!(
        run("throw [[1,2,3,4,5].values().take(2).toArray().join(','), \
                   [1,2,3,4,5].values().drop(3).toArray().join(',')].join('|');"),
        "1,2|4,5"
    );
}

#[test]
fn filter_and_find_agree_with_the_array_methods() {
    assert_eq!(
        run("var it = [1,2,3,4].values().filter(function(v) { return v % 2 === 0; }); \
             throw [it.toArray().join(','), \
                   [1,2,3,4].values().find(function(v) { return v > 2; })].join('|');"),
        "2,4|3"
    );
}

#[test]
fn helpers_are_lazy_and_brand_checked() {
    assert_eq!(
        run("var calls = 0; \
             var it = [1,2,3].values().map(function(v) { calls++; return v; }); \
             var before = calls; it.toArray(); \
             throw [before, calls].join(',');"),
        "0,3"
    );
    assert_eq!(
        run("try { Iterator.prototype.map.call({}, function(){}); throw 'no-throw'; } \
             catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"),
        "TypeError"
    );
}

#[test]
fn helper_length_and_name_are_spec_values() {
    assert_eq!(
        run("throw [Iterator.prototype.map.length, Iterator.prototype.take.length, \
                   Iterator.prototype.reduce.length, Iterator.prototype.map.name, \
                   Iterator.prototype.take.name].join(',');"),
        "1,1,1,map,take"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine iterator_surface`
Expected: FAIL — `ReferenceError: Iterator is not defined`.

- [ ] **Step 3: Register the global**

In `crates/v12-bytecode`, append `"Iterator"` to `GLOBAL_INTRINSICS` and to
`GLOBAL_ACCESS_INTRINSICS` if the compiler superset does not already cover it. Confirm
`GLOBAL_VAR_OFFSET` is derived from `GLOBAL_INTRINSICS.len()` and not hardcoded:

```bash
grep -n "GLOBAL_VAR_OFFSET\|GLOBAL_INTRINSICS\|GLOBAL_ACCESS_INTRINSICS" crates/v12-bytecode/src/lib.rs
```

Then install it in `crates/v12-engine/src/realm.rs` next to the other
non-`define_builtins!` globals (`Iterator` has no native body — its constructor throws,
per spec 27.1.2 — so it needs a small throwing body, not a `define_builtins!` entry):

```rust
// Iterator (spec 27.1.2): the constructor is abstract and always throws.
// The prototype carries the iterator-helper methods.
let iter_proto = ctx.alloc_object(ObjectKind::Ordinary);
ctx.install_ctor_link(ctx.global(), "Iterator", iter_proto, 0, Some(0));
```

- [ ] **Step 4: Implement the helpers**

In `crates/v12-engine/src/builtins/iterator.rs`, add one generic driver — every helper is
"pull from the source iterator, test, emit" — and keep each helper a two-line wrapper:

```rust
/// Drives an iterator-helper method. `step` returns `None` when the source is
/// exhausted. `emit` turns an accepted element into the helper's result: a
/// value for the value-producing helpers, or `Some(())` to mark the element
/// consumed without producing output (`filter`, `some`, `every`, `find`,
/// `forEach`). Lazy: nothing runs until the result iterator is pulled.
fn drive_iterator_helper(
    ctx: &mut Ctx,
    source: JsValue,
    mut step: impl FnMut(&mut Ctx, JsValue) -> Result<Option<JsValue>, Throw>,
    mut emit: impl FnMut(&mut Ctx, &mut Vec<JsValue>, JsValue) -> Result<bool, Throw>,
    eager: bool,
) -> Result<JsValue, Throw> {
    if !ctx.is_object(source) || !ctx.has_property(source, "next") {
        return Err(ctx.type_error("Iterator helper called on a non-iterator"));
    }
    let mut out: Vec<JsValue> = Vec::new();
    loop {
        let Some(value) = step(ctx, source)? else { break };
        if !emit(ctx, &mut out, value)? {
            break;
        }
    }
    Ok(if eager {
        // forEach / some / every / find / reduce return a value, not an iterator.
        out.pop().unwrap_or(JsValue::UNDEFINED)
    } else {
        ctx.make_iterator_from_vec(out)
    })
}
```

`make_iterator_from_vec` does not exist yet — add it to `iterator.rs` next to the
existing array-iterator wrapper (the file already builds Array/Map/Set iterators; reuse
its iterator-object constructor rather than writing a new one). `take`/`drop` also need a
remaining-count, so give `step` the closure the job needs rather than hardcoding a
bound.

Register each helper through `define_builtins!` (`mod.rs:795`) with a `call_ctx` body and
a `builtin_length` entry, matching how the last five builtin commits did it. Arity: every
helper is `(1)`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine iterator_surface`
Expected: PASS (7/7).

- [ ] **Step 6: Check for regressions and measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/Iterator --jobs 0
./conformance/run.sh --filter built-ins/ArrayIteratorPrototype --jobs 0
```
Expected: nextest green; `Iterator` 14 → several hundred. Array and Map/Set iterator
tests must not regress — the new prototype becomes the ancestor of the array iterator
prototype, so a wrong `[[Prototype]]` link shows up here.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-bytecode/src/lib.rs crates/v12-native/src/id.rs \
        crates/v12-engine/src/builtins/mod.rs crates/v12-engine/src/builtins/realm.rs \
        crates/v12-engine/src/builtins/iterator.rs \
        crates/v12-engine/tests/iterator_surface.rs conformance/fix-log.md
git commit -m "builtins: install the Iterator global and its prototype helpers"
```

---

### Task 6: Materialize the `RegExp.prototype` methods

**Files:**
- Modify: `crates/v12-engine/src/builtins/mod.rs` (the `RegExpProto` macro arm that installs to `None`), `crates/v12-engine/src/builtins/realm.rs`, `crates/v12-engine/src/builtins/regexp.rs`
- Test: `crates/v12-engine/tests/regexp_prototype.rs` (create)

**Interfaces:**
- Consumes: `install_string_regex_methods` (`mod.rs:418`) and the existing `RegExpExec`/`RegExpTest` NativeIds (dispatch-only today).
- Produces: `RegExp.prototype` gains own, non-enumerable methods `exec`, `test`, `toString`, `compile`, and the six `Symbol.*` methods.

**Root cause (reproduced):** `RegExp.prototype` has no methods at all.

```js
Object.getOwnPropertyNames(RegExp.prototype);   // → ['constructor']
typeof RegExp.prototype.exec;                    // → 'undefined'
typeof /a/.exec;                                // → 'function'
```

Instances resolve `exec` through a hardcoded fast path, so ordinary code works while
every spec-shaped access fails: `RegExp.prototype.exec.call(re, s)`, `delete
RegExp.prototype.exec`, `'exec' in RegExp.prototype`,
`Object.getOwnPropertyDescriptor(RegExp.prototype, 'exec')`. That is the `callee is not a
function` cluster (RegExp 116) plus the 75 `Cannot read properties of undefined (reading
'get')` legacy-accessor failures — roughly 300 tests. The `RegExpProto` arm of
`define_builtins!` installs to `None` (`mod.rs:510`), which is the bug in one line.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/regexp_prototype.rs`:

```rust
//! `RegExp.prototype` must own its methods (spec 22.2.6). Regression: the
//! prototype installed zero methods and instances resolved them through a
//! hardcoded fast path, so every prototype-shaped access failed.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn prototype_owns_the_spec_methods() {
    assert_eq!(
        run("var want = ['exec','test','toString']; \
             var missing = want.filter(function(n) { \
               return !Object.getOwnPropertyNames(RegExp.prototype).includes(n); }); \
             throw missing.length;"),
        "undefined"
    );
}

#[test]
fn methods_are_non_enumerable_and_have_the_right_length() {
    assert_eq!(
        run("var d = Object.getOwnPropertyDescriptor(RegExp.prototype, 'exec'); \
             throw [d.enumerable, d.writable, d.configurable, RegExp.prototype.exec.length, \
                    RegExp.prototype.exec.name].join(',');"),
        "false,true,true,1,exec"
    );
}

#[test]
fn exec_is_callable_with_an_explicit_receiver() {
    assert_eq!(
        run("var re = /a+/; \
             throw ['xaaay'.replace, RegExp.prototype.exec.call(re, 'xaaay')[0]].join(',');"),
        "function,aaa"
    );
}

#[test]
fn test_uses_the_brand_check() {
    assert_eq!(
        run("var re = /a/; \
             throw [RegExp.prototype.test.call(re, 'a'), \
                    RegExp.prototype.test.call(re, 'b')].join(',');"),
        "true,false"
    );
    assert_eq!(
        run("try { RegExp.prototype.exec.call({}, ''); throw 'no-throw'; } \
             catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"),
        "TypeError"
    );
}

#[test]
fn toString_reflects_source_and_flags() {
    assert_eq!(
        run("throw RegExp.prototype.toString.call(/ab+c/gi);"),
        "/ab+c/gi"
    );
}

#[test]
fn symbol_methods_are_present_and_symbol_keyed() {
    assert_eq!(
        run("var syms = [Symbol.match, Symbol.matchAll, Symbol.replace, \
                        Symbol.search, Symbol.split]; \
             var missing = syms.filter(function(s) { \
               return typeof RegExp.prototype[s] === 'undefined'; }); \
             throw missing.length;"),
        "undefined"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine regexp_prototype`
Expected: FAIL — `RegExp.prototype.exec` is `undefined`.

- [ ] **Step 3: Fix the install arm**

Read `crates/v12-engine/src/builtins/mod.rs` around the `RegExpProto` arm (reported at
`:510`) and the `install_string_regex_methods` helper (`:418`). The arm passes `None` as
the install target, so every RegExp method id is compiled into the dispatch table but
never installed anywhere. Change the arm to install onto the RegExp prototype object, and
confirm `realm.rs` allocates that prototype and links it to the constructor before
`install_builtins` runs.

Install `exec` and `test` as ordinary methods whose bodies call the existing
`RegExpExec`/`RegExpTest` bodies in `regexp.rs` (they exist — they are only reachable via
the fast path today). Extract that shared logic into one `fn regexp_exec_impl(ctx, re,
str) -> Result<JsValue, Throw>` and have both the fast path and the installed method call
it, so there is exactly one implementation (DRY).

Give every method a `builtin_length` entry: `exec`/`test` are `(1)`, `toString` is `(0)`,
`compile` is `(2)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine regexp_prototype`
Expected: PASS (6/6).

- [ ] **Step 5: Keep the fast path consistent**

The instance fast path must now agree with the installed method, or the same operation
would give two answers. Verify the fast path still returns a live `RegExp` for
`lastIndex` writes:

```bash
cargo nextest run -p v12-engine
./conformance/run.sh --filter built-ins/RegExp/prototype --jobs 0
```
Expected: PASS; `RegExp/prototype` improves with no new failures.

- [ ] **Step 6: Measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/RegExp --jobs 0
./conformance/run.sh --filter annexB/built-ins/RegExp --jobs 0
```
Expected: `RegExp` 801 → ~1 100+; annexB RegExp 123 → higher.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/mod.rs crates/v12-engine/src/builtins/realm.rs \
        crates/v12-engine/src/builtins/regexp.rs \
        crates/v12-engine/tests/regexp_prototype.rs conformance/fix-log.md
git commit -m "builtins: install RegExp.prototype methods instead of fast-pathing them"
```

---

### Task 7: Array ES2023 change methods and the Set operations

**Files:**
- Modify: `crates/v12-engine/src/builtins/array.rs`, `crates/v12-engine/src/builtins/map.rs` (Set half), `crates/v12-engine/src/builtins/mod.rs`, `crates/v12-engine/src/builtins/realm.rs`
- Test: `crates/v12-engine/tests/array_change_methods.rs`, `crates/v12-engine/tests/set_operations.rs` (both create)

**Interfaces:**
- Consumes: the Set storage in `map.rs` (live entries, iteration order) and the existing `map`/`filter` callback seam in `call_setup.rs`.
- Produces: `Array.prototype.toReversed`, `toSorted`, `toSpliced`, `with`, `toLocaleString`; `Set.prototype.union`, `intersection`, `difference`, `symmetricDifference`, `isSubsetOf`, `isSupersetOf`, `isDisjointFrom`.

**Root cause (reproduced):** all nine methods are simply absent. They account for the
102 `callee is not a function` failures in `built-ins/Set` and a large share of the
`built-ins/Array` gap (`Array.prototype` currently ends at `flatMap`).

- [ ] **Step 1: Write the failing tests**

Create `crates/v12-engine/tests/array_change_methods.rs`:

```rust
//! Array.prototype change methods (spec 23.1.3.28-31): the copy-and-return
//! family that does not mutate the receiver.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn to_reversed_copies() {
    assert_eq!(
        run("var a = [1,2,3]; var r = a.toReversed(); \
             throw [r.join(','), a.join(','), r === a].join('|');"),
        "3,2,1|1,2,3|false"
    );
}

#[test]
fn to_sorted_copies_and_accepts_a_comparator() {
    assert_eq!(
        run("var a = [3,1,2]; \
             throw [a.toSorted().join(','), a.toSorted(function(x,y){return y-x;}).join(','), \
                    a.join(',')].join('|');"),
        "1,2,3|3,2,1|3,1,2"
    );
}

#[test]
fn to_spliced_and_with_are_index_precise() {
    assert_eq!(
        run("var a = [1,2,3,4]; \
             throw [a.toSpliced(1,2,'x').join(','), a.toSpliced(1,0).join(','), \
                    a.with(1,'y').join(','), a.join(',')].join('|');"),
        "1,x,4|1,2,3,4|1,y,3,4|1,2,3,4"
    );
}

#[test]
fn out_of_range_indices_throw_and_ignore_extra_args() {
    assert_eq!(
        run("try { [1].with(5, 0); throw 'no-throw'; } \
             catch (e) { throw e instanceof RangeError ? 'RangeError' : 'other'; }"),
        "RangeError"
    );
    assert_eq!(
        run("var a = [1,2,3]; throw a.toSpliced(1, 1, 'a', 'b', 'c').join(',');"),
        "1,a,b,c,3"
    );
}

#[test]
fn holes_are_preserved_as_undefined_not_dropped() {
    assert_eq!(
        run("var a = [1, , 3]; var r = a.toReversed(); \
             throw [r.length, 1 in r, String(r[1])].join(',');"),
        "3,true,"
    );
}
```

Create `crates/v12-engine/tests/set_operations.rs`:

```rust
//! Set.prototype set operations (spec 24.2.4): all return a new Set and none
//! mutate their receiver.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

fn sorted(set_expr: &str) -> String {
    run(&format!("var s = {}; throw Array.from(s).sort().join(',');", set_expr))
}

#[test]
fn union_intersection_difference() {
    assert_eq!(sorted("new Set([1,2]).union(new Set([2,3]))"), "1,2,3");
    assert_eq!(sorted("new Set([1,2]).intersection(new Set([2,3]))"), "2");
    assert_eq!(sorted("new Set([1,2]).difference(new Set([2]))"), "1");
    assert_eq!(sorted("new Set([1,2]).symmetricDifference(new Set([2,3]))"), "1,3");
}

#[test]
fn relational_predicates() {
    assert_eq!(run("throw new Set([1]).isSubsetOf(new Set([1,2]));"), "true");
    assert_eq!(run("throw new Set([1,2]).isSupersetOf(new Set([1]));"), "true");
    assert_eq!(run("throw new Set([1]).isDisjointFrom(new Set([2]));"), "true");
    assert_eq!(run("throw new Set([1]).isDisjointFrom(new Set([1]));"), "false");
}

#[test]
fn receivers_are_not_mutated_and_equality_is_set_semantics() {
    assert_eq!(
        run("var a = new Set([1,2]); var b = new Set([2,3]); a.union(b); \
             throw [a.size, b.size, new Set([1,1,2]).size].join(',');"),
        "2,2,2"
    );
}

#[test]
fn operations_reject_a_non_set_receiver() {
    assert_eq!(
        run("try { Set.prototype.union.call(new Map(), new Set()); throw 'no-throw'; } \
             catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"),
        "TypeError"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p v12-engine array_change_methods set_operations`
Expected: FAIL — the methods are `undefined`.

- [ ] **Step 3: Implement the Array methods**

In `crates/v12-engine/src/builtins/array.rs`, implement `to_reversed`, `to_sorted`,
`to_spliced`, `with`, and `to_locale_string` as pure reads of the receiver plus a fresh
array. `to_sorted` must use the same comparison and the same `SortCompare` throw
semantics as the existing `sort` — extract that into a shared `fn sort_compare(ctx, x, y)
-> Result<Ordering, Throw>` and call it from both, so the two cannot drift (DRY). `with`
must throw `RangeError` when `index >= len` or `index < 0` after `ToIntegerOrInfinity`.

- [ ] **Step 4: Implement the Set operations**

In `crates/v12-engine/src/builtins/map.rs`, the Set operations all reduce to: snapshot the
receiver's entries into a `Vec<JsValue>`, do the same for the argument set, then apply
SameValueZero membership. Write one driver and seven thin wrappers, matching the
`drive_iterator_helper` shape from Task 5:

```rust
/// Shared engine for the seven Set operations. `mode` selects the combination;
/// `predicate` selects the relational ones. Both read through the Set's live
/// storage so a set mutated by a comparison callback cannot be observed
/// mid-iteration.
fn set_operation(ctx: &mut Ctx, this: JsValue, other: JsValue, mode: SetOpMode)
    -> Result<JsValue, Throw>
```

Register all twelve methods through `define_builtins!` with `builtin_length` entries:
`toReversed`/`toSorted` are `(1)`, `toSpliced`/`with` are `(2)`, `toLocaleString` is `(0)`,
and every Set operation is `(1)`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine array_change_methods set_operations`
Expected: PASS (5/5 and 4/4).

- [ ] **Step 6: Check for regressions and measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/Array --jobs 0
./conformance/run.sh --filter built-ins/Set --jobs 0
```
Expected: nextest green; `Set` 210 → ~300+; `Array` improved with no new failures.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/array.rs crates/v12-engine/src/builtins/map.rs \
        crates/v12-engine/src/builtins/mod.rs crates/v12-engine/src/builtins/realm.rs \
        crates/v12-engine/tests/array_change_methods.rs \
        crates/v12-engine/tests/set_operations.rs conformance/fix-log.md
git commit -m "builtins: add Array change methods and Set set-operations"
```

---

### Task 8: Install `length` on every builtin that lacks it

**Files:**
- Modify: `crates/v12-engine/src/builtins/mod.rs` (the `builtin_length` table, `:117-372`)
- Test: `crates/v12-engine/tests/builtin_length.rs` (create)

**Interfaces:**
- Consumes: `builtin_length(NativeId) -> Option<u8>` (`mod.rs:117`), consumed by `install_native` (`mod.rs:398`).
- Produces: a `builtin_length` entry for every id reached by `define_builtins!`; a new unit test that fails the build if any installed id is missing one.

**Root cause:** 81 of 358 NativeIds have no `builtin_length` entry, so
`Ctx::define_method` (`ctx.rs:492`) skips the `length` install — the function has **no**
`length` property at all. Affected families: every `*Construct` id (all constructors),
`Function`/`FunctionApply`/`FunctionCall`/`FunctionBind`, `Eval`, `ProxyConstruct`,
`ModuleImport`, `RegExpExec`/`Test`/`Compile`, `PromiseConstruct`, all 20 `Iterator*`
helpers, all 13 `SymbolWellKnown*`, and all 17 `*Const*` values. The
`debug_assert_eq!(builtin_length(id), Some(len))` at `ctx.rs:514` only fires when an arity
*is* passed, so the gap is silent.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/builtin_length.rs`:

```rust
//! Every installed builtin function must expose a `length` (spec 27.2.3 /
//! Function `length`). Regression: 81 NativeIds had no `builtin_length` entry,
//! so `define_method` skipped the property entirely and `hasOwnProperty('length')`
//! was false.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn constructors_expose_length() {
    assert_eq!(
        run("var ctor = [Array, Object, Function, Date, RegExp, Map, Set, Promise, \
                           Symbol, Error, TypeError, Proxy, Reflect, AggregateError]; \
             var bad = ctor.filter(function(c) { \
               return !c.hasOwnProperty('length') || typeof c.length !== 'number'; }); \
             throw bad.map(function(c) { return c.name; }).join(',') || 'ok';"),
        "ok"
    );
}

#[test]
fn constructors_expose_the_spec_arity() {
    assert_eq!(
        run("throw [Array.length, Object.length, Function.length, Date.length, \
                   RegExp.length, Map.length, Set.length, Promise.length, \
                   Symbol.length, Error.length, TypeError.length, Proxy.length, \
                   Reflect.length, AggregateError.length].join(',');"),
        "1,1,1,7,2,0,0,1,0,1,1,2,1,2"
    );
}

#[test]
fn function_prototype_methods_expose_length() {
    assert_eq!(
        run("throw [Function.prototype.call.length, Function.prototype.apply.length, \
                   Function.prototype.bind.length].join(',');"),
        "1,2,1"
    );
}

#[test]
fn regexp_methods_expose_length() {
    assert_eq!(
        run("throw [RegExp.prototype.exec.length, RegExp.prototype.test.length, \
                   RegExp.prototype.compile.length].join(',');"),
        "1,1,2"
    );
}

#[test]
fn iterator_helpers_expose_length() {
    assert_eq!(
        run("var names = ['map','filter','take','drop','flatMap','reduce','toArray', \
                            'forEach','some','every','find']; \
             var bad = names.filter(function(n) { \
               return Iterator.prototype[n].length !== 1; }); \
             throw bad.join(',') || 'ok';"),
        "ok"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine builtin_length`
Expected: FAIL — constructors report no `length`.

- [ ] **Step 3: Add the missing entries**

Cross-check the table against the install list mechanically rather than by eye:

```bash
grep -oE "^\s+[A-Za-z0-9_]+ =>" crates/v12-engine/src/builtins/mod.rs | tr -d ' =>' | sort -u > /tmp/ids_installed
grep -oE "[A-Za-z0-9_]+" crates/v12-native/src/id.rs | sort -u > /tmp/ids_all
```

For every id in the `define_builtins!` invocation (`:795`) that is absent from the
`builtin_length` table, add an entry with the arity from ECMA-262. The values above in
Step 1 are the spec arities — copy them, do not derive them from the Rust signature.
Pure values (`*Const*`, `SymbolWellKnown*`) get no entry: they are not functions, and
`install_value` does not call `define_method`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine builtin_length`
Expected: PASS (5/5).

- [ ] **Step 5: Make the gap impossible to reintroduce**

`Ctx::define_method` only asserts when an arity is supplied, so add a coverage guard
alongside the existing `assert_own_data_value` (`ctx.rs:694`). In the `debug_assert` at
`ctx.rs:514`, change the condition so a *missing* table entry is also a failure for any
id that reaches an install:

```rust
// A function install must declare its arity: without an entry the `length`
// property is silently absent, which is a conformance failure, not a default.
debug_assert!(
    builtin_length(id).is_some(),
    "NativeId {id:?} installs a function with no builtin_length entry"
);
```

Then verify the debug build is clean across the whole suite — this is the check that
catches a missed id:

```bash
cargo nextest run --workspace
cargo nextest run -p v12-engine --profile dev
```
Expected: all green with no `debug_assert` firing.

- [ ] **Step 6: Measure**

```bash
./conformance/run.sh --filter built-ins/Function --jobs 0
./conformance/run.sh --filter built-ins/Iterator --jobs 0
```
Expected: `Function` 245 → ~280+; `Iterator` improved; no regressions elsewhere.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/mod.rs crates/v12-engine/src/builtins/ctx.rs \
        crates/v12-engine/tests/builtin_length.rs conformance/fix-log.md
git commit -m "builtins: install length on every function and add a coverage assert"
```

---

### Task 9: Array literals longer than 255 elements

**Files:**
- Modify: `crates/v12-bccompiler/src/expr.rs:621-627`, `crates/v12-bccompiler/src/model.rs:1327-1332`, `crates/v12-bytecode/src/lib.rs` (new wide opcode)
- Test: `crates/v12-bccompiler/tests/long_array_literal.rs` (create)

**Interfaces:**
- Consumes: `emit_new_array(dst: u16, base: u16, len: u8, span)` (`model.rs:1330`).
- Produces: `Opcode::NewArrayW` (disc 15, width 4) carrying a `u16` length, plus `emit_new_array_w(dst: u16, base: u16, len: u16, span)`. `NewArray` keeps its `u8` form for literals that fit.

**Root cause (reproduced):** the compiler rejects the program.

```js
var a = [/* 256 elements */];
// SyntaxError-ish: threw: array literals above 255 elements are not supported
```

`expr.rs:624` converts the element count with `u8::try_from` and errors out; `model.rs:1330`
documents that the `u8` length is a hard limit of the instruction encoding. 34 tests fail
this way (31 in `built-ins/RegExp`, all generated `regexp-generator` cases with big
literal tables), and the same limit will block any later test that needs a large literal.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-bccompiler/tests/long_array_literal.rs`:

```rust
//! Array literals above 255 elements must compile. Regression: the element
//! count was narrowed to `u8` and the compiler raised
//! "array literals above 255 elements are not supported".

#[test]
fn literal_of_256_elements_compiles() {
    let src = format!("var a = [{}];", vec!["0"; 256].join(","));
    assert!(
        v12_bccompiler::compile_source(&src, "test.js").is_ok(),
        "a 256-element literal must compile"
    );
}

#[test]
fn literal_of_1000_elements_compiles() {
    let src = format!("var a = [{}];", vec!["0"; 1000].join(","));
    assert!(v12_bccompiler::compile_source(&src, "test.js").is_ok());
}

#[test]
fn narrow_opcode_is_still_used_when_it_fits() {
    let src = format!("var a = [{}];", vec!["0"; 10].join(","));
    let unit = v12_bccompiler::compile_source(&src, "test.js").expect("compiles");
    let has_narrow = unit
        .code
        .iter()
        .any(|i| i.opcode() == v12_bytecode::Opcode::NewArray);
    let has_wide = unit
        .code
        .iter()
        .any(|i| i.opcode() == v12_bytecode::Opcode::NewArrayW);
    assert!(has_narrow, "10 elements must use the narrow NewArray");
    assert!(!has_wide, "10 elements must not need the wide form");
}
```

Adapt `compile_source` to the real entry point in `crates/v12-bccompiler/src/lib.rs`
(grep for the public compile function) and the unit field that holds the instruction
stream. The existing `v12-bytecode` decode sweep in the nextest suite already covers
opcode widths, so the new opcode is validated there too.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-bccompiler long_array_literal`
Expected: FAIL — compile returns the "above 255 elements" error.

- [ ] **Step 3: Add the wide opcode**

In `crates/v12-bytecode/src/lib.rs`, add `NewArrayW` as disc 15 with width 4 (disc 11-14
are the existing wide ops — take the next free disc, and update the decode sweep's width
table in the same edit). Add to the `WideOps` match so the decoder reads a `u16` for the
length field.

In `crates/v12-bccompiler/src/model.rs`, add:

```rust
/// `NewArrayW dst, base, len` — the same operation as [`Self::emit_new_array`]
/// with a 16-bit length, for literals that do not fit the narrow encoding's
/// `u8` count. `dst`/`base` may escape through `RegExt`.
pub fn emit_new_array_w(&mut self, dst: u16, base: u16, len: u16, span: oxc_span::Span) {
    self.emit_regs(Opcode::NewArrayW, dst, base, len, 0b0011, span);
}
```

In `crates/v12-bccompiler/src/expr.rs:621-627`, branch on the count:

```rust
        if !has_spread {
            let base_reg = self.new_temps_for(elems.len());
            for (i, el) in elems.iter().enumerate() { /* unchanged per-element lowering */ }
            let n = elems.len();
            if let Ok(narrow) = u8::try_from(n) {
                self.emit_new_array(dst, base_reg, narrow, span);
            } else {
                let wide = u16::try_from(n).map_err(|_| {
                    self.err(span, "array literals above 65535 elements are not supported")
                })?;
                self.emit_new_array_w(dst, base_reg, wide, span);
            }
        }
```

Preserve the existing spread path untouched.

- [ ] **Step 4: Add the interpreter arm**

In `crates/v12-interp/src/execute.rs`, add the `NewArrayW` arm beside `NewArray`. Do not
duplicate the array-construction body — call the same helper `NewArray` uses, passing the
`u16` length widened to whatever the helper takes. Reusing the helper is what keeps the
two encodings from diverging (DRY).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-bccompiler long_array_literal -p v12-bytecode`
Expected: PASS, and the bytecode decode sweep still green.

- [ ] **Step 6: Check for regressions and measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/RegExp --jobs 0
```
Expected: nextest green; the 31 `array literals above N elements` failures gone.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-bytecode/src/lib.rs crates/v12-bccompiler/src/expr.rs \
        crates/v12-bccompiler/src/model.rs crates/v12-interp/src/execute.rs \
        crates/v12-bccompiler/tests/long_array_literal.rs conformance/fix-log.md
git commit -m "compiler: NewArrayW for array literals above 255 elements"
```

---

### Task 10: `Number.prototype` brand checks are over-strict

**Files:**
- Modify: `crates/v12-engine/src/builtins/number.rs`
- Test: `crates/v12-engine/tests/number_brand_checks.rs` (create)

**Interfaces:**
- Consumes: `Ctx::this_number(value) -> Result<f64, Throw>` (or the equivalent helper the file already uses).
- Produces: no new public API.

**Root cause:** 37 tests fail with
`TypeError: Number.prototype.toString requires that 'this' be a Number`. The check is
correct in rejecting a primitive `this` from sloppy mode, but it is also rejecting cases
the spec accepts — most visibly a boxed `Number` reachable through a subclass or a
cross-realm path, where the engine's brand test does not recognise the object.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/number_brand_checks.rs`:

```rust
//! Number.prototype methods must accept every receiver the spec accepts and
//! reject the rest. Regression: the brand check rejected some legitimate
//! receivers with "requires that 'this' be a Number".

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn boxed_receiver_is_accepted() {
    assert_eq!(run("throw Number.prototype.toString.call(new Number(7));"), "7");
    assert_eq!(run("throw Number.prototype.valueOf.call(new Number(7));"), "7");
    assert_eq!(run("throw Number.prototype.toFixed.call(new Number(1.5), 1);"), "1.5");
}

#[test]
fn subclass_receiver_is_accepted() {
    assert_eq!(
        run("class N extends Number {}; throw Number.prototype.toString.call(new N(3));"),
        "3"
    );
}

#[test]
fn cross_realm_receiver_is_accepted() {
    assert_eq!(
        run("var other = $262.createRealm(); \
             throw Number.prototype.toString.call(new other.Number(5));"),
        "5"
    );
}

#[test]
fn non_number_receiver_is_rejected() {
    assert_eq!(
        run("try { Number.prototype.valueOf.call({}); throw 'no-throw'; } \
             catch (e) { throw e instanceof TypeError ? 'TypeError' : 'other'; }"),
        "TypeError"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine number_brand_checks`
Expected: at least the subclass and cross-realm cases FAIL.

- [ ] **Step 3: Fix the brand check**

Find the throw site:

```bash
grep -rn "requires that 'this' be a Number" crates/v12-engine/src/builtins/number.rs
```

Replace the narrow kind comparison with the spec's `thisNumberValue` (spec 6.1.6.1.20):
unbox a `Number` exotic object regardless of which realm or subclass allocated it, and
require a primitive `this` to have come from a strict-mode caller. If the check is a
`Kind::Number` tag comparison, switch it to the heap's number-unwrap helper (grep
`fn as_number` / `number_value` in `crates/v12-heap/src`) so a subclass instance and a
cross-realm instance both unwrap. Apply the same helper to `to_fixed`,
`to_precision`, `to_exponential`, and `value_of` — they must agree.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine number_brand_checks`
Expected: PASS (4/4).

- [ ] **Step 5: Measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/Number --jobs 0
```
Expected: `Number` 210 → ~240+; no regressions.

- [ ] **Step 6: Commit**

```bash
git add crates/v12-engine/src/builtins/number.rs \
        crates/v12-engine/tests/number_brand_checks.rs conformance/fix-log.md
git commit -m "builtins: accept every spec-valid receiver in Number.prototype brand checks"
```

---

### Task 11: `escape` and `unescape`

**Files:**
- Modify: `crates/v12-engine/src/builtins/global.rs` (bodies), `crates/v12-engine/src/builtins/mod.rs` (install + `builtin_length`), `crates/v12-native/src/id.rs` (two ids)
- Test: `crates/v12-engine/tests/escape_unescape.rs` (create)

**Interfaces:**
- Consumes: `define_builtins!` + `builtin_length` + `Ctx::define_method` — the standard path.
- Produces: globals `escape` (arity 1) and `unescape` (arity 1). Annex B, so they are
  non-enumerable? No — they are ordinary writable global properties.

**Root cause:** both globals are absent; 27 `annexB` tests report
`ReferenceError: escape is not defined` / `unescape is not defined`.

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/escape_unescape.rs`:

```rust
//! The Annex B `escape` / `unescape` globals (spec B.2.1.1, B.2.1.2).

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn escape_covers_the_six_escape_sets() {
    assert_eq!(
        run("throw [escape('a'), escape('@'), escape('*'), escape('+'), \
                   escape('-'), escape('/'), escape(' ')].join('|');"),
        "a|%40|*|+|-|/|%20"
    );
    assert_eq!(run("throw escape('\\u00E9');"), "%E9");
    assert_eq!(run("throw escape('\\uD83D\\uDE00');"), "%uD83D%uDE00");
}

#[test]
fn unescape_is_the_inverse() {
    assert_eq!(run("throw unescape('%41%20%2F');"), "A /");
    assert_eq!(run("throw unescape('%uD83D%uDE00');"), "\\uD83D\\uDE00");
    assert_eq!(run("throw unescape('abc');"), "abc");
}

#[test]
fn globals_are_writable_configurable_functions() {
    assert_eq!(
        run("var d = Object.getOwnPropertyDescriptor(globalThis, 'escape'); \
             throw [typeof escape, escape.length, d.writable, d.enumerable, \
                    d.configurable].join(',');"),
        "function,1,true,false,true"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine escape_unescape`
Expected: FAIL — `escape` is not defined.

- [ ] **Step 3: Implement both**

Add two ids in the free `2920-2999` block of `crates/v12-native/src/id.rs`
(`Escape`, `Unescape`), two `builtin_length` entries of `1`, and two `define_builtins!`
arms. Implement the bodies in `global.rs` next to the `encodeURI`/`decodeURI` bodies
(Annex B lives with them): `escape` keeps `A-Z a-z 0-9 @ * _ + - . /` and maps everything
else to `%XX` or `%uXXXX`; `unescape` decodes both forms and leaves anything else alone.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine escape_unescape`
Expected: PASS (3/3).

- [ ] **Step 5: Measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter annexB/built-ins --jobs 0
```
Expected: annexB 123 → ~150+; no regressions.

- [ ] **Step 6: Commit**

```bash
git add crates/v12-native/src/id.rs crates/v12-engine/src/builtins/mod.rs \
        crates/v12-engine/src/builtins/global.rs \
        crates/v12-engine/tests/escape_unescape.rs conformance/fix-log.md
git commit -m "builtins: add the Annex B escape and unescape globals"
```

---

### Task 12: `String.prototype[Symbol.iterator]`

**Files:**
- Modify: `crates/v12-engine/src/builtins/mod.rs`, `crates/v12-engine/src/builtins/realm.rs`, `crates/v12-engine/src/builtins/string.rs`, `crates/v12-engine/src/builtins/iterator.rs`
- Test: `crates/v12-engine/tests/string_iterator.rs` (create)

**Interfaces:**
- Consumes: the string-iterator constructor in `iterator.rs` and `Symbol::iterator` from
  `realm.rs`.
- Produces: `String.prototype[Symbol.iterator]` yielding `{ value, done }` code points,
  plus a `StringIteratorPrototype` with the `Iterator.prototype` helpers as its prototype.

**Root cause (reproduced):** `for...of` over a string works only because the compiler
special-cases it; the protocol method itself is missing.

```js
typeof ''[Symbol.iterator];                        // → 'undefined'
Object.getOwnPropertyNames(String.prototype);      // no '@@iterator'
```

Any spec-shaped access fails, and it breaks the general iterable path for strings
(spread, `Array.from`, destructuring, `String.prototype[Symbol.iterator]` lookups in
harness code).

- [ ] **Step 1: Write the failing test**

Create `crates/v12-engine/tests/string_iterator.rs`:

```rust
//! `String.prototype[Symbol.iterator]` (spec 22.1.3.16) and the string iterator
//! prototype. Regression: for-of worked only through a compiler special case;
//! the protocol method was absent.

use v12_engine::Engine;

fn run(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

#[test]
fn the_method_exists_and_is_an_accessor_returning_a_function() {
    assert_eq!(
        run("var d = Object.getOwnPropertyDescriptor(String.prototype, Symbol.iterator); \
             throw [typeof d.get, typeof d.get.call(''), d.enumerable, d.configurable] \
                    .join(',');"),
        "function,function,false,true"
    );
}

#[test]
fn it_iterates_code_points_not_code_units() {
    assert_eq!(
        run("var out = []; for (var c of 'a\\u{1F600}b') { out.push(c.codePointAt(0).toString(16)); } \
             throw out.join(',');"),
        "61,1f600,62"
    );
}

#[test]
fn it_works_with_spread_and_array_from() {
    assert_eq!(
        run("var s = 'a\\u{1F600}b'; \
             throw [[...s].length, Array.from(s).length, s.length].join(',');"),
        "3,3,4"
    );
}

#[test]
fn it_supports_the_iterator_helpers_from_task_5() {
    assert_eq!(
        run("throw Array.from('abc'.values().toArray()).join(',');"),
        "a,b,c"
    );
}

#[test]
fn empty_string_yields_nothing() {
    assert_eq!(run("var n = 0; for (var c of '') { n++; } throw n;"), "0");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p v12-engine string_iterator`
Expected: FAIL — `''[Symbol.iterator]` is `undefined`.

- [ ] **Step 3: Install the accessor and the iterator prototype**

In `realm.rs`, create a `StringIteratorPrototype` whose `[[Prototype]]` is
`Iterator.prototype` (from Task 5 — this is why Task 5 must land first), give it a
`Symbol.toStringTag` of `"String Iterator"`, and install an accessor on
`String.prototype` under `Symbol.iterator` whose getter returns a function that produces
a fresh string iterator over the receiver's code points. Register the accessor's getter
and setter functions through `define_builtins!` with `builtin_length` `0`.

Reuse the iterator-object constructor in `iterator.rs`; do not add a second one.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p v12-engine string_iterator`
Expected: PASS (5/5).

- [ ] **Step 5: Remove the compiler special case only if it is now redundant**

The compiler special-cases string iteration. Leave it in place: removing it is a
separate optimization with its own regression surface, and KISS says do not bundle it
into a conformance fix. Note it in `fix-log.md` as a follow-up.

- [ ] **Step 6: Measure**

```bash
cargo nextest run --workspace
./conformance/run.sh --filter built-ins/StringIteratorPrototype --jobs 0
./conformance/run.sh --filter language/statements/for-of --jobs 0
```
Expected: `StringIteratorPrototype` 0 → 7/7; for-of unaffected.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/mod.rs crates/v12-engine/src/builtins/realm.rs \
        crates/v12-engine/src/builtins/string.rs crates/v12-engine/src/builtins/iterator.rs \
        crates/v12-engine/tests/string_iterator.rs conformance/fix-log.md
git commit -m "builtins: install String.prototype[Symbol.iterator] and its iterator prototype"
```

---

## Final verification

After Task 12, the plan is complete when all of the following hold:

- [ ] `cargo nextest run --workspace` — green, 0 skipped, 0 failed.
- [ ] `cargo clippy --workspace --all-targets` — 0 errors.
- [ ] `cargo fmt --check` — clean.
- [ ] `./conformance/run.sh --filter built-ins --jobs 0 --format human,json --json-out /tmp/bi-final.json`
      — record the summary line in `fix-log.md`.
- [ ] Recompute core % from `/tmp/bi-final.json` and confirm it moved from **54.5 %** to
      **~67 %** (passes 8 314 → ~10 200 of 15 267).
- [ ] Confirm no deferred topic moved from `skip` to `pass` or back to `fail`.
- [ ] Update `CONTEXT.md` §Conformance with the new built-ins numbers and the core/raw
      metric definitions, so the next session does not re-derive them.

## Out of scope for this plan

Recorded so they are not re-opened silently. Each is a deliberate deferral, not a
regression:

- Temporal (4 603), the TypedArray family (2 718), `Atomics`/`SharedArrayBuffer` (332),
  the weak collections (302), explicit resource management (219), `BigInt` (75),
  `ShadowRealm` (64) — all gated by Task 1.
- `Proxy` `apply`/`construct` traps, `Intl`, `structuredClone`, `queueMicrotask`
  (id `1700` exists but is not installed — a one-line follow-up worth doing after Task 8).
- `AsyncIterator.prototype` helpers beyond `map`/`filter`.
- Removing the compiler's string-iteration special case (Task 12, Step 5).
- The ROADMAP §2 steps 4-6 (`Ctx` install-path collapse, single `dispatch_native`,
  hard-error lints). The `Ctx` migration itself is already effectively complete — 17 of
  21 builtin files are `Ctx`-shaped, so that section of the ROADMAP is stale and should
  be corrected rather than executed.
