# v12 — Architecture Evolution Plan

> **Persona:** Software Architect. Goal: name the root-cause structural decisions,
> record them as ADRs with trade-offs, and lay out a *reversible*, phased plan to
> harden the API and the crate boundaries before the engine grows any larger.
>
> **Read this before** the three sub-plans already in `docs/superpowers/plans/`:
> `2026-08-27-embed-api.md` (facade crate, *implemented*),
> `2026-08-28-generators-async-full-support.md` (features, *implemented*),
> `2026-08-29-engine-interp-refactor.md` (DRY / symptom fixes, *implemented*).
> This plan sits **above** those: it fixes the *structural* causes those plans
> worked around and defines the public API shape the facade plan builds on.

## 0. How to read this

- **Bounded contexts** are drawn from `CONTEXT.md` (the v12 ubiquitous language).
- Every decision is an **ADR** (status: *Proposed*). Each lists ≥2 options and the
  trade-off we are accepting.
- The roadmap is **ordered by reversibility risk**, cheapest-and-safest first.
- Nothing here changes JS semantics. Conformance (Test262 24.9%) is a guardrail,
  not a goal of this plan.

---

## 1. Current architecture (as-built)

### 1.1 Crate dependency graph (verified against `Cargo.toml`)

```
                 v12-cli ──► v12-engine ──► v12-interp ──► v12-bccompiler ──► v12-bytecode ──► oxc_span
                       │            │             │              │                  │
                       │            ├──► v12-heap ◄┘              │                  └─► lasso
                       │            ├──► v12-regex ──► regress    └─► oxc_* (parser/ast/semantic)
                       │            ├──► v12-intl  ──► icu / temporal_rs
                       │            └──► v12-jit-baseline (optional) ──► cranelift_*
                       └──► v12-bccompiler, v12-bytecode  (CLI reaches into internals)

                 v12-jit-opt ──► v12-jit-baseline ──► v12-interp ──► v12-bytecode
                 (NOT wired into engine — Tier-2 driver pending)

                 test-support ──► v12-interp + v12-bccompiler + v12-heap + v12-bytecode
```

### 1.2 Bounded contexts in the engine domain

| Context | Lives in | Responsibility | Talks to |
|---|---|---|---|
| **Front-end** | `v12-bccompiler`, `v12-bytecode`, `v12-regex`, `v12-intl` | Parse → bytecode; string/regex/intl primitives | compiler only |
| **Runtime core** | `v12-heap` | Value word, handles, hidden classes, shape tree, mark-sweep GC | nobody above it |
| **Execution** | `v12-interp`, `v12-jit-baseline`, `v12-jit-opt` | Run bytecode; tiers; tier-up | heap + bytecode |
| **Embedding** | `v12-engine` | Realm, intrinsics, built-ins, job queue, native registry, host API | everything |
| **Delivery** | `v12-cli`, (planned `v12-api`) | REPL / script runner / embedder facade | engine |

### 1.3 Quality-attribute snapshot

- **Correctness:** strong — interpreter-first, every optimization fails closed.
- **Changeability:** weak — `v12-interp/src/lib.rs` is ~3.7 kLOC; two shape side-tables; heap ownership swaps on every call.
- **Embeddability:** weak — `Engine` is both orchestrator *and* public surface; no facade; completion value is always `undefined`; errors are raw `JsValue`s.
- **Build/dependency hygiene:** weak — the *runtime* (`v12-interp`) depends on the *front-end* (`v12-bccompiler`), pulling `oxc_*` + `lasso` into every embedder's binary.

---

## 2. Problems to update (root causes, not symptoms)

