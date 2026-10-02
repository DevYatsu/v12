# ADR-0004: One index/length reader for array-like receivers, routed through the internal methods

**Status:** Proposed
**Date:** 2026-10-02
**Deciders:** user (pending)
**Relates to:** `CONTEXT.md` ("Shape", "Dictionary mode", "Validity cell"); `docs/architecture-maintainability-plan.md` §4 (enforcement tier)
**Supersedes:** nothing
**Informed by:** ADR-0002 (Pass A exhaustiveness — the same lesson, one layer up: a silent skip in a pass nobody diffs is a shipped conformance defect); `CONTEXT.md`'s own standing warning that "an entry that says a bug is 'still open' is a claim with an as-of date"

---

## Context

### The finding

`Array.prototype` methods are **generic** in the spec: they operate on any object via
`LengthOfArrayLike` (`ToLength(Get(O, "length"))`) and then `HasProperty`/`Get`/`Set`/`Delete`
at integer keys. The spec has no element store for a generic receiver at all. v12 invented one.

Measured against `node` v24.21.0 on `b8d6ef17`, with a **numeric** `length` and all indices
**own** — receiver `{0:"a",1:"b",2:"c",length:3}`, `ret` is the method's return value, `obj` is the
receiver afterwards:

| method | node `ret` | node `obj` | v12 `ret` | v12 `obj` | |
|---|---|---|---|---|---|
| `fill` | `undefined` | `{0:"q",1:"q",2:"q",length:3}` | `undefined` | `{0:"a",1:"b",2:"c",length:3}` | ✗ |
| `copyWithin` | `undefined` | `{0:"a",1:"a",2:"b",length:3}` | `undefined` | `{0:"a",1:"b",2:"c",length:3}` | ✗ |
| `slice` | `["a","b","c"]` | — | `[]` | — | ✗ |
| `join` | `"a-b-c"` | — | `""` | — | ✗ |
| `sort` | `undefined` | `{0:"a",1:"b",2:"c",length:3}` | `undefined` | `{0:"c",1:"a",2:"b",length:3}` | ✗ |
| `reverse` | `undefined` | `{0:"c",1:"b",2:"a",length:3}` | `undefined` | `{0:"a",1:"b",2:"c",length:3}` | ✗ |
| `shift` | `"a"` | `{0:"b",1:"c",length:2}` | `"a"` | `{0:"a",1:"b",2:"c",length:3}` | ✗ |
| `unshift` | `4` | `{0:"z",1:"a",2:"b",3:"c",length:4}` | `1` | `{0:"a",1:"b",2:"c",length:3}` | ✗ |
| `splice` | `["b"]` | `{0:"a",1:"c",length:2}` | `[]` | `{0:"a",1:"b",2:"c",length:3}` | ✗ |
| `concat` | `[{…}]` | — | `[]` | — | ✗ |
| `toString` | `"[object Object]"` | — | `""` | — | ✗ † |
| `push` | `4` | `{0:"a",…,3:"d",length:4}` | `4` | `{0:"a",1:"b",2:"c",length:4}` | ✗ |
| `map` | `["a","b","c"]` | — | `[]` | — | ✗ ‡ |
| `forEach` | `["a","b","c"]` | — | `[]` | — | ✗ ‡ |
| `filter` | `["a","b","c"]` | — | `[]` | — | ✗ ‡ |
| `reduce` | `"-a-b-c"` | — | `""` | — | ✗ ‡ |
| `some` | `true` | — | **`false`** | — | ✗ ‡ |
| `pop` | `"c"` | `{0:"a",1:"b",length:2}` | `"c"` | `{0:"a",1:"b",length:2}`` | ✓ |
| `indexOf` | `1` | — | `1` | — | ✓ |
| `lastIndexOf` | `0` | — | `0` | — | ✓ |
| `includes` | `true` | — | `true` | — | ✓ |
| `at` | `"b"` | — | `"b"` | — | ✓ |
| `every` | `true` | — | `true` | — | ✓ (accidentally) |
| `Array.from` | `["a","b","c"]` | — | `["a","b","c"]` | — | ✓ |

**17 of 24 diverge, not 12 of 13.** The brief this ADR answers understated the blast radius in
three ways, all measured below: it omitted `sort`, `concat` and `toString`; it omitted the
**entire callback seam** (`map`/`forEach`/`filter`/`reduce`/`some`/`every` — 5 of the 17, in a
*different crate*); and its claim that `indexOf`/`lastIndexOf` "match" is true only for
shape-bound indices — §"Contradictions" shows they contradict v12's *own property read*.

† `toString` diverges for a **different reason**: node's `Array.prototype.toString` step 3 is
`let func = Get(O, "join"); if IsCallable(func) … else OrdinaryToPrimitive`. v12's `array_to_string`
(`array.rs:777`) calls `array_join` unconditionally, skipping the `join`-is-callable test. This is
an independent defect that the storage fix does **not** address; it is listed in Batch 1 because it
is a two-line change in the same file, and in "What this does not fix" because it is a different bug.

‡ The callback seam (`map`/`forEach`/`filter`/`reduce`/`some`/`every`) is
`Interp::run_callback_builtin` at `call_setup.rs:3025-3050`, a **sixth** copy of the
elements-vs-shape straddle in `v12-interp`.

`some` is the worst of the set and should be read as the headline: it returns `false` for a
receiver that demonstrably has three elements and a predicate that accepts everything. It is the
only case here where v12 asserts something *positive and wrong* about present data rather than
merely omitting data.

### Inherited indices: 6 of 6 diverge

Receiver `Object.create({0:"inh", length:3})` — the indices live on the prototype:

| | node | v12 |
|---|---|---|
| `indexOf(o,"inh")` | `0` | `-1` |
| `slice(o)` | `["inh",null,null]` | `[]` |
| `includes(o,"inh")` | `true` | `false` |
| `at(o,0)` | `"inh"` | `undefined` |
| `Array.from(o)` | `["inh",null,null]` | `[]` |
| `join(o,"-")` | `"inh--"` | `""` |

This is spec-correct in node: `Array.prototype.fill` and friends are defined over `HasProperty(O, Pk)`
and `Get(O, Pk)`, and `[[Get]]` walks the prototype chain. v12's hand-rolled readers do not.

### Arguments exotic: 2 more divergences, in `length` bookkeeping

The `arguments` object stores elements in the flat `elements` vec and is the one receiver where
`elements_snapshot()` is the *right* answer. It still diverges, because the length writers do not
cover `Kind::Arguments`:

| | node | v12 |
|---|---|---|
| `unshift` on `(1,2)` | `len=3`, `z,1,2` | `len=2`, `z,1,2` |
| `splice(1,1)` on `(1,2,3)` | `len=2`, `1,3` | `len=3`, `1,undefined` |

Cause: `array_unshift` (`array.rs:633`) guards its length write with `if kind == Array`, and
`set_array_len` (`array.rs:351`) only ever lowers `length` on `Kind::Array`.

### The finding is real, and the fix is smaller than the brief assumes

**The barrier that `array.rs` "cannot reach" `dispatch_get` does not exist.** All three facts,
read directly:

- `crates/v12-engine/src/lib.rs:9` — `pub mod internal_methods;`
- `crates/v12-engine/src/internal_methods.rs:1298` — `pub fn dispatch_get(…)`
- `crates/v12-engine/src/builtins/array.rs:8` — `use v12_heap::{…}`; same crate, same module tree.

There is no visibility modifier to change and no dependency to add. **Two files in the *same
directory* already call it:**

- `crates/v12-engine/src/builtins/object.rs:98` — `crate::internal_methods::dispatch_get(&mut *ctx.heap, obj, pk, JsValue::object(obj))`
- `crates/v12-engine/src/builtins/reflect.rs:655` — the same call inside `create_list_from_array_like`

`reflect.rs:644-673` is the **reference implementation of what every `Array.prototype` method
should do**: `dispatch_get` the `length`, `ToLength` it, then `dispatch_get` each index in
`0..len`. It is 25 lines, in the same crate, three files from `array.rs`, and it is correct.
`array.rs` re-implemented the same idea four times and got it wrong four times.

This is a **DRY** finding, not an architecture finding: the knowledge exists once and correctly;
the copies are the defect.

### The actual root cause: five index readers, five length readers

**Index readers** — five implementations of "what is at index `i` of `O`":

| # | reader | location | store order | proto chain | dict rung | hole-aware |
|---|---|---|---|---|---|---|
| 1 | `ordinary_get` | `internal_methods.rs:974` | shape → dict → **proto** | ✅ | ✅ | ❌ |
| 2 | `read_index` | `array.rs:264` | **`elements` first**, then own shape | ❌ | ❌ | ✅ |
| 3 | `elements_snapshot` | `object.rs:435` | flat vec / lattice | ❌ | ❌ | ✅ |
| 4 | `create_list_from_array_like` | `reflect.rs:651` | `dispatch_get` | ✅ | ✅ | ❌ |
| 5 | `callback_elem` | `call_setup.rs:2945` | **`elements` first**, then own shape | ❌ | ❌ | ✅ |

`read_index` inverts reader 1's precedence. That single inversion is the whole bug: a plain
object's integer indices are **shape-bound string properties**, so `read_index`'s `elements` probe
answers first and returns whatever `fill` last wrote into a store `ordinary_get` never consults.

**Length readers** — five implementations of "what is `O.length`":

| # | reader | location | coercion | consulted by |
|---|---|---|---|---|
| 1 | `array_length` | `array.rs:36` | ✅ `heap_to_integer_or_infinity` + `length_from` | `pop` only |
| 2 | `array_len` | `array.rs:324` | ❌ `as_smi().or(as_f64)` | `indexOf` `lastIndexOf` `includes` `at` `Array.from` `splice` |
| 3 | `array_push_fallback` | `object_ops.rs:812` | ❌ inline `as_smi().or(as_f64)` | `push` (the live one) |
| 4 | `array_join_fallback` | `object_ops.rs:780` | ❌ **none — ignores `length`** | `join`, `toString` |
| 5 | `callback_len` | `call_setup.rs:2923` | ❌ **`element_len()`, never reads `length`** | `map` `forEach` `filter` `reduce` `some` `every` |

Three of the five never see a `length` property at all. That is why `map` on a correct
`{0:"a",1:"b",2:"c",length:3}` visits nothing: `callback_len` returns `element_len()` = 0.

### Two router arms shadow the registry, and one of them makes a test lie

`dispatch_native` (`call_setup.rs:1569`) matches `NativeId::ArrayJoin` (`:1589`) and
`NativeId::ArrayPush` (`:1590`) explicitly, **before** the `_ =>` fallthrough that reaches
`run_callback_builtin` and then `call_native` (`:1874`). The registry entries at
`builtins/mod.rs:1328` (`array::array_push`) and `:1330` (`array_join`) are therefore
**unreachable from JavaScript**.

`array.rs::array_push` is not dead code, though, and this is why nobody noticed: it is called
**directly** by `crates/v12-engine/src/tests.rs:276`, on a **real `Array`**, asserting
`elements_array.len()`. So the one green test for `push` exercises the shadowed body on the one
receiver kind where the shadowing is invisible. The brief's "DEAD for push" is right about the
dispatch path and wrong about the function.

---

## Decision

**Delete the parallel readers. Route every array-like index and length read/write through
`internal_methods::dispatch_get` / `dispatch_set` / `dispatch_has` / `dispatch_delete` — the
functions that already exist, are already `pub`, and are already used from the same directory.**

No new abstraction is introduced. No new crate. No new trait. The change is *subtractive*: four
private helpers and one duplicated router arm go away, and eleven call sites change from
"consult a second storage model" to "ask the object".

### The four helpers (Batch 1, all in `crates/v12-engine/src/builtins/array.rs`)

```rust
/// ES `LengthOfArrayLike`: `ToLength(Get(O, "length"))`, walked through
/// [[Get]] so `Object.create({length:3})` works. Falls back to the element
/// store only for `Kind::Array` (an unbound array's store *is* its length).
fn array_like_len(heap: &mut Heap, obj: Handle<JsObject>) -> f64;

