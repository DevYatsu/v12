# CONTEXT.md — v12 ubiquitous language

Canonical terms for the v12 project. One meaning per term; use these words
consistently in docs, ADRs, and discussion. Implementation details live in
`plan_idea.md`, not here.

## Engine & scope

- **v12** — the JavaScript engine project; also the CLI binary name.
- **v1** — the first shippable milestone: ES2026 ratified baseline, Annex B,
  single realm, single-threaded mutator (no Workers/SharedArrayBuffer/Atomics).
- **Realm** — a self-contained global environment: its own global object and
  built-in intrinsics. v1 has exactly one.
- **Tier 0 / Tier 1 / Tier 2** — the three execution tiers: bytecode interpreter /
  baseline template JIT / speculative optimizing JIT (Tier 2 is post-v1).
- **Execution driver** — the engine component that owns the run loop: it consumes
  tier-up flags and hands work to whichever tier runs next.
- **Embedder** — any program hosting v12 as a library; distinct from the CLI.

## Values

- **JsValue** — the machine word holding one JavaScript value (number, small
  integer, heap reference, or special like `undefined`).
- **Smi** — a small integer stored directly inside a JsValue, never on the heap.
- **HeapRef** — a reference to a heap object carried inside a JsValue.
- **Hole** — the internal marker for an absent array element; never observable
  from conforming JavaScript.
- **Interned string / internalization** — making a string canonical so identity
  comparison becomes integer comparison.

## Objects

- **Shape** — the immutable description of an object's property layout, shared by
  all objects built through the same property-addition history.
- **Transition tree** — the graph linking shapes by property addition; children
  are new shapes.
- **Validity cell** — a version stamp guarding assumptions about a prototype
  chain; mutations to the chain invalidate dependent caches.
- **Integrity level** — sealed/frozen status, represented as a shape transition.
- **Internal method** — one of the spec's `[[...]]` operations (`[[Get]]`,
  `[[Set]]`, …) that define object behavior.
- **Ordinary object** — an object whose internal methods are the spec defaults;
  fast paths may assume ordinariness after a shape check.
- **Dictionary mode** — an object that abandoned shape-backed storage for a hash
  map of properties.

## Execution

- **Frame** — one activation of a function: its registers, PC, and control state;
  pausable data, not native stack.
- **Value stack** — the single contiguous memory region holding all frames'
  register files.
- **Environment** — a heap object holding variables captured by inner functions.
- **FeedbackVector** — per-function slots recording observed types/shapes used by
  inline caches and tier-up decisions.
- **Inline cache (IC)** — a call site specialized to the shapes it has observed.
- **Megamorphic** — a site too polymorphic to specialize; served by the stub cache.
- **OSR (on-stack replacement)** — moving execution of a *running* activation from
  one tier into another.
- **Deopt** — abandoning speculated JIT code and materializing an equivalent
  interpreter frame.

## Memory

- **Handle** — a typed index into a per-class heap space; the only way to reach a
  heap object.
- **Segment** — a fixed-size block of heap memory bump-allocated and swept.
- **Ephemeron** — a weak-collection entry kept alive only while both its key and
  value are reachable; requires special marking.
- **CleanupJob** — the queued callback running a FinalizationRegistry's work for
  a collected target.

## Concurrency & scheduling

- **Mutator** — the thread executing JavaScript (exactly one in v1).
- **Job queue** — the ordered list of pending promise reactions, microtasks, and
  cleanup jobs; drained at microtask checkpoints.

## Conformance & tooling