| # | Severity | Problem | Evidence |
|---|---|---|---|
| P1 | **High** | **Runtime → Front-end dependency inversion.** `v12-interp` depends on `v12-bccompiler` (`from_source`, `model::GLOBAL_INTRINSICS`). The execution context should never depend on the compiler. | `interp/src/lib.rs:4,160,522`; `interp/Cargo.toml` |
| P1 | **High** | **Global `SHAPE_TABLE` keyed by raw heap address.** `static SHAPE_TABLE: RefCell<HashMap<(usize, u32), ShapeHandle>>` keyed by `heap as *const Heap as usize`. Address reuse after `Heap` drop/realloc → stale shape entries; shared across all engines/threads in the process; duplicated by the interp's own validity-cell side table. | `engine/src/internal_methods.rs:625-675` |
| P1 | **High** | **Heap ownership ping-pong.** Every `eval`/`run_jobs` does `std::mem::replace(&mut self.heap, Heap::new(GcPolicy::NoGC))`, runs the `Interp` as heap owner, then swaps back. Allocates a throwaway `Heap` per call; `Engine::heap()` returns a stale reference during execution; `retained` program + strings are deep-cloned every `eval` and `run_jobs`. | `engine/src/engine.rs:173,304,409` |
| P1 | **High** | **No structured error / no completion value.** `Engine::eval` returns `Result<JsValue, JsValue>`; compile errors, thrown values, and OOM are indistinguishable to a host; normal completion is hard-coded to `undefined` (spec violation for `eval`). | `engine/src/engine.rs:188`; `value.rs` traits |
| P2 | Med | **Reentrant native-channel mutation.** `eval_indirect` calls `registry.set_pending(Rc::clone(&local))` then *restores* it after. A panic in between leaves the engine's queue pointed at the wrong heap's pending jobs. | `engine/src/engine.rs:222-235` |
| P2 | Med | **`Engine` = orchestrator + public API.** No stable facade; hosts depend on `v12_engine::*` internals. `v12-api` facade now exists (ADR-005); `v12-cli` repointing is follow-up. | `engine/src/engine.rs`; `crates/v12-api` |
| P2 | Med | **Tier-2 not wired; JIT layering inverted.** `v12-engine` doesn't depend on `v12-jit-opt` at all. `v12-jit-opt` depends on `v12-jit-baseline` — a higher tier depending on a lower one. | `engine/Cargo.toml`; `jit-opt/Cargo.toml` |
| P2 | Med | **Monolith + duplicated shape state.** 3.7 kLOC `lib.rs`; engine and interp each keep their own object→shape side table. | `interp/src/lib.rs`; `internal_methods.rs` |
| P3 | Low | **Bytecode crate carries `oxc_span`** (front-end span metadata in the frozen ISA crate); **CLI depends on `v12-bccompiler`/`v12-bytecode` directly** instead of going through the engine facade; **`scratch_tests.rs` committed into `src/`**. | `bytecode/Cargo.toml`; `cli/Cargo.toml`; `engine/src/scratch_tests.rs` |

---

## 3. Architecture Decision Records

> Each ADR names the trade-off we accept. "Reversible?" rates how cheaply we can
> unwind the decision if it proves wrong.

### ADR-001 — Move `Program` (and intrinsic indices) into `v12-bytecode`; decouple `v12-interp` from `v12-bccompiler`

**Status:** Accepted — landed with the A1 workstream (2026-08-29). `Program`
and `GLOBAL_INTRINSICS` live in `v12-bytecode`; `v12-interp`'s runtime deps
are `v12-bytecode` + `v12-heap` only (`from_source` is feature-gated).

**Context (P1):** The interpreter should run bytecode, not own a compiler. Today
`v12-interp` imports `v12_bccompiler::Program`, `compile_source_with_strings`,
`GLOBAL_INTRINSICS`. `Program { functions: Vec<FunctionBytecode>, main: u32 }`
already only references `v12-bytecode` types.

**Options**
1. *(Chosen)* Move `Program` + the `GLOBAL_INTRINSICS` constant into `v12-bytecode`.
   `Interp::new(program.functions, program.main, strings)` already takes the
   decomposed parts; only `from_source` needs the compiler, so move `from_source`
   out of `Interp` (into `v12-engine` or a thin `v12-compile` adapter). `v12-bccompiler`
   re-exports `Program` for back-compat.
2. Leave it; accept `oxc_*` + `lasso` in every embedder binary.
3. Merge bccompiler into interp (worse — front-end leaks into runtime forever).

**Consequences**
- `+` `v12-interp` depends only on `v12-bytecode` + `v12-heap`. Embedders can run
  pre-compiled `Program`s with zero front-end deps.
- `+` Smaller, faster-to-link runtime crate; cleaner dependency direction.
- `-` One mechanical move + a re-export shim. `from_source` callers update one path.
- **Reversible:** yes — fully (re-export keeps old names).

### ADR-002 — Shape/validity-cell association owned by `Heap`; delete the global `SHAPE_TABLE`