/// Index read as `(value, present)`. `present` is a separate
/// `dispatch_has` because `indexOf`/`fill`/`copyWithin` must distinguish a
/// hole from `undefined` — and because returning `JsValue::hole()` through a
/// property path is a category error.
fn index_get(heap: &mut Heap, obj: Handle<JsObject>, i: u32) -> (JsValue, bool);

/// Index write as `Set(O, ToString(i), v, O)`.
fn index_set(heap: &mut Heap, obj: Handle<JsObject>, i: u32, v: JsValue);

/// Index delete as `Delete(O, ToString(i))` — `pop`'s DeletePropertyOrThrow.
fn index_delete(heap: &mut Heap, obj: Handle<JsObject>, i: u32);
```

`index_get`'s two-value return is the one genuinely new thing in this ADR. `reflect.rs:655` cannot
use it because `CreateListFromArrayLike` has no hole semantics, but every `Array.prototype` method
does: `fill` writes through holes, `indexOf` skips them, `copyWithin` propagates them. No existing
reader in the tree can express that — `ordinary_get` collapses hole to `undefined`, and
`read_index`'s `Option` cannot distinguish "absent" from "present and `undefined`". Splitting the
question into `dispatch_has` + `dispatch_get` is what the spec's `HasProperty`/`Get` pair already
does.

### Why (a) and not the alternatives

| Option | Verdict | Reason |
|---|---|---|
| **(a) One reader on the real property path** | **Chosen** | The function already exists, is `pub`, and is already used from the same directory (`object.rs:98`, `reflect.rs:655`). Subtractive, not additive. Fixes the store split, the proto chain, and the coercion in one edit because all three are the same edit. Fails nothing. |
| **(b) Give array-likes a dense store** | Rejected | Doubles down on the invention that *is* the bug — the spec has no element store for a generic receiver. And it cannot work without inverting precedence: `read_index` reads `elements` first (`array.rs:268`), so materialising `{0:"a"}` into `elements` while the shape still binds `"0"` reproduces `z[0]==="shape0"` vs `slice(z)[0]==="F"` in the *opposite* direction. Fails *YAGNI* (a second representation for a case the first already covers) and *KISS* (it needs the very precedence inversion that caused the bug). |
| **(c) Length-aware `elements_snapshot`** | Rejected | Fixes reads, leaves writes invisible — so `slice` starts reporting `["a","b","c"]` while `fill` still writes into a store nothing reads. That is *more* divergence across *more* methods, each individually smaller and collectively harder to see: the headline symptom (`slice` returns `[]`) disappears, so the next bug report starts from a stranger place. It also adds a **sixth** length reader. Fails *fail-fast*: it defers the diagnosis instead of removing the cause. |
| **(d) Normalise at object creation** | Rejected | Would mean `{0:"a"}` and `Object.assign`-built objects land in `elements`, which re-introduces two representations for one value and breaks `Object.keys` ordering, spread, and `JSON.stringify` — all of which **currently match `node`** (measured: `Object.assign`, `{…o}`, `Object.keys`/`entries`/`values`, `JSON.stringify`, `String()`, `+o` are all correct today). It would trade a measured defect for five unmeasured ones. Fails *YAGNI*, and it fixes the symptom's *cause* only by making the engine less spec-shaped. |
| **(e) Fix the coercion only** | Rejected | See "Sequencing" below. It is net-negative for diagnosis. |

There is no "shared macro/trait" option worth considering: a trait over index access would abstract
over the one thing that must not be abstracted — whether the receiver has an element store at all.
`Kind::Array` is the only such kind, and it is already a one-line `if`.

### Sequencing: coercion alone is net-negative, and this is measurable

The brief asks whether coercion should land first. **It should not, and the harm is not
hypothetical.** Consider the numeric-length case, measured today on `b8d6ef17`:

```js
var o = {0:"a", length:3};
Array.prototype.fill.call(o, "q", 0, 3);
o[0]                       // v12: "a"    node: "q"
Array.prototype.indexOf.call(o, "q")   // v12: 0     node: 0
Array.prototype.slice.call(o)         // v12: ["q","q","q"]   node: ["q","q","q"]
```

**Every `Array.prototype` method agrees on `"q"`. The property read on the same object, same
index, same tick, says `"a"`.** v12 contradicts itself today, with no coercion bug in the picture
at all. `indexOf a` returns `-1` on an object whose index 0 demonstrably holds `"a"`.

Now apply **only** the coercion fix, to the string-length variant `{0:"a", length:"3"}` with the
same `fill` then `indexOf`:

- **Today:** `array_len` coerces `"3"` → `NaN` → `0`. So `fill` computes `len=0, end=0`, the loop
  is empty, nothing is written; `indexOf` computes `len=0`, `dense_bound` clamps to 0, returns
  `-1`. **One wrong answer, in one place.**
- **After coercion only:** `array_len` returns 3. `fill` writes `elements[0..3]`. `indexOf` now
  scans and finds `"q"` — **returns 0, matching `node`** — while `o[0]` is still `"a"`.

So the coercion-only state is *exactly* the state measured above for the numeric case: a
**newly-passing conformance test sitting on top of a still-broken object**. It converts "one method
is wrong" into "the method is right and the object lies", and it does so behind precisely the tests
that would otherwise gate the storage fix. That is *fail-fast* inverted.

Both fixes are one edit anyway: `fill`'s write range is derived from `len`, so you cannot widen the
range correctly while reading the wrong length. **Land them in one commit.**

### Concrete first batch (Batch 1)

One commit. `crates/v12-engine/src/builtins/array.rs` plus two arm deletions in `v12-interp`.

| Site | Change |
|---|---|
| `array.rs:264` `read_index` | **delete**; 5 call sites (`:447 :497 :543 :592 :813`) → `index_get` |
| `array.rs:287` `write_index` | **delete**; 3 call sites (`:382 :699 :739`) → `index_set` |
| `array.rs:296` `dense_bound` | **delete**; it was a correctness bug dressed as an optimisation — it silently narrows `fill`/`copyWithin`'s range. `MAX_WRITE_SPAN` (`array.rs:676`) stays as the *declared* OOM guard. |
| `array.rs:324` `array_len` | **delete**; 5 call sites → `array_like_len` |
| `array.rs:36` `array_length` | **delete**; `array_pop` (`:165`) → `array_like_len` |
| `array.rs:351` `set_array_len` | rewrite as a `dispatch_set` of `length`; fixes generic `length` never being lowered, and `Kind::Arguments` being skipped |
| `array.rs:123` `pop_delete` | → `index_delete` |
| `array.rs:189` `array_slice` | `elements_snapshot()` → `0..len` index loop |
| `array.rs:222` `array_sort` | same |
| `array.rs:567 :575` `array_concat` | same (both the receiver and each `Kind::Array` argument) |
| `array.rs:599` `array_reverse` | same, writing back via `index_set` |
| `array.rs:616` `array_shift` | same |
| `array.rs:629` `array_unshift` | same; **drop the `if kind == Array` guard at `:633`** |
| `array.rs:655` `array_splice` | same; drop the `elems.len()` clamps at `:658-659` |
| `array.rs:729` `array_copy_within` | same |
| `array.rs:760` `flatten_into` | same (recursion included) |
| `array.rs:777` `array_to_string` | add the `Get(O,"join")`-is-callable test (node returns `"[object Object]"`) |
| `object_ops.rs:780` `array_join_fallback` | **delete** |
| `object_ops.rs:812` `array_push_fallback` | **delete** |
| `call_setup.rs:1589-1590` | **delete both router arms** so `mod.rs:1328 :1330` become the live paths |

Net effect on the measured table: **17 → 0 numeric divergences**, `sort` included, plus the two
`arguments` length bugs, plus `toString`. `fill`'s `"q"` becomes readable through `o[0]`, closing
the self-contradiction.

**Verification (in order; step 1 is the point):**

1. **The self-consistency assertion**, as a new test in
   `crates/v12-engine/tests/array_generic_receivers.rs`. For a fixed corpus of array-likes
   (`{length:3}`; `{0:"a",1:"b",2:"c",length:3}`; `Object.create({0:"inh",length:3})`;
   `Object.create({length:3})`; `(function(){return arguments})(1,2,3)`) and every method in the
   table above, assert `receiver[i] === methodResult[i]` for all `i < ToLength(receiver)` **after**
   the call. This is the mutation-sensitive form: it fails today for 17 of 24 methods and cannot
   pass while any reader disagrees with `ordinary_get`. Mutation-check it — reintroducing the
   `elements`-first arm in `index_get` must fail at least one case.
2. **The node-agreement table**, as the same file's second test, transcribing the 24 rows above.
   Seven of the 24 pass today and must not regress; the other 17 must flip.
3. **`cargo nextest run --workspace --no-fail-fast`** — 1 273 passed / 2 skipped at `eaa495b5` is
   the gate. Expected churn: `crates/v12-engine/src/tests.rs:269` `array_push_and_pop_length`
   still exercises `array_push`, but on a **live** path now, so it becomes a real gate.
4. **`faildiff conformance/baseline/language.json` against a fresh run**, BROKE 0 / REGRESSED 0.
   Expect *gains* on `built-ins/Array` — but the honest expectation is that `language/` may move by
   a small negative, because some `language/` tests exercise array-likes incidentally and a
   previously-silent `elements` store was accidentally satisfying them. **Run it; do not assume.**

**Batches 2-3, each independently green, not started here:**

- **Batch 2 — the callback seam, `v12-interp`.** `callback_len` (`call_setup.rs:2923`) must read
  `length` via `get_property` (matching `create_list_from_array_like` at `:2115`, which already
  does); `callback_elem` (`:2945`) becomes `get_property(obj, index_key)`; `callback_dense_bound`
  (`:2964`) is deleted or reduced to the `MAX_WRITE_SPAN` guard. Fixes the 6 callback methods.
- **Batch 3.** `toSorted`/`toReversed`/`with`/`toSpliced` do not exist in v12 today (verified:
  `Array.prototype.toReversed` is `undefined`). Whatever lands must land on the new readers.

### Consequences

**Positive**

- One reader for indices, one for length, in one place, on the path the spec names.
- The prototype chain starts working for generic receivers for free — `dispatch_get` walks it at
  `internal_methods.rs:1034`. 6 of 6 inherited-index divergences close.
- The dictionary rung starts working for free — `dispatch_get` consults it at
  `internal_methods.rs:1002-1018`; `read_index` never did. (Measured: no receiver in this batch's
  corpus exposes this today, so this is robustness, not a claimed fix.)
- `MAX_WRITE_SPAN` becomes a *declared* deviation with a name, instead of `dense_bound`'s silent
  range-narrowing.
- The `Array.prototype.push` test at `tests.rs:276` stops testing a shadowed function.

**Negative**

- `fill`/`copyWithin` on a huge-`length` receiver still will not write billions of entries. This is
  a real, permanent deviation from the spec, inherited from `MAX_WRITE_SPAN`. It must be pinned by a
  named test so a future reader does not "fix" it into an OOM.
- `Array.prototype` methods on a **Proxy** receiver change from silently writing `elements` to
  **throwing `TypeError`**, because `dispatch_set` routes to `proxy_set`
  (`internal_methods.rs:1237`), which is an unimplemented-trap stub. That is *correct* (the spec
  requires a real `[[Set]]`), and honest, but it is a behaviour change and must be in the commit
  message.
- `index_get` is slower than `read_index` for the `Kind::Array` case: `dispatch_get` re-derives
  `kind_of` and re-checks the index-key canonicalisation on every probe. **Keep the existing
  `if kind == Array` fast path inside `index_get`** rather than removing it — measured arithmetic,
  not a style preference.
- Every future `Array.prototype` method must use the new helpers or reintroduce a sixth reader. No
  compiler guard prevents this (unlike ADR-0002, there is no exhaustiveness to exploit here). The
  mitigation is the Batch-1 test in step 1: it fails for *any* method that reintroduces a private
  reader, not just for the 24 listed.

---

## What this decision does **not** fix

Stated explicitly, because the temptation after landing it will be to call this closed.

- **The callback seam is untouched by Batch 1.** `map`, `forEach`, `filter`, `reduce`, `some`,
  `every` live in `v12-interp` (`call_setup.rs:3025-3050`) and keep their own reader pair
  (`callback_len`/`callback_elem` at `:2923`/`:2945`). **Batch 1 fixes 17 methods; the callback 6
  are Batch 2.** `some` returning `false` on `{0:"a",1:"b",2:"c",length:3}` is therefore
  **still wrong after Batch 1.**
- **`array_to_string`'s `join`-is-callable test** is included because it is in the same file and two
  lines. `Array.prototype.toLocaleString` and the general `OrdinaryToPrimitive` fallback are not.
- **`MAX_WRITE_SPAN`** (`array.rs:676`) stays. A permanent, named deviation.
- **`toSorted` / `toReversed` / `toSpliced` / `with`** do not exist in v12 (verified:
  `Array.prototype.toReversed` is `undefined`). Batch 3 is not a fix; it is a constraint on
  future work.
- **`elements` is not removed as a representation.** `Kind::Array` (`elements_array`) and
  `Kind::Arguments` (`elements`) both legitimately use it. Option (b) was rejected, not deferred.
- **`JSON.stringify` of an `arguments` object returns `{}`** (measured: node
  `{"0":"z","1":"z"}`, v12 `{}`, while `args[0]` and `args[1]` are both correctly `"z"`). That is a
  display/serialisation bug in `json.rs`, independent of the index readers, and Batch 1 does not
  address it.
- **Nothing here is gated.** `ci.yml` does not run the test suite (ADR-0002 §"What this does not
  fix" records this at `:32`). The 24-row test in verification step 2 is only a gate once W0.4
  lands `faildiff` as a CI step. Until then it is a manual check, and this ADR's real guarantee is
  the code change, not the test.

---

## Contradictions with the brief this ADR answers

Recorded because each one changes the size of the work, and the first three were verified by
running `node` and `target/debug/v12` side by side on `b8d6ef17`.

1. **"12 of 13 methods diverge" is an undercount: 17 of 24 measured.** The brief's table omits
   `sort` (diverges: node sorts, v12 no-ops), `concat` (diverges), `toString` (diverges for an
   unrelated reason), and the **entire callback seam** — `map`/`forEach`/`filter`/`reduce`/`some`,
   5 divergences in a *different crate* that the brief does not mention.
2. **"`indexOf`/`lastIndexOf` match" is true only for shape-bound indices, and is worse than a
   mismatch.** They match `node`, but they contradict **v12's own property read** on the same
   object: `fill.call({0:"a",length:3},"q",0,3)` then `indexOf(o,"q") === 0` while `o[0] === "a"`,
   and `indexOf(o,"a") === -1` on an object that has `"a"` at index 0. Any fix validated only by
   `indexOf` conformance tests will pass with the object still broken.
3. **"`array.rs::array_push` is DEAD for push" is right about the dispatch path and wrong about the
   function.** `dispatch_native` (`call_setup.rs:1590`) does intercept `NativeId::ArrayPush` before
   `call_native` (`:1874`), so `mod.rs:1328` is unreachable from JS. But
   `crates/v12-engine/src/tests.rs:276` calls `array::array_push` **directly**, on a **real
   `Array`**, and asserts on `elements_array`. It is live code with a green test — which is exactly
   why the shadowing survived.
4. **The barrier is a visibility modifier that does not exist.** `pub mod internal_methods`
   (`lib.rs:9`) + `pub fn dispatch_get` (`internal_methods.rs:1298`) + `builtins/object.rs:98` and
   `builtins/reflect.rs:655` already calling it. Nothing to un-block. The brief's framing
   ("`array.rs` cannot reach them", "the previous lane recommended" a rewrite of seven bodies
   against a new abstraction) overstates the work: this is hours, not days, because the correct
   implementation is already 25 lines at `reflect.rs:644-673`.
5. **"Seven `elements_snapshot()` bodies" is eleven** in `array.rs` (`:189 :222 :355 :567 :575
   :599 :616 :629 :655 :729 :760`), and there are 30 call sites across the workspace — though most
   of the other 19 are reached only with `Kind::Array` receivers and are correct as written.
6. **The `read_index` defect is not "straddles both".** `read_index` (`array.rs:264`) does not
   straddle — it *inverts* the precedence `ordinary_get` uses (`elements` first at `:268`,
   shape at `:281`). That inversion, not the straddling, is what makes writes invisible.

---

## Open questions for the decider

1. **Should Batch 1 and Batch 2 be one commit?** Recommendation: **no** — two crates, and Batch 1
   is independently verifiable. But Batch 1 alone leaves `some` returning `false` on a correct
   array-like, which is a bad state to publish. If you would rather not ship that intermediate, the
   honest alternative is to do Batch 1 + Batch 2 together and skip the `17 of 24` checkpoint.
2. **`fill`'s `MAX_WRITE_SPAN` deviation — accept permanently, or pin with a test?** Recommendation:
   accept, and pin with a named test asserting `fill.call({length:2**32},"q")` completes without
   materialising. It is the reason `dense_bound` existed; deleting `dense_bound` without this test
   invites someone to restore it.
3. **Should the Proxy behaviour change (silent-write → `TypeError`) land in this ADR, or with the
   real Proxy traps?** Recommendation: land it here, and say so in the commit message. A `TypeError`
   from an unimplemented trap is the correct honest failure and is what `internal_methods.rs:1215`
   was written to produce.