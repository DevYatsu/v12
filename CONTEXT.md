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