**Status:** Accepted — landed earlier (the `thread_local SHAPE_TABLE` is
gone; `Heap::shape_of`/`bind_shape` own the map; see the note at
`internal_methods.rs:622`).

**Context (P1):** Object→shape binding currently lives in a `thread_local`
`RefCell<HashMap<(usize,u32),ShapeHandle>>` keyed by **raw heap pointer**. Same
address reused by a new `Heap` reads/writes another engine's shapes. It is also
duplicated by the interpreter's own validity-cell side table.

**Options**
1. *(Chosen)* Store the shape association **inside `Heap`** — e.g. a `HashMap<ValidityCellId, ShapeHandle>` owned by the heap instance, rooted with the object. `shape_of`/`bind_shape` become `Heap` methods; delete the `thread_local`. The interp's parallel table is removed in favor of the same `Heap`-owned map.
2. Keep the global but key by an explicit `HeapId` (u64 epoch) instead of pointer.
   Cheaper than (1) but still global state; two tables remain.
3. Store the `ShapeHandle` directly on `JsObject` (no side table). Fastest lookup
   but widens every object word and complicates GC tracing.

**Consequences**
- `+` Kills the address-reuse correctness bug; shapes are GC-scoped with the heap.
- `+` Single source of truth for object→shape; removes the engine/interp duplication.
- `-` Touches every shape-touching call site (`internal_methods.rs`, `interp`).
- **Reversible:** yes (the map is internal; no public API change expected yet).

### ADR-003 — `Engine` owns its `Heap` for its whole lifetime; `Interp` borrows `&mut Heap`

**Status:** Accepted — landed with the A2 workstream (2026-08-29, commit
`5128ca6`). `Interp<'a>` borrows `&mut Heap`; the three `mem::replace` swaps
are gone; `Engine::heap()` stays valid for the interpreter's lifetime.

**Context (P1):** The `mem::replace` swap allocates a sentinel `Heap` per
`eval`, invalidates `Engine::heap()` mid-call, and forces a full
`retained`-program clone each time. This is the single biggest embeddability
blocker.

**Options**
1. *(Chosen)* `Engine` keeps `heap: Heap` for its whole life. `Interp::new` takes
   `&mut Heap` (+ `&Realm`/`&mut JobQueue` as needed) and returns the borrowed
   heap when done (or never takes ownership). `eval`/`run_jobs`/`call_global` build
   an `Interp` over `&mut self.heap` and drop it at the end — no swap, no sentinel,
   no clone. The `retained` `Program` is **borrowed**, not cloned.
2. Keep the swap but clone lazily / use `Arc<Program>`. Band-aid; the borrow
   invalidation stays.
3. Make `Interp` the long-lived object and `Engine` a thin wrapper. Inverts the
   intended ownership (Engine should own policy/realm, Interp is a transient runner).

**Consequences**
- `+` No per-call `Heap` allocation; `Engine::heap()` is always valid.
- `+` `retained` is borrowed → `eval`/`run_jobs` stop deep-cloning functions+strings.
- `-` `Interp` API changes from `new_with_heap(heap, …) -> into_heap()` to
   `new(heap: &mut Heap, …)`. Every call site (engine, `eval_indirect`, tests)
   updates; `from_source` convenience moves to the engine/compile layer.
- **Reversible:** moderate — changes `Interp`'s primary constructor; pin with tests.

### ADR-004 — Structured `EngineError` + spec-compliant completion value

**Status:** Accepted (structured error part) — `EngineError` with
`Compile`/`Thrown`/`Host` variants and `eval_with_completion` landed
earlier (`crates/v12-engine/src/error.rs`). The spec-compliant completion
value for expression statements remains a known gap: `top_result` only
captures explicit `return`s, so `eval("1+1")` still completes as
`undefined` (documented on `Engine::eval_with_completion`).

**Context (P1):** `Result<JsValue, JsValue>` forces hosts to stringify thrown values
and cannot represent "compile failed" vs "threw" vs "out of memory". `eval` return
is always `undefined`.

**Options**
1. *(Chosen)* Introduce `pub enum EngineError { Compile(CompileError), Thrown(JsValue), Host(String) }` and change
   `eval` to `Result<JsValue, EngineError>` where `Ok` is the **real script
   completion value**. Keep `JSException(JsValue)` as the *internal* interp signal.
   Provide `to_display_string` on the error.
2. Keep `Result<JsValue, JsValue>` but add a `#[repr]` tag. Cheaper but still no
   completion value and still untyped.
