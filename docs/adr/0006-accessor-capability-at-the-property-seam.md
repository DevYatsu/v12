# ADR-0006: Accessor invocation belongs at the call seam; the engine's `[[Get]]` must lose the ability to answer

**Status:** Proposed
**Date:** 2026-10-05
**Deciders:** user (pending)
**Relates to:** `CONTEXT.md` ("Internal method", "Ordinary object", "Job queue", "Mutator"); ADR-0004 (the sixth index/length reader and the two router arms that shadow the registry); ADR-0002 (a silent catch-all is a shipped defect, not a style choice)
**Supersedes:** nothing
**Informed by:** the `E2_ACCESSOR_GETTER_UNREADABLE` row of `crates/v12-engine/tests/array_generic_receivers.rs:636-682`; the `callback_len` note at `crates/v12-interp/src/call_setup.rs:2953-2972`; the `dispatch_native` router contract at `crates/v12-interp/src/call_setup.rs:1561-1583`

---

## Context

### The defect

Reproduced (reported by the finding lane; **not re-verified against `node` by this
ADR's author** — no gate was run, see Verification):

```js
var o = { 0:"a", 1:"b", get length() { return 3 } };
o.length                       // 3    — interpreter [[Get]]: correct
Array.prototype.slice.call(o)  // length 0 — node says 3
```

98 of the 156 remaining node-differential divergences. Two prior lanes declined to
patch it. This ADR decides how to actually remove it.

### There are two `[[Get]]`s, and they have already diverged three times

This is the fact the brief understates, and it is the one that decides the ADR.

**The interpreter's `[[Get]]`** is `Interp::get_property`
(`crates/v12-interp/src/property.rs:120-266`): null/undefined rejection, key
flatten + intern, a Proxy arm (`:170`), a ModuleNamespace arm (`:173`), an
Arguments-`caller` arm (`:188`), the `Kind::Array`/`Arguments` element store
(`:210`), then **fifteen named structural surfaces** in a fixed order
(`:223-264`), falling through to `ic_lookup` (`:265`).

**The engine's `[[Get]]`** is `internal_methods::dispatch_get`
(`crates/v12-engine/src/internal_methods.rs:1427-1436`) → a kind table →
`ordinary_get` (`:1061-1140`): the element store, a guarded stub-cache fast path,
a dictionary rung, a shape rung, and a prototype walk.

**They share no code.** `v12-interp` contains zero calls to `dispatch_get` — the
only two occurrences of the string in that crate are doc comments
(`call_setup.rs:2953`, `object_ops.rs:839`). `callback_len` states the reason it
cannot share (`call_setup.rs:2959-2966`): `v12-interp` does not depend on
`v12-engine`, and adding it would invert the DAG, since `v12-engine` depends on
`v12-interp` (`crates/v12-engine/Cargo.toml:11`).

So E2 is not a bug in one function. It is **the third proven disagreement between
two implementations of one spec operation**:

| # | Disagreement | Where it is visible |
|---|---|---|
| 1 | A bytecode getter answers `undefined` | `internal_methods.rs:964-968`; 98 ledger rows |
| 2 | `receiver` is ignored — `this` binds to the *owning* object | `_receiver` is unused at `internal_methods.rs:1065` (get) and `:1147` (set); documented as a live bug at `regexp.rs:415-421`, where it makes `dispatch_get(heap, re, "source", re)` answer `"(?:)"` for every instance |
| 3 | Per-index attributes do not exist, so a non-writable element cannot be honoured | `array.rs:276-286` (explicit `CONCESSION`), which records that `ordinary_set` has the identical hole |

**This is the load-bearing context.** Any design that leaves `dispatch_get` as a
parallel implementation has to be argued against a file that already contains a
*named, documented, unfixed* second divergence — and the mechanism that produced
that second divergence is the very reimplementation this ADR is about. A design
that fixes #1 and leaves the module in place has not removed the class.

### The setter mirror is three sites, not one

The ledger records "`ordinary_set` has the mirror-image hole for a *setter*". It
is three, and the third is the worst:

| Site | Lines | Current answer |
|---|---|---|
| dictionary rung | `internal_methods.rs:1166-1172` | `if entry.setter.is_some() { return Ok(true) }` — write silently dropped; comment says "v1: setter invocation is a no-op beyond acknowledgement" |
| shape rung | `:1193-1201` | same, `let _ = value;` |
| **inherited** | `:1204-1211` | `Descriptor::Accessor { setter, .. } if setter.is_none() => return Ok(false)`, else `=> return Ok(true)` — a prototype accessor's setter is skipped and the write is **lost**, and `object_ops.rs`-side writes believe otherwise |

The inherited case makes the setter mirror *worse* than the getter defect rather
than symmetric to it: a dropped setter is an observable **write loss** on a
live test262 shape (`Array.prototype.fill` / `copyWithin` over an accessor-backed
index), whereas a dropped getter is an observable read loss. `dispatch_set` has
3 non-test call sites (`array.rs:293`, `array.rs:531`, `regexp.rs:1056`) —
the same magnitude as `dispatch_get`, so it costs no extra design.

### The deciding axis: can the getter be resolved *before* the heap boundary?

The hypothesis put to this ADR was that the real question is "does the heap-side
property path need call capability at all?", and that if the answer is no, a
fourth option exists that nobody has written down.

**The answer is yes, it needs call capability — and the "resolve it early" design
is circular, not merely absent.**

The circularity, stated exactly: `array_like_len` (`array.rs:150-172`) must
perform `[[Get]](O, "length")` in order to learn `len`; and `len` is what
determines the index key set `0..len` that `index_get` (`array.rs:220-243`) then
reads. If `length` is a getter, **the first read is itself the read that needs the
machine.** There is no key set to pre-resolve, because the key set is discovered
through the very operation that requires the capability. Any design of the shape
"resolve the accessors, then let the native run on plain data" therefore has to
run the getter to find out what to resolve — which is the thing it was trying to
avoid.

So the early-resolution option does not exist. Recorded here so that the next lane
does not spend a day on it.

**The reframing that does work.** The useful question is not *does the path need
capability* but *who is allowed to reach the path*. And there the answer is
already visible in the tree, in the one place that got it right:

> `Interp::dispatch_native` is documented as the "Single normalization point for
> every `NativeId` call… Everything else tries `run_callback_builtin`
> (**re-entrant, needs the machine**) before `NativeRegistry::call_native`."
> — `crates/v12-interp/src/call_setup.rs:1561-1566`

`run_callback_builtin` (`:1980-2050`+) already intercepts 13 `Array*` ids, plus
`ArrayJoin` and `ArrayPush` (with hand-written `*_fallback` bodies in
`object_ops.rs:802-838`, `:855-880`), plus `Map`/`Set` construction and
`forEach`, plus 9 Promise ids, plus `StringConstruct`/`NumberConstruct`. Those
bodies read `length` and the indices through `Interp::get_property`, which is why
**the callback-seam methods already agree with `node` on the `length_getter`
receiver while the native-path methods do not** — same receiver, same property,
two answers.

The seam exists, is documented, is on the hot path, and is already two-thirds
populated for this exact family. **The engine's natives are the only callers that
bypass it.**

### The rejected option, re-examined — the verdict holds, two of three reasons do not

The ledger rejects "rebuild a nested `Interp` inside a native from the registry"
(`array_generic_receivers.rs:669-671`) because a getter body "would run with no
job-queue checkpoint and no access to the caller's frame chain, so a
`Promise.then` inside a getter would deadlock the drain rather than run it."

Re-read against the source, the **verdict is confirmed and the reasoning is
partly wrong**. All three claims need correcting, because the job-queue framing in
particular invites a future lane to go fix the job queue and be surprised.

**Confirmed.** Host `Job`s genuinely cannot run inside a native.
`Interp::run_jobs` (`crates/v12-interp/src/lib.rs:2229-2250`) drains
`self.pending_awaits` **only** — it is an async-resume pump. The host queue is
`JobQueue::drain(&mut self, interp: &mut Interp<'_>, pending: Rc<RefCell<Vec<Job>>>)`
(`crates/v12-engine/src/job_queue.rs:122`), reached only from
`Engine::drain_checkpoint` (`crates/v12-engine/src/engine/eval.rs:520-521`),
which the engine describes as its *single* scheduler and which sits **above every
native**. `Interp::run()` (`lib.rs:1232`) does not drain.

**Correction 1 — it is not a deadlock; it is a silent wrong answer, which is
worse.** `Ctx.pending` is `Rc<RefCell<Vec<Job>>>` (`builtins/ctx.rs:64-65`) and is
*shared* with the engine's queue. A reaction enqueued inside a nested `Interp`
therefore does run — later, at the engine's next checkpoint, i.e. **after the
native has already computed and returned its answer** from a getter whose
microtasks had not yet run. No hang, no error, a wrong value.

**Correction 2 — "no access to the caller's frame chain" is wrong.** A getter is a
fresh activation of a function object; it needs no caller frames. Drop this
reason; it is not load-bearing and it is not true.

**Correction 3 — the real blocker is not the job queue.** Two structural facts,
neither of which is about scheduling:

1. `NativeRegistry::eval` builds its nested `Interp`
   (`crates/v12-engine/src/builtins/registry.rs:562-563`) from a `heap` that is
   **handed to it as a parameter** (`:541`). `ordinary_get` is *mid-walk holding*
   `&mut Heap` and cannot release it without restructuring the walk into
   locate-then-invoke. This is the actual reason the option fails, and it is not
   in the ledger.
2. `FunctionTarget::Bytecode(idx)` resolves through
   `functions_for_program(self.program_id)` (`crates/v12-interp/src/lib.rs:824-833`),
   which needs the interpreter's `program_id`, its `programs` table, **and** its
   local `functions` — three pieces of interpreter state that a `NativeHandler`
   cannot name. (`programs` does appear as a trait parameter on the `eval` path,
   `v12-native/src/registry.rs:44-53`, but only there, and it does not carry the
   caller's `functions`.)

**And the constraint does not bind the recommendation.** The seam *is* the
interpreter's own `Interp`, in the middle of a live run. A getter body invoked
there enqueues into the same `pending` sink and is drained by the same
`drain_checkpoint` at the same point it would have been otherwise. Option 4 below
is the only candidate on the table that does not perturb the scheduling position of
a getter call — worth stating, because the rejected option's failure mode is
precisely a scheduling perturbation.

### Blast radius: three different counts, and they mean different things

The lane reported 2 and 24. Both are real and neither is the blast radius.

| count | what it is | how it is known | what it is good for |
|---|---|---|---|
| **2** | the `invoke_accessor_getter` call sites (`internal_methods.rs:1105`, `:1133`) | reading the file | the **definition-site** count. It is also *unpatchable alone*, and patching it alone is actively wrong: it converts a wrong answer into a `Throw` and relocates the defect into its callers without fixing any of them. |
| **20** | non-test `dispatch_get` call sites, across 9 files: `object.rs` 5, `reflect.rs` 4, `array.rs` 3, `error.rs` 2, `iterator.rs` 2, `string.rs` 1, `json.rs` 1, `promise.rs` 1, `ctx.rs` 1 | a grep | the **syntax** count: how many lines the compiler will point at. It is *not* the damage — most of these read a v12-controlled key on a v12-allocated object (`"message"`, `"name"`, a species constructor's `"constructor"`), whose shape v12 owns and which can therefore never be a user accessor. |
| **N** | natives that reach a reader of a **user-supplied receiver** | **not statically knowable** | the **reachability** count, and the only one that determines cost. It is a property of the transitive `&mut Ctx` call graph over 156 `&mut Ctx` functions. |

(The brief says 24. I count **20** non-test call sites plus the definition; the
difference is comment lines at `regexp.rs:415-421` and doc comments at
`call_setup.rs:2953` / `object_ops.rs:839`.)

**They differ because 2 is a definition-site count, 20 is a syntax count, and N is
a reachability count.** The operative consequence: N is the migration unit, N is
not enumerable by reading, and 20 is a *lower bound* on the compiler's output
rather than an estimate of the work. **This asymmetry is the argument for making
the failure a compile error** — see below. A hand-maintained list of 20 (or of N)
is a list that will be wrong, and per ADR-0002 and this repo's own incident log, a
list that is quietly wrong is worse than no list.

## Decision

**Accessor invocation happens at the call seam. The engine-side `[[Get]]`/`[[Set]]`
lose the ability to answer, and the loss is a type error.**

Three rules, in dependency order.

### Rule 1 — delete the wrong answer; make it a `Throw`

Remove `invoke_accessor_getter` (`internal_methods.rs:942-972`) entirely. In its
place, `ordinary_get`'s two accessor arms (`:1104-1106`, `:1132-1134`) return
`Err`, not a value. Symmetrically, `ordinary_set`'s **three** accessor arms
(`:1166-1172`, `:1193-1201`, `:1204-1211`) return `Err`.

Two constraints on the error, both load-bearing:

- **It must be a `Throw`, not a `panic!` and not a `debug_assert!`.** The
  conformance runner is a release build (`--profile runner`), so a debug-only
  assertion would leave release silently wrong — which is the current defect with
  extra steps.
- **It must name the key and the receiver's kind** in its message, so a
  conformance row that trips it identifies itself without a debugger.

This one change converts an unbounded silent-defect surface into a bounded
compile-time work list: `dispatch_get`/`dispatch_set` already return
`InternalResult<T> = Result<T, Throw>`, so all 20 + 3 call sites must now name an
error path they previously did not have. **This is the migration that cannot
silently skip a builtin** — it is not a checklist, it is rustc.

The honest cost: this lands as a *red* tree. Every one of those sites fails to
compile until migrated or explicitly tolerated. That is the intended shape of the
batch, not a defect in it.

### Rule 2 — split the walk, so the pure part is shareable and the impure part is nameable

Refactor `ordinary_get` into:

- `locate(heap, obj, key) -> Located`, where `Located` is
  `Value(JsValue) | Getter(Handle<JsObject>, receiver) | Absent`. Pure heap
  manipulation; no call; no capability. This is the part that is genuinely common
  ground between the two implementations.
- the invoke step, which is the only part needing the machine.

`Located::Getter` carries the **receiver**, which is what closes disagreement #2:
the invoke step binds `this` to the receiver, not to the walk's current `o`. Fixing
#2 is a consequence of the split, not an extra change — worth recording, because
`regexp.rs:415-421` is currently working around #2 by hand-calling the 22.2.6.1
getter, and that workaround should be revisited once `locate` exists.

`locate` is where the long-term DRY win lives (merge it with the interpreter's
walk), but this ADR does **not** decide that — see Open questions.

### Rule 3 — migrate along the existing seam, over a shared triple

Each site that Rule 1 turns red is resolved one of two ways, and the choice is
mechanical:

- **It reads a user-supplied receiver → migrate to the seam.** The body moves
  behind `Interp::dispatch_native`, beside `ArrayJoin`/`ArrayPush`
  (`call_setup.rs:1603-1604`) and `run_callback_builtin` (`:1980+`). The
  expectation is ~15–25 array methods: of the 38 `Array*` `NativeId` variants,
  15 are already intercepted, leaving the non-callback generic methods
  (`slice`, `splice`, `indexOf`, `lastIndexOf`, `includes`, `at`, `fill`,
  `copyWithin`, `reverse`, `pop`, `shift`, `unshift`, `concat`, `toString`,
  `flat`). **[Not measured as a body count — this is the variant list, not a
  census of which bodies actually reach `dispatch_get`.]**
- **It reads a v12-allocated object at a v12-controlled key → tolerate
  explicitly**, with a one-line comment naming why the key cannot be an accessor.
  `Reflect.get` (`reflect.rs:477`) is the counter-example that proves the rule has
  teeth: it is *specified* to run accessors and already threads a `receiver`, so
  it migrates.

**The anti-drift requirement, and this is the part that decides between DRY and
KISS.** A migrated body must be a *thin* spec-step sequence over a shared triple
— `LengthOfArrayLike`, `HasProperty`+`Get` at an index, `Set` at an index — and
those three must become the **only** index/length readers in the codebase. Today
there are at least six (`array.rs` `array_like_len`/`index_get`/`index_set`,
`call_setup.rs` `callback_len`/`callback_index`, `object_ops.rs`
`array_push_fallback`/`array_join_fallback`, plus the engine-native
`elements_snapshot` readers). ADR-0004 asked for one reader and produced six; this
ADR must not produce a seventh.

Therefore: **the triple is landed first, as its own commit, and a freeze-lint
gate lands with it** — the same mechanism as `./scripts/freeze-lint.sh`, extended
to fail on any new index/length reader outside `v12-interp`. The gate is the
deliverable; the triple is the easy half. Without the gate this decision
reproduces the condition that produced ADR-0004.

### Why not option 2 — change `NativeHandler`

Option 2 as stated is not implementable: a `NativeHandler` already holds
`&mut Heap`, so giving it `&mut Interp` is an aliasing violation, not a signature
change. The only workable form is **additive** — a second table
`MachineNative = fn(&mut Interp, JsValue, &[JsValue])` alongside the existing
`NativeHandler` — and that form has two problems:

1. **It relocates rather than fixes.** `v12-interp` cannot call `v12-engine`'s
   array bodies, so a machine-native table must *move* those bodies into
   `v12-interp`. That is the same move as Rule 3, organised as a table instead of
   match arms.
2. **Its audit has no closing condition.** Giving natives a machine means
   answering "is this native safe to re-enter?" for all 156 `&mut Ctx` functions.
   **Safety here is not a local property** — it depends on what the native calls,
   transitively. There is no test that can close that audit, and no compiler check
   either, because `&mut Heap` is always "fine" to the borrow checker.

Named conflict: **separation of concerns (hexagonal) vs. verifiability.** Giving
`v12-engine` machine capability is the architecturally purer shape — the engine
owns the builtins and would own the capability. Verifiability wins, because this
codebase's demonstrated failure mode is *unenforced* structure, not misplaced
structure: per the memory-verified audit, `./scripts/check-architecture.sh` does
not exist, clippy has never run in CI, and `deny.toml` is wired into nothing. In a
repo where the enforcement tier is already the defect, adding a capability that
*cannot* be statically checked is the wrong trade.

### Why not option 3 — make `dispatch_get` reach the interpreter's real `[[Get]]`

The DAG forbids it in the direction needed, and the reason is worth writing down
because it is not the reason one expects. `v12-engine` *does* depend on
`v12-interp`, so it can name `Interp`. What it cannot do is **obtain one**: the
`Interp` that owns the live program is owned by `Engine::run_compiled` and is
deliberately *scoped* —

> "The interpreter is scoped to this method, so the borrow ends when it drops.
> Destructure `self` so heap and jobs/registry accesses are disjoint locals."
> — `crates/v12-engine/src/engine/eval.rs:619-620`

— and `drain_checkpoint` (`eval.rs:520-521`) needs `&mut Interp` *and*
`&mut NativeRegistry` *and* `&mut JobQueue` as disjoint locals of the same
`Engine` destructure. Making option 3 work means making the `Interp` a long-lived
field of `Engine`, owning `&mut Heap` for the engine's lifetime. That is a much
larger change than it looks, and it moves the aliasing problem from a place where
it is visible (inside one native) to a place where it is not (the engine's whole
lifetime).

Rejected for now — but note it is the natural **destination state** if the engine
ever wants a persistent machine (a resident REPL, a long-lived `Engine` driving
many evals). Recorded so that lane does not re-derive it.

## Consequences

1. **E2 closes; 98 ledger rows close with it.** The `E2_ACCESSOR_GETTER_UNREADABLE`
   excuse string at `array_generic_receivers.rs:682-688` must be **deleted**, not
   reworded. The `KNOWN_DIVERGENCES` ratchet (`array_generic_receivers.rs:676+`)
   recomputes the divergence set and fails on any added row, so a re-introduced E2
   resurfaces as a new failing row rather than as a stale comment.

2. **Disagreement #2 closes as a side effect** (Rule 2's `Located::Getter` carries
   the receiver), and `regexp.rs:415-421`'s hand-rolled workaround becomes
   removable. Not verified here that removing it is safe — the 22.2.6.1 getter does
   its own receiver resolution, so the workaround may still be the *better* call
   at that one site. Flagged, not decided.

3. **A second `FunctionTarget` disagreement is closed by construction.** The
   grouped arm at `internal_methods.rs:964-968` folds `Bytecode`, `RealmEval` and
   `Bound` into one `return Ok(undefined)`. When it is deleted, each variant gets
   its own arm. To prevent a future sixth variant from being folded back into a
   catch-all — the ADR-0002 lesson, one layer down — the replacement must name all
   five variants **individually**, each with its own `Throw` message naming the
   variant. Grouping them again would recreate the defect with a different
   payload.

4. **The setter mirror lands in the same commit.** Three sites, not one; write-loss,
   not read-loss; `dispatch_set` has 3 call sites.

5. **The tree goes red on landing.** Rule 1 deliberately breaks 23 call sites
   (20 `dispatch_get` + 3 `dispatch_set`) before Rule 3 heals them. This must be
   **one branch, batched**, not 23 commits — the incident log records a workspace
   reset that destroyed an uncommitted multi-file diff, and a 23-commit
   fix-one-site-each history is exactly the shape that invites it.

6. **`InternalError`-class throws become reachable in release.** The conformance
   runner classifies throws; a new class of internal error may change how failing
   rows are labelled. Not investigated (Open questions).

## What this decision does **not** fix

1. **Disagreement #3 — per-index attributes.** `array.rs:276-286` records it as a
   missing `v12-heap` *feature* (`Object.defineProperty(a, 2, {configurable:false})`
   is silently dropped; `Object.freeze` on an array leaves elements reporting
   `writable:true`). That is a heap-model change, not a capability change, and
   bundling it would make this batch untestable. **Recommend against bundling.**
2. **The fifteen structural surfaces in `Interp::get_property`**
   (`property.rs:223-264`). Rule 2 extracts `locate` from `ordinary_get` only.
   Merging `locate` into the interpreter's walk is the larger DRY win and is
   deferred (Open questions).
3. **`array_like_len`'s elements-first probe** (`array.rs:152-160`), an inlined
   shortcut ahead of the property path that ADR-0004 flagged as "MUTATION 6". It
   will survive Rule 3 and remains a divergence on a receiver that is `Kind::Array`
   with a populated `elements` *and* an accessor-backed `length`.
4. **`Object.defineProperty` / `getOwnPropertyDescriptor` agreement** for
   accessors on dictionary-rung keys. Out of scope; the ledger's E1 row is closed
   and this is not E1.
5. **Anything about conformance numbers.** No gate was run. The 98 figure is
   carried from the finding lane, unverified.

## Contradictions with the brief this ADR answers

- **"Option 1 is a tax."** Partly conceded and partly reversed. Option 1 *as
  stated* — 25 hand-written fallbacks, each a re-implementation free to drift — is
  a tax, and it is the condition that let ADR-0004 find one cause living at three
  sites. What this ADR adds is that the fallbacks are **not chosen up front**: they
  are *selected per compile error* by Rule 1, so the 23 sites that do **not** need
  migrating are identified by the compiler rather than by my estimate. The DRY
  defence is the shared triple plus the freeze-lint gate, not discipline.
- **"The heap-side property path needs call capability" ⇒ "options 2 and 3 are the
  same decision."** Not quite. Options 2 and 3 both *grant* capability; they differ
  in **who holds it** (a handler signature vs. a resident `Interp`). They are the
  same decision about *whether*, not about *where*. The question that decides it is
  "who can reach the path," and that has the answer already in the tree.
- **"There are 24 `dispatch_get` call sites."** 20 non-test, plus the definition.
  See Context.
- **"A `Promise.then` inside a getter would deadlock the microtask drain."** It
  would not deadlock; it would be silently deferred past the native's return. See
  Context. The verdict on the option is unchanged; the reason is corrected so that
  it cannot be mis-repaired.

## Open questions for the decider

1. **Should `locate` replace the interpreter's walk too?** This is the real DRY
   prize — one `[[Get]]` instead of two would close the whole disagreement class,
   not just these three instances. **I did not decide it because I could not
   measure whether `get_property`'s fifteen surfaces plus `ic_lookup` are
   IC-critical**; merging a specialised hot path into a general one is a
   performance decision, not an architectural one, and this ADR is read-only.
   *Would change my mind if* a profile showed the surfaces are not IC-hot — then
   the prize is large enough to pay for the merge.
2. **What `Throw` shape for the new accessor error?** Loud in release is required;
   whether it is a distinguishable variant or a message-classified `InternalError`
   affects how the conformance runner labels rows. Not checked.
3. **Per-site tolerate-vs-migrate for `object.rs` (5) and `ctx.rs` (1).**
   `ctx.rs:498` reads a species constructor's `"constructor"`; `object.rs`'s
   `"message"`/`"name"` reads are on v12-allocated error objects. I expect all six
   to tolerate, but that is per-site judgement about code I read only in grep
   context, not a finding.
4. **Do `FunctionTarget::Bound` and `RealmEval` actually occur as getters
   today?** They share the grouped arm and Rule 3's per-variant split will touch
   them, but I did not establish whether either is reachable in that role. If one
   is dead, the arm should be deleted rather than given a `Throw`.
5. **Is `Engine`-resident-`Interp` (option 3's destination) wanted independently?**
   If the roadmap wants a long-lived machine, doing that *first* changes this ADR's
   arithmetic. Nothing in `CONTEXT.md` or `ROADMAP.md` as read says so.

## Verification

No gate was run for this ADR — two lanes were mid-edit, so `cargo nextest` and
`conformance/run.sh` results would have been meaningless. Every claim above is a
file-and-line reading. Re-checkable:

```sh
# the two [[Get]]s share no code
grep -rn "dispatch_get" crates/v12-interp/src/          # 2 hits, both doc comments
grep -rn "fn get_property" crates/v12-interp/src/property.rs

# the DAG constraint, as the tree states it
grep -n "v12-interp" crates/v12-engine/Cargo.toml        # engine -> interp
sed -n '2953,2972p' crates/v12-interp/src/call_setup.rs # the inverse is forbidden

# the seam and its population
sed -n '1561,1566p' crates/v12-interp/src/call_setup.rs # the router contract
grep -n "NativeId::Array" crates/v12-interp/src/call_setup.rs   # 15 intercepted
grep -o "Array[A-Za-z]*" crates/v12-native/src/id.rs | sort -u | wc -l   # 38

# the three getter sites and the three setter sites
sed -n '942,972p'   crates/v12-engine/src/internal_methods.rs
sed -n '1104,1134p' crates/v12-engine/src/internal_methods.rs
sed -n '1166,1211p' crates/v12-engine/src/internal_methods.rs

# the ignored receiver (disagreement #2)
sed -n '1061,1066p' crates/v12-engine/src/internal_methods.rs
sed -n '415,421p'   crates/v12-engine/src/builtins/regexp.rs

# the re-entrancy constraint
sed -n '2229,2250p' crates/v12-interp/src/lib.rs          # run_jobs: awaits only
sed -n '118,126p'   crates/v12-engine/src/job_queue.rs    # drain needs &mut Interp
sed -n '518,524p'   crates/v12-engine/src/engine/eval.rs  # the single scheduler
sed -n '538,576p'   crates/v12-engine/src/builtins/registry.rs  # eval: heap is a PARAM
sed -n '824,833p'   crates/v12-interp/src/lib.rs          # Bytecode -> program_id

# the blast-radius counts
grep -rn "dispatch_get(" crates/v12-engine/src crates/v12-interp/src \
  | grep -v "pub fn" | grep -v "///" | grep -v "^\S*: *//" | wc -l    # 21 = 20 + def
grep -rn "dispatch_set(" crates/v12-engine/src | grep -v "pub fn"        # 4 = 3 + tests
```