- Always run tests with **`cargo nextest run --workspace`**, not `cargo test`. `cargo nextest` is the workspace gate (faster, clearer output, same 563 tests). `cargo test` remains available but is not the canonical command.
- **Test262 pass rate** — `language` suite: **~36 %** (8 919 hidden-key, ~8 800 spec-correct; 24 446 executable, 427 skipped) after Step 7b-7c (2026-09-02); `language/expressions` **3 771/11 164 (33.8 %)** spec-correct (3 828 hidden-key). Step 7a was 34.8 % (8 501); baseline 19.9 % (4 858). Verified via `cargo run -p test262-runner -- --filter language/expressions --jobs 4 --format json` (full language timed out in CI). Nextest gate is canonical.
- **cargo nextest** — **563 passed, 1 skipped** (`cargo nextest run --workspace`, 14.0 s). Covers `v12-bytecode` decode sweeps (1.7 s with new WideOps 11-14 width 4), `v12-bccompiler` (132), `v12-interp`, `v12-engine` builtins, `v12-jit-*`, `v12-cli` spawns.
- **GetNewTarget** — bytecode opcode 63 (`r{a} = new.target`). Returns the constructor for `new` calls, `undefined` otherwise. Arrow functions inherit from enclosing non-arrow frame. Backed by `Frame::new_target: Option<JsValue>`.
- **Dynamic import** — `import(source)` desugared to `Closure #NATIVE_IMPORT_INDEX` call (254). Step 5 registered `ModuleImport` native stub → `ModuleImport is not registered` 306 → 324 `dynamic import not supported` (same count, now proper TypeError).
- **Collector walk** — `crates/v12-bccompiler/src/collect.rs` now walks `Switch`/`With`/`New`/`Template`/`Yield`/`Await` and destructuring assignment targets. Closed `nested function missing from plans` 684 → 62 (−622) after Step 5 catch-destructuring.
- **Array.isArray + Object statics** — Step 3a: `Array.isArray` native 1106 + `Object.create`/`getPrototypeOf`/`defineProperty` → `callee` 4 518 → 3 690 (−828).
- **Object/Function/Array protos** — Step 3b: `Object.keys/values/entries/hasOwnProperty`, `Function.prototype.call/apply/bind/toString`, `Array.slice/sort` → `callee` 3 690 → 3 547 (−143).
- **For-of destructuring** — Step 4: `stmt.rs` handles array/object destructuring and member targets via `assign_for_of_value` → `for-of` 479 → 389 (−90); language +94.
- **Minor syntax** — Step 5: `BigInt` → `BigInt` heap, `tagged template` → desugared call, `with` → best-effort, `catch` destructuring, sentinel `0xFFFFFFFF` → `not a function` → closed BigInt 147 → 0, tagged 43 → 0, with 108 → 0.
- **Instanceof/prototype** — Step 6: `op_instanceof` validates callable and lazily materializes `Function.prototype` → `non-object prototype` 9 → 0; nextest 563.
- **Object Object triage** — Step 7a: `engine.rs`/`interp` `to_display_string` renders plain-object `Test262Error` via `message`/`name` → opaque `threw: [object Object]` 3 167 → 0 (reclassified to `Expected a undefined to be thrown` 975, `abrupt completion` 205, etc.); pass unchanged then.
- **Private fields** — Step 7b hidden-key then 7c spec-correct: 1925a29 desugared `#x` to `"#x"` (1 492 → 0 but leaked via `obj["#x"]`). Replaced in 2dfb52c with WideOps `GetPrivateW/SetPrivateW/DefinePrivateW/HasPrivateW` (disc 11-14, width 4), `JsObject {private_brand, private_fields}` with GC trace and `Construct` clone, brand-checked `private_get/has/define/set` → `obj["#x"]` now `undefined`, outside-class `TypeError`; `this` slot panic fixed (`expr.rs:142` fallback + `collect.rs` field-init walk for `() => this.#x`). `language/expressions` 1 492 → 0 retained, 3 771 pass spec-correct. Nextest 563.

## Incident log

- **2026-09-02 workspace reset** — parallel fixer lanes shared one working tree and one lane ran `git stash` + `reset` to "preserve WIP", wiping the uncommitted Step 8 diff (and the fix-log Steps 1–7c backfill). Recovery: `stash@{0}` restored ~half, the erased files were rebuilt spec-first, everything landed as `24e838f`; the fix-log backfill was reconstructed from git-log commit messages. `stash@{0}` is retained untouched as a safety copy (its content now also lives in `24e838f`). Rules: commit verified work immediately; never fan out write lanes that touch the same files in parallel; no lane may run `git stash`/`reset`/`checkout` or workspace-wide `cargo fmt`.