3. Full `CompletionRecord` type. Most correct, but YAGNI for v1.

**Consequences**
- `+` Hosts can `match` on error kind; `eval("1+1")` finally returns `2`.
- `+` Enables typed `Context::eval::<T>()` in the facade without string hacks.
- `-` Breaks the existing `eval` signature; all `engine.rs` callers + the facade
   plan update. (`Option<T>`/`Vec<T>` `FromValue` already exist — see `value.rs`.)
- **Reversible:** low — signature change; keep a `eval_unwrap_value` compat shim.

### ADR-005 — Facade crate `v12-api` over a corrected `Engine` (ports & adapters)

**Status:** Accepted — `crates/v12-api` landed with the B workstream
(2026-08-29): `Context` (`new`/`eval`/`register_fn`/`call`/`pump`),
`Runtime`, and `V12Error` (`Compile`/`Thrown`/`Host`), plus the
`calculator` example and README. `v12-cli` repointing onto the facade is
left as follow-up.

**Context (P2):** `v12-api` was planned but unstarted when this ADR was
written; the facade plan as written builds on the *current* (swapping,
`undefined`-returning) `Engine`. ADR-003/004 were sequenced first so the
facade did not bake the leaks in.

**Options**
1. *(Chosen)* Do **ADR-003/004 first**, then build `v12-api` (`Runtime` →
   `Context`, `register_fn`, `call`, `pump`) strictly over the public `Engine`
   surface. `v12-engine` becomes the *internal* orchestrator (not re-exported by
   the facade). `v12-cli` and hosts depend on `v12-api`, not `v12-engine`.
2. Build `v12-api` now over current `Engine`, refactor later. Rework risk.
3. No facade; document `v12-engine` as the public API. Leaves the monolith public.

**Consequences**
- `+` Clean, versioned, `Send`-free embedder surface; CLI and hosts share it.
- `+` Engine internals can evolve behind the facade without breaking hosts.
- `-` Net-new crate + the 7-task facade plan, now sequenced *after* the core fixes.
- **Reversible:** yes — facade is additive; old `v12-engine` API can linger.

### ADR-006 — JIT as a pluggable tier behind a shared codegen core; wire Tier-2

**Status:** Accepted (type-layering part) — landed with the D1 workstream
(2026-08-29, commit `20703fb`): `CompiledFn`/`JitCache`/`JitError`/limits
moved into `v12-codegen`; both tiers consume them from the shared core and
`v12-jit-opt`'s public API no longer re-exports baseline types. `Engine`
has `tier_policy`; the Tier-2 driver wiring (`JitOpt::compile` from the
engine) remains follow-up.

**Context (P2):** `v12-jit-opt` depends on `v12-jit-baseline` (inverted), and
`v12-engine` never invokes `v12-jit-opt`. Driver wiring is still "pending" in README.

**Options**
1. *(Chosen)* Extract a `v12-codegen` (or `v12-jit-core`) with the shared seams
   both tiers need: deopt map, guard emission, executable-memory region, tier-up
   hooks (`TierHooks` already exists in interp). Both `v12-jit-baseline` and
   `v12-jit-opt` depend on `v12-codegen`, **not** on each other. `Engine` gains a
   `tier_policy` and wires the second tier-up into `JitOpt::compile` + deopt backoff.
2. Keep baseline→opt dependency; only wire the driver. Less churn now, debt later.
3. Single combined JIT crate. Simpler deps but couples the two tiers' maturity.

**Consequences**
- `+` Correct layering; Tier-2 becomes pluggable and testable in isolation.
- `+` Closes the README "Tier-2 driver wiring" roadmap item.
- `-` New crate + refactor of both JIT crates; feature-flag wiring in `v12-engine`.
- **Reversible:** moderate — new crate; old deps can be restored.

### ADR-007 — Job queue / native-pending channel must not mutate shared registry state

**Status:** Accepted — `eval_indirect` uses its own `NativeRegistry` with its
own pending sink (no `set_pending` save/restore); the engine's registry is
untouched for the whole call.

**Context (P2):** `eval_indirect` re-points `registry.set_pending(...)` to a local
channel and restores it — panics leave the engine cross-wired.

**Options**
1. *(Chosen)* Make the pending-microtask sink a **per-`Interp`** field
   (`Vec<Job>` owned by the running interp), not a borrow of the engine's
   `Rc<RefCell<…>>`. `Engine::adopt_pending` pulls from the finished interp's
   queue. No global channel swap; `eval_indirect`'s save/restore disappears.
2. Keep the channel but scope it per-`Engine` with RAII guard. Safer than now but
   still shared mutable state.
3. Move jobs onto the `Heap` (GC-managed job queue). Most "engine-like" but couples
   scheduling to GC lifetime.

**Consequences**
- `+` Removes a reentrancy hazard; indirect-eval isolation becomes structural.
- `-` Changes how natives enqueue; touches `NativeRegistry` + `JobQueue` seams.
- **Reversible:** yes.

---

## 4. Target architecture (C4 container view)

```
┌─────────────────────────────────────────────────────────────────────────┐
│  DELIVERY                                                              │
│   v12-cli ──► v12-api (facade)      hosts ──► v12-api                  │
└───────────────────────────┬─────────────────────────────────────────────┘
                              │ depends on (public surface only)
┌────────────────────────────▼────────────────────────────────────────────┐
│  EMBEDDING (v12-engine)                                                 │
│   Engine owns: Heap, Realm, JobQueue, NativeRegistry, RetainedProgram    │
│   API: eval→Result<JsValue,EngineError>  call / pump / register_fn      │
└───────┬───────────────┬───────────────┬───────────────┬─────────────────┘
        │               │               │               │
┌───────▼──────┐ ┌──────▼───────┐ ┌─────▼──────┐ ┌──────▼──────────────┐
│ EXECUTION   │ │ RUNTIME CORE  │ │ FRONT-END  │ │  JIT (optional)     │
│ v12-interp  │►│ v12-heap      │◄┤ bccompiler │ │ v12-codegen (core)  │
│ (borrows    │ │  owns shapes  │ │  compiles  │ │   ├─ jit-baseline   │
│  &mut Heap) │ │  + GC + vals  │ │  →Program  │ │   └─ jit-opt        │
└─────────────┘ └──────▲───────┘ └─────▲──────┘ └──────▲──────────────┘
                        │               │               │
                 v12-bytecode ◄─────────┘  (Program, ISA, FunctionBytecode)
                 (oxc_span only)
```

Key arrows that flip vs today:
- `v12-interp` → `v12-bccompiler` is **removed** (ADR-001).
- `v12-interp` **borrows** `Heap` (ADR-003) instead of owning/swapping it.
- Shape state lives **inside** `v12-heap` (ADR-002).
- `v12-jit-baseline`/`v12-jit-opt` both depend on `v12-codegen`, not each other (ADR-006).
- `v12-cli` + hosts go through `v12-api`, not `v12-engine` internals (ADR-005).

---

## 5. Phased roadmap (ordered by reversibility risk, cheapest first)

### Phase 0 — Stabilize & de-risk (no API change, fully reversible)
1. **Delete `engine/src/scratch_tests.rs`**; add a CI lint that blocks stray `*.rs`
   in `src/` that isn't a module. (P3)
2. **`cargo clippy --workspace -D warnings`** gate; lock the 484-test baseline so
   every later phase has a hard floor. (P1/P2 guardrail)
3. **Confirm Test262 `language` baseline** (4 958/24 873) before any structural move.

### Phase 1 — Dependency direction (ADR-001) — *low risk, high payoff*
4. Move `Program` + `GLOBAL_INTRINSICS` into `v12-bytecode`; re-export from
   `v12-bccompiler`. Move `Interp::from_source` into `v12-engine`/`v12-compile`.
5. Drop `v12-bccompiler` from `v12-interp` deps. Verify interp builds with *only*
   `v12-bytecode` + `v12-heap`.

### Phase 2 — Own shape state in the heap (ADR-002) — *medium risk*
6. Add `Heap::shape_of` / `Heap::bind_shape` owning the map; delete the
   `thread_local SHAPE_TABLE`.
7. Remove the interpreter's parallel validity-cell table; route through `Heap`.

### Phase 3 — Fix heap ownership (ADR-003) — *medium risk, biggest embed win*
8. `Interp::new(&mut Heap, …)`; remove `into_heap`/swap in `Engine`.
9. Borrow `retained` instead of cloning on `eval`/`run_jobs`.

### Phase 4 — Error & completion model (ADR-004) — *medium risk, API change*
10. Introduce `EngineError`; change `eval` to return the completion value.
11. Keep `JSException` internal only; add `to_display_string` on `EngineError`.

### Phase 5 — JIT layering + Tier-2 wiring (ADR-006)
12. Extract `v12-codegen`; rewire both JIT crates to depend on it.
13. `Engine::tier_policy` + second tier-up → `JitOpt::compile` + deopt backoff.

### Phase 6 — Facade (ADR-005) — *additive, do last over the cleaned core*
14. Build `v12-api` (`Runtime`/`Context`/`register_fn`/`call`/`pump`) strictly over
    the post-Phase-3/4 `Engine`. Repoint `v12-cli` onto `v12-api`.

### Phase 7 — Job-queue hygiene (ADR-007)
15. Move pending-microtask sink into the `Interp`; delete `set_pending` save/restore.

---

## 6. API structure improvements (public surface design)

### 6.1 Embedding facade (`v12-api`) — the only thing hosts import
```rust
pub struct Runtime;                 // configured factory (GC policy, intrinsics)
pub struct Context;                 // = one Engine, one realm, not Send
impl Context {
    pub fn new() -> Self;
    pub fn eval<T: FromValue>(&mut self, src: &str) -> Result<T, V12Error>;
    pub fn call<T, A>(&mut self, name: &str, args: &[A]) -> Result<T, V12Error>;
    pub fn register_fn<F>(&mut self, name: &str, f: F) -> Result<(), V12Error>;
    pub fn pump(&mut self) -> usize;          // drain microtasks
}
pub enum V12Error { Compile(String), Thrown(String), Host(String) }
```
(`Option<T>`/`Vec<T>`/`u64` `FromValue` already implemented in `value.rs`.)

### 6.2 `v12-engine` (internal orchestrator, not re-exported by facade)
```rust
pub struct Engine { /* owns Heap, Realm, JobQueue, NativeRegistry */ }
impl Engine {
    pub fn eval(&mut self, src: &str) -> Result<JsValue, EngineError>;  // completion value
    pub fn eval_module(&mut self, src: &str) -> Result<JsValue, EngineError>;
    pub fn call_global(&mut self, name: &str, args: &[JsValue]) -> Result<JsValue, EngineError>;
    pub fn run_jobs(&mut self) -> usize;
    pub fn heap(&self) -> &Heap;            // ALWAYS valid (ADR-003)
}
pub enum EngineError { Compile(CompileError), Thrown(JsValue), Host(String) }
```

### 6.3 `v12-interp` (transient runner, borrows the heap)
```rust
impl Interp {
    pub fn new(heap: &mut Heap, realm: &Realm, program: &Program, strings: &[String]) -> Self;
    pub fn run(&mut self) -> Result<(), JSException>;   // JSException stays internal
    // no more into_heap() / new_with_heap() / Heap swap
}
```

### 6.4 `v12-heap` (owns values, handles, shapes, GC)
```rust
impl Heap {
    pub fn shape_of(&self, obj: Handle<JsObject>) -> ShapeHandle;     // ADR-002
    pub fn bind_shape(&mut self, obj: Handle<JsObject>, s: ShapeHandle);
    // no more global SHAPE_TABLE
}
```

---

## 7. Verification & rollback

- **Guardrail:** `cargo nextest run --workspace` stays green at 484+; Test262
  `language` stays ≥ 4 958/24 873 after each phase.
- **Phase gates:** each phase is independently revertible via its own commit;
  Phases 1–4 are individually safe to ship (no public API yet). Phase 4 and 6
  change signatures — keep `eval_unwrap_value` / `v12-engine` legacy shims for one
  release.
- **Rollback:** every ADR is reversible (see per-ADR "Reversible" note). If Phase 3
  proves too invasive pre-facade, Phase 7 (job hygiene) and Phase 1 (dep decoupling)
  still ship independently.

## 8. Open questions for the team
1. Do we want `v12-bytecode` to keep carrying `oxc_span` (P3), or peel spans into a
   separate `v12-bytecode-span` so the ISA crate is front-end-free?
2. Is `EngineError::Host(String)` enough, or do embedders need structured host errors?
3. Single `v12-api` crate, or split `v12-api` (facade) vs `v12-sys` (unsafe JIT mem)?
4. Should `v12-cli` become the *reference consumer* of `v12-api` to dogfood the facade?
