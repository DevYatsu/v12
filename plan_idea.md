# v12 — A JavaScript Engine in Rust: Architecture & Execution Plan

Working name **v12**. All crates prefixed `v12-*`; the CLI binary is `v12`.
Plan revision: 2026-08-26 (grill-session revision). Library versions verified
against crates.io on 2026-08-25.

---

## 0. Goal, stated honestly

Build a correct, embeddable engine conforming to the **ES2026 ratified baseline**
(finished proposals included explicitly, listed in `conformance/`), competitive with
JavaScriptCore's baseline/DFG tiers and V8's Ignition+Sparkplug tiers.

v1 scope decisions (2026-08-25):
- **Annex B included** — sloppy-mode web compat; Test262 swaths assume it.
- **Single realm** — multi-realm post-v1.
- **Single-threaded mutator** — no Workers / `SharedArrayBuffer` / `Atomics` in v1;
  this underwrites the single-threaded heap and event loop design throughout.

What is realistic early: matching or beating V8 on startup time and memory footprint
(Rust + arenas + no legacy baggage). What is out of scope for v1: beating TurboFan on
hot code. That requires years of inline-cache and speculative-optimization tuning and
is a follow-on project once Tier 0/1 are solid.

## 1. Principles

1. **Reuse before rebuild.** If a maintained, spec-complete crate exists whose
   performance class fits, we depend on it. We build custom only where no such crate
   exists — and every custom row below says why nothing off-the-shelf qualifies.
2. **Correctness before speed.** Test262 must be green on the interpreter before any
   JIT work starts.
3. **Freeze narrow interfaces first.** Two artifacts unlock all parallel work:
   the bytecode ISA, and the heap layout (`JsValue` bits + handle API).
4. **Reversibility.** Every risky dependency sits behind a thin internal seam
   (GC trait, regex wrapper, code-emitter trait) so it can be swapped without
   touching call sites.
5. **Crates are dependency boundaries, not modules.** Subdivide only where
   dependency direction demands it; if two crates would need each other's
   exports, they are one crate. Nine crates total, each justified by what it
   may not import.

## 2. Execution pipeline

The entire language front end is **oxc**, not ours:

```
Source text
   │
   ▼
[oxc_parser] ──▶ [oxc AST] ──▶ [oxc_semantic: scopes, symbols, spans]
                                     │
                                     ▼
                            [v12-bccompiler]
                                     │
                                     ▼
                            [v12-bytecode]  (register ISA, const pool)
                                     │
                                     ▼
                            [v12-interp]                    Tier 0
                                     │ hot function / loop
                                     ▼
                            [v12-jit-baseline] (Cranelift)  Tier 1
                                     │ hot + stable type feedback
                                     ▼
                            [v12-jit-opt] (Cranelift + speculation)  Tier 2, post-v1
                                     │
                                     ▼
                                Machine code
```

Cross-cutting runtime services: value representation (NaN-boxing), shapes/hidden
classes, GC heap (handle-based), string table, built-ins.

## 3. Tooling decisions

| Component | Choice (version, Aug 2026) | Why | Rejected |
|---|---|---|---|
| Parser / AST / scopes | `oxc_parser` + `oxc_ast` + `oxc_semantic`, **pinned `=0.147.0`** | Fastest spec-compliant Rust JS parser; passes all Test262 parser tests + 99% Babel/TS; error recovery; continuously fuzzed; scope/symbol resolution included | Hand-written parser (weeks-to-months to reach parity we get day 1); `swc_ecma_parser` (heavier `swc_common` infra, monthly major bumps, weaker conformance claim) |
| Bytecode ISA, bytecode compiler, interpreter, value model, shapes | Custom (`v12-*`) | Core IP. Register-based bytecode (Ignition/LuaJIT style) — operand locations are static, which makes lifting to Cranelift IR direct. NaN-boxed `JsValue(u64)` and transition-tree shapes have no credible off-the-shelf crate | `boa_engine` internals (read as prior art; its stack-VM object model doesn't match our GC) |
| Garbage collector | **Custom handle-based heap** in `v12-heap`: 32-bit handles into arena segments, non-moving mark-sweep first; `mmtk-core` stays pluggable behind the `v12-heap` trait seam | See ADR-2. Handle-based heaps sidestep precise stack scanning for interpreted frames entirely (this is Nova's design) | `mmtk-core` today (see ADR-2 — self-declared not production-ready); `gc-arena` (phase-discipline model forbids mutation outside collection phases — Ruffle proves it ships, but it is a throughput ceiling for a JS engine) |
| JIT backend (Tiers 1+2) | `cranelift-frontend` / `-codegen` / `-module` / `-native`, 0.135.x | Fast compilation, memory-safe, active. **User stack maps landed mid-2024**: every call is a safepoint, spilled refs reload after each safepoint — moving-collector-safe JIT frames are now supported | Hand-written assembler à la YJIT (YJIT abandoned Cranelift for exactly this control, at a cost of years); LLVM (compile latency kills a JIT) |
| Code emission | Dev: `cranelift-jit`. Production: own `mmap` + relocation handling over `cranelift-module` output | `cranelift-jit` is verbatim "extremely experimental" upstream; Wasmtime itself maps compiled artifacts manually. `JITModule` doesn't expose stack maps — read function metadata ourselves, as Wasmtime does | `cranelift-object` + `dlopen` (kept as fallback) |
| RegExp | `regress` 0.12, wrapped by `v12-regex` | Purpose-built for ECMA-262: backreferences, variable-width lookbehind, named groups (incl. duplicates), `v` flag, UTF-16 input mode; Boa depends on it; co-maintained by Boa's lead | Rust `regex` (no backrefs/lookbehind); `fancy-regex` (Oniguruma flavor, lacks ES quirks — octal-vs-backref duality, unicodeSets case folding); custom engine (would duplicate a maintained, engine-proven one) |
| BigInt | `malachite` 0.10 (`malachite-nz`) — **LGPL-3.0-only** | GMP/FLINT-derived algorithms, pure Rust, actively developed (pushes within days of check); nothing else matches its arithmetic performance in pure Rust. License accepted deliberately — obligations (notices, library-source availability, relinkability of distributed binaries) are tracked in `deny.toml` | `rug` (GMP C dependency) |
| Intl / date-time | `icu` meta-crate 2.3.x (+ component crates) + `temporal_rs` 0.2.x | Browser-proven: Firefox vendors ICU4X by default; Chrome ships Temporal on `temporal_rs`; SpiderMonkey shipped Temporal default-on. `temporal_rs` replaces any "implement Temporal later" line item | Binding C++ ICU; `chrono` (not ECMA-402) |
| Identifier interning | `lasso` 0.7.3 at **compile time** (`Rodeo<Spur>` while compiling; freeze to `RodeoResolver` — consumers only resolve key→string, runtime keying belongs to the heap) | Renowned, small API; `Spur` u32 handles feed `PropKey` directly. Runtime key-internalization stays inside `v12-heap` — interned strings are GC-managed objects no external crate can own | `ustr` (global static table, breaks embeddability) |
| Arena allocation | `bumpalo` 3.20 (for our own arenas: const pools, compiler scratch) | Standard choice. Note: **oxc uses its own bump allocator internally** (via `allocator-api2`) — do not assume bumpalo types cross that boundary | — |
| Fuzzing | `cargo-fuzz` targets per crate + LibAFL; AST-mutator grammar fuzzing à la oxc's `shift-fuzzer-js` setup | GC and parser edges break under structure-aware fuzzing, not unit tests | — |
| Event loop / jobs | Custom minimal single-threaded microtask queue in `v12-engine` | Architecturally simple; the core must stay embeddable and executor-agnostic — no `tokio` inside the engine | — |

Reference architecture worth reading (not a dependency): `winch-codegen`
(Bytecode Alliance) — single-pass baseline compiler with a per-ISA `MacroAssembler`
trait; a good structural template for `v12-jit-baseline`.

## 4. Core data structures & algorithms

Performance-critical choices per crate, each following a named production
precedent. Defaults, not dogma — `bench/` gates deviations.

### v12-bytecode — instruction format
- **Fixed-width 32-bit instructions**: opcode + up to three operand fields;
  wide-prefix escape for rare oversized immediates (precedent: Lua 5.4, LuaJIT).
  O(1) decode, branch-free PC advance, direct lifting to Cranelift IR.
- Registers allocated like stack slots at compile time (expression-depth aware,
  freed on scope exit); frame size = compile-time max live.
- Constant pool deduplicated via hash map keyed on bit patterns during emission.
- Forward jumps: single-pass emission with backpatching (fixup chain per branch).
- Post-pass peephole: constant folding, dead-jump elimination; superinstruction
  fusion only where profiles demand (later milestone).
- **Capture policy**: static escape analysis marks captured variables; they live in
  heap Environment objects (dense slots, lazy name→slot map for direct eval /
  sloppy eval var-introduction / Annex B `with`), everything else stays registers.
- **Exception encoding**: static per-function handler table (sorted guarded ranges
  → handler PC + stack depth), zero cost on the non-throwing path; the same table
  drives unwinding through Tier-1 JIT frames.
- **Suspension**: one mechanism total — pausable frames. Generators wrap
  `{frame, state}`; `async`/`await` desugars to promise-driven resumption;
  async generators reuse it; Tier-1 resume points enter as OSR entries.

### v12-heap — values, shapes, strings
- **Frozen `JsValue(u64)` layout**: exponent ≠ 0x7FF → raw f64; NaN-space payloads
  carry a 4-bit tag selecting i31 Smi / HeapRef (object|string|symbol|bigint,
  32-bit handle) / specials incl. `hole` and `empty`; spare payload bits zeroed
  (canonical form asserted by property tests + fuzzing). No raw pointers ever live
  in a value ⇒ layout is portable down to wasm32.
- Handles are per-class index spaces (`Handle<JsObject>`, `Handle<V12Str>`…);
  the value's payload tag names the space, so downcasts need no header check;
  debug builds validate handle liveness against per-space epochs; a `--gc-stress`
  mode forces collection on a configurable allocation cadence from Phase 1.
- **Transition tree of shapes**: single parent link; child transitions in a small
  array upgraded to open addressing past ~8 siblings; descriptors inline while few,
  out-of-line array beyond (V8 Maps / JSC Structures).
- **Keys**: unified tagged-u32 property-key space over {interned string ids,
  symbol handles} shared by shape tables and stub cache. Intern-on-use: compile-time
  identifiers eagerly, runtime strings lazily on first use as a key. Well-known
  symbols are realm singletons; class `#private` names compile to unforgeable symbols.
- Megamorphic lookups go through a global open-addressed **stub cache** keyed by
  `(shape, name)` with secondary probe (V8 StubCache).
- **Invalidation protocol**: shapes are heap objects — transition trees branch
  freely, unreachable subtrees are GC-reclaimed (no deprecation/migration machinery).
  Prototype-chain guards check a **validity-cell serial** (`setPrototypeOf`,
  accessor add/remove bumps it); seal/freeze are integrity-level transitions to
  dedicated shapes. Invariant: every fast-path guard is exactly a shape pointer or
  a validity-cell serial — fail-closed, no exceptions.
- Property storage: in-object slots + contiguous out-of-object array with geometric
  growth; elements tagged by an **ElementsKind lattice** (packed smi → packed double →
  packed object → dictionary), writes generalize along the lattice only (V8).
- Dictionary-mode objects: Swiss-table structure (`hashbrown`) with cached key
  hashes (V8 SwissNameDictionary). Hasher policy: cheap integer mixing for
  engine-internal keys (handles, interned ids); seeded flood-resistant hasher
  (foldhash/ahash class) only where users control the keys — plain fast hashes
  there are a hash-flooding DoS.
- Strings: two encodings (Latin-1 8-bit | UTF-16); concatenation builds ConsStrings
  flattened past a length threshold; substrings become SlicedStrings; hash lazy and
  cached; identifiers interned (V8/JSC string model). Most real-world text never
  leaves the 8-bit encoding — large memory and memcmp win.
- Number ↔ string: shortest-roundtrip dtoa (Ryu-class) with ES formatting thresholds;
  parsing via Eisel-Lemire fast-float class algorithms.

### GC within v12-heap
- Bump-pointer allocation into 256KB–1MB segments; fast path is compare-and-increment.
- Non-moving mark-sweep: per-segment mark bitmaps; iterative marking with
  worker-local mark stacks spilling to a shared batch queue (never recursion —
  prototype chains and nested structures get deep).
- Lazy incremental sweeping: freed space returns through size-class segregated free
  lists; sweep budget charged against allocations.
- **Trigger**: heap-growth policy — collect when allocated-since-last-mark reaches
  ~live-at-last-mark (2× growth), floor 1MB, embedder-set ceiling; `idle()` hook;
  `--expose-gc` for tests; `--gc-stress` cadence from Phase 1.
- **Weak collections are ephemeron-correct from Phase 1**: `WeakMap`/`WeakSet` are
  handle-keyed side tables; deferred ephemeron tables drain to fixpoint at end of
  each mark cycle; sweep enqueues `FinalizationRegistry` targets as CleanupJobs on
  the job queue.

### v12-interp
- **Call model**: iterative dispatch loop over a frame vector; one contiguous,
  geometrically grown value stack — a frame's register file is the window
  `[base, base+maxregs)`. Frames are pausable data (generators/async suspend
  without native continuations); depth counter throws deterministic `RangeError`.
  Tier 0 never scans the machine stack; Cranelift user stack maps cover JIT frames.
- Dispatch: `loop` + `match` over the fixed-width encoding (Rust has no computed
  goto; this is what Wasmtime's Pulley interpreter does). Handlers small; unlikely
  paths `#[cold]`; bounds checks hoisted out of the fetch loop.
- Feedback: per-function FeedbackVector; slot index carried in an instruction
  operand; counters saturating u16.
- Tier-up triggers: saturating counters at function entry and loop headers set a
  flag consumed by the execution driver in `v12-engine`.

### v12-jit-baseline
- Template compilation: one Cranelift block per bytecode op; shapes/offsets baked
  as immediates.
- Inline caches: monomorphic = shape-guard compare + fixed-offset load inline;
  polymorphic ≤ 4-way branch chain; megamorphic calls the runtime stub cache.
- OSR: loop-header entry points translate interpreter frames from recorded layouts.
- **Runtime-call convention**: custom register convention passing
  `(engine_ctx, frame, args…)` as raw boxes/handles; non-moving heap ⇒ nothing to
  pin across calls. Invariant: a handle never lives only in a register across a
  call (Cranelift stack-map spills enforce it).
- **Exceptions**: check-after-call — runtime returns errors, template code branches
  to unwind stubs; ONE unwinder walks the shared per-function handler tables across
  tiers, materializing interpreter frames via deopt metadata when needed.
- **Deopt data split**: frame layouts + resume semantics owned by `v12-interp`;
  pc↔bytecode-pc and register-mapping tables emitted by the JIT in a format defined
  in `v12-bytecode` (reused by Tier 2 later).
- **Registry & gating**: `FunctionData` (bytecode handle, feedback vector, tier
  flags) owned by `v12-interp`; the JIT keeps its own code cache keyed by
  `FunctionId`; the engine driver consults both. JIT is a cargo feature (`jit`)
  and interpreter-only builds are first-class CI targets (free Tier0-vs-Tier1
  differential testing; W^X-hostile embedders stay supported).
- **W^X for the production path**: Linux = write-map → `mprotect(PROT_EXEC)` after
  relocation; macOS arm64 = `MAP_JIT` + `pthread_jit_write_protect_np` toggling;
  ARM64 icache flush after every IC patch. IC patching rewrites executable pages
  constantly — this is a designed-for constraint, not an afterthought.

### v12-engine
- **Internal-methods module**: the 13 spec internal methods (`[[Get]]`…`[[Construct]]`)
  dispatched over an object-kind function table; ordinary kinds direct, Proxy runs
  the trap algorithm with invariant enforcement. Fast paths (ICs, JIT guards) stay
  shape-guarded and proxy-blind by construction — a Proxy never matches a shape,
  so it always falls to the generic path.
- `Map`/`Set`/property enumeration require insertion order ⇒ dense entry vector +
  open-addressed hash index (**IndexMap pattern**), not plain hashbrown.
- Stable sort for `Array.prototype.sort` (ES2019 mandates stability) with
  reentrancy-safe element writes while user comparators run.
- **Modules**: ESM in core from Phase 2 — instantiate-then-evaluate spec order,
  top-level await rides the async desugar; host-injected `resolve`/`load` hooks
  (the CLI ships relative-path + file-URL defaults); CommonJS stays out of core.
  `eval`/`new Function` work from Phase 1 (oxc is already in-process).
- Microtasks: ring buffer (`VecDeque`). Timers, when added: hierarchical timing
  wheel, not a sorted list.
- **Embedder API**: spec microtask checkpoints run automatically after top-level
  scripts/modules/callbacks; everything else drains explicitly via `run_jobs()`
  (core never blocks on I/O). Values cross the boundary only as safe wrappers with
  `FromValue`/`ToValue` — handles never escape the crate. Host functions register
  against the internal-method table (trap-level control for free).
  `console`/timers/fetch stay host-side; the CLI registers console only.
  **Panics are engine bugs** — no unwind-to-JS masking; builtins target
  panic-freedom enforced by fuzzing.

YAGNI guard for v1: no sea-of-nodes IR, no concurrent marker, no generational write
barriers. Each is a Phase-3+ decision gated on profile data.

## 5. Decision records

### ADR-1: Adopt oxc wholesale for parsing, AST, and scope analysis
**Status:** Accepted.
**Context:** The previous revision planned a hand-written lexer/parser/AST with oxc as
a differential-testing oracle. Under the reuse-before-rebuild principle this spends
~10% of total engineering re-deriving the ecosystem's best-tested component.
**Decision:** Depend on `oxc_parser`/`oxc_ast`/`oxc_semantic`; `v12-bccompiler` walks
the oxc AST directly; spans come free via `oxc_span`.
**Consequences:** Weekly 0.x releases with breaking AST changes → pin exact versions,
upgrade deliberately. AST is shaped for tooling (spans everywhere, no parent links);
the bytecode compiler adapts to it rather than the reverse — accepted coupling, this
is the least reversible decision in the plan. Eliminates five planned crates
(lexer, parser, AST, semantic, span) and an entire workstream.

### ADR-2: Self-built handle-based heap now; mmtk-core remains pluggable
**Status:** Accepted (supersedes "use mmtk-core" from the previous revision).
**Context:** There is no production-grade GC framework to reuse. mmtk-core 0.33 is
actively developed but its own site says it is not ready for production; the CRuby
integration is an experimental MarkSweep-only gem measured ~4× slower than the
default GC at upstreaming time; the V8 binding is a NoGC experiment; bindings demand
implementing the full `VMBinding` trait family (OpenJDK's took years). Meanwhile
every existing Rust language VM rolls its own scheme, and the strongest Rust JS
engine (Nova) uses 32-bit handles into arena segments.
**Decision:** `v12-heap` owns a handle-based heap: values reference objects by index,
never by raw pointer, so roots are enumerable without stack scanning in interpreted
frames; JIT frames use Cranelift user stack maps. Ship NoGC (bring-up) → non-moving
mark-sweep. Do **not** hand-roll a moving/generational/concurrent collector first —
that remains true; it is the classic hobby-engine death trap.
**Consequences:** We own GC correctness (mitigated: non-moving first, fuzz under
allocation pressure, handles make object motion a later additive step — fix up
handles, not pointers). The `v12-heap` trait seam keeps an mmtk-core swap open for the
day it matures. (Judgment call: the handle indirection costs some access overhead;
Nova's existence argues it is compatible with a fast engine.)

### ADR-3: Cranelift for both JIT tiers, with a split emission path
**Status:** Accepted.
**Context:** Cranelift is the maintained, safe, fast-compiling option, and user stack
maps (2024) give precise, moving-safe safepoints. Its weaknesses: `cranelift-jit` is
experimental, generated-code peak quality trails LLVM/TurboFan, and dynamic-language
production precedents are scarce.
**Decision:** Tier 1 is a template JIT (one Cranelift block per bytecode op,
Sparkplug-style). Tier 2 adds SSA + type feedback + speculative guards with deopt.
Dev runs on `cranelift-jit`; production loads emitted code through our own mapper.
**Consequences:** Peak hot-loop performance is capped relative to LLVM-tier backends —
accepted for v1; revisit only if Tier 2 plateaus below target. Budget the majority of
JIT effort for ICs/OSR/deopt, which Cranelift does not provide.

### ADR-4: Every fast-path guard fails closed through shapes or validity cells
**Status:** Accepted.
**Context:** Inline caches cache lookup results; prototype mutations, accessor
changes, and seal/freeze can silently invalidate them. Wrong-code bugs (ICs serving
stale results) are strictly worse than crashes and are the classic failure mode of
new engines.
**Decision:** An optimization may ship in v1 only if its assumptions are observable
through exactly two guard primitives: a shape-pointer check or a validity-cell
serial check. Prototype-chain-affecting changes bump validity cells; integrity levels
(seal/freeze) become dedicated shape transitions; shapes are heap objects, so
transition trees branch freely and unreachable subtrees are GC-reclaimed — no
V8-style deprecation/migration machinery.
**Consequences:** Coarse invalidation re-specializes more ICs than necessary;
acceptable until profiles justify fine-grained dependent-code marking. Any future
fast path that cannot phrase its assumption as one of the two primitives does not
ship.

### ADR-5: Iterative interpreter over a contiguous value stack with pausable frames
**Status:** Accepted.
**Context:** JS requires exact `RangeError` stack-depth control, resumable
generators/async functions, and OSR/deopt over inspectable state. Native recursion
per JS call provides none of these safely in Rust (no guaranteed TCO, opaque native
stack).
**Decision:** One dispatch loop; calls push explicit `Frame` structs onto a frame
vector; register files are windows `[base, base+maxregs)` into a single
geometrically grown value stack; suspension records PC + state and nothing else.
**Consequences:** Slightly higher per-call overhead than native recursion — accepted,
since removing interpreter overhead is Tier 1's job. Generators/async come nearly
free; stack overflow becomes deterministic; GC never scans the machine stack for
interpreted frames.

## 6. Workspace layout

Granularity rule (Principle 5): a crate exists only where dependency direction
demands it; any two crates that would need each other's exports are one crate.
Nine crates:

```
v12/                                  (cargo workspace root)
├── Cargo.toml                        [workspace.dependencies] pins: oxc =0.147.0, etc.
├── crates/
│   ├── v12-bytecode/                 # register ISA defs, operand layout, const pool,
│   │                                 #   disassembler. Pure data, zero internal deps;
│   │                                 #   carries oxc Span pairs for traces/deopt.
│   ├── v12-heap/                     # THE foundation: Heap, Handle<T>, Trace, write
│   │                                 #   barriers, NaN-boxed JsValue(u64), Shape/
│   │                                 #   transition tree, NoGC + mark-sweep plans.
│   │                                 #   JsValue bits encode heap handles, so value
│   │                                 #   repr and GC are inseparable. Trait seam
│   │                                 #   reserved for a future mmtk backend.
│   ├── v12-bccompiler/               # oxc AST (+ oxc_semantic scopes) → bytecode.
│   │                                 #   Depends on: oxc-*, v12-bytecode
│   ├── v12-interp/                   # Tier 0 interpreter + type-feedback collection +
│   │                                 #   frame layout used by deopt/OSR.
│   │                                 #   Depends on: v12-bytecode, v12-heap
│   ├── v12-jit-baseline/             # Tier 1 template JIT (Cranelift), ICs, OSR.
│   │                                 #   May import v12-interp (frames, deopt resume);
│   │                                 #   never the reverse. Depends on: cranelift-*,
│   │                                 #   v12-bytecode, v12-heap, v12-interp
│   ├── v12-jit-opt/                  # Tier 2 speculative optimizer. Post-v1 milestone;
│   │                                 #   same import discipline as baseline.
│   ├── v12-regex/                    # ES RegExp semantics wrapper over regress:
│   │                                 #   lastIndex, species, exec side effects.
│   │                                 #   Depends on: regress only. Publishable standalone.
│   ├── v12-intl/                     # Intl + Temporal glue over icu + temporal_rs.
│   ├── v12-engine/                   # built-ins (Object/Array/String/Promise…, BigInt
│   │                                 #   via malachite) + realms + microtask queue +
│   │                                 #   execution driver (hot-tier handoff interp↔JIT)
│   │                                 #   + embeddable entry API. Depends on: all above,
│   │                                 #   plus malachite/icu/temporal_rs
│   └── v12-cli/                      # `v12` binary: REPL + script runner.
│                                     #   Depends on: v12-engine only.
├── fuzz/                             # cargo-fuzz targets: bccompiler, interp, heap, regex
├── conformance/                      # test262 harness + CI gating
└── bench/                            # criterion micro + JetStream2/Speedometer-class
                                      #   macro suites, regression-gated in CI from day one
```

Merge rationale (coupling is mutual):
- **`v12-heap`** = former value + GC crates. `JsValue` bit patterns contain heap
  handles; the two designs cannot evolve independently, and the old split bought an
  interface seam that churned on every change.
- **`v12-engine`** = former built-ins + runtime. Built-ins re-enter user JS
  (comparators, getters, proxies) and need the execution entry point, while realms
  must register built-ins — a direct mutual dependency.

Kept separate despite tight coupling:
- `v12-interp` vs `v12-jit-*`: coupling is strictly one-directional (the JIT reads
  interpreter frames); separating keeps Cranelift out of interpreter builds.
  Tier-up requests travel as flags consumed by `v12-engine`'s execution driver —
  never as interp→JIT imports.
- `v12-bytecode`: zero-dep stability anchor shared by compiler, interpreter, and
  both JIT tiers.

Dependency direction is acyclic: `bytecode` ← {`bccompiler`, `interp`, `jit-*`};
`heap` ← {`interp`, `jit-*`, `engine`}; `interp` ← `jit-baseline`; `engine` on top;
`cli` above `engine`.

## 7. Parallelism

With the front end outsourced to oxc, four lanes remain genuinely independent once
two interfaces are frozen (target: end of week 1):

1. **Bytecode ISA** (`v12-bytecode`) — enum + operand layout as data, no logic.
2. **Heap layout** (`v12-heap`) — `JsValue` bit encoding + `Heap`/`Handle<T>`/
   `Trace`/barrier API, NoGC backend first.

| Lane | Starts | Blocked by |
|---|---|---|
| `v12-bytecode` ISA design | Day 1 | Nothing |
| `v12-heap` (values + GC) | Day 1 | Nothing |
| `v12-regex` | Day 1 | Nothing (pure wrapper) |
| `v12-intl` | Day 1 | Nothing (icu/temporal_rs are self-contained) |
| `v12-bccompiler` | After ISA + oxc spike | ISA |
| `v12-interp` | After ISA + heap layout | ISA, heap |
| `v12-jit-baseline` | After ISA frozen; develop against stub interpreter | ISA, heap, interp (stub) |
| `v12-engine` built-ins | After heap API frozen | heap |

## 8. Phased plan

**Phase 0 (weeks 1–2): interfaces + spikes**
- Freeze the two interfaces above (docs + crate skeletons, no logic).
- oxc spike: parse a large real-world corpus, run `oxc_semantic`, walk the AST —
  validate ADR-1's coupling assumption before committing.
- `v12-heap`: allocation working under NoGC. `v12-regex`: regress wrapper skeleton.
- CI standing from day one: test262 harness, criterion benches, fuzz targets.

**Phase 1 (weeks 2–8): Tier 0 end-to-end**
- `v12-bccompiler` + `v12-interp`: parse → compile → interpret. First fully working
  engine. Push Test262 here, before any JIT exists.
- Swap NoGC → non-moving mark-sweep once real allocation patterns exist.
- Wire lasso interning into the compiler path.
- **Exit gate**: ≥60% of `test262/test/language`, zero fuzz crashes for 7
  consecutive nightly runs, jit and no-jit configurations produce identical
  results across the suite.

**Phase 2 (weeks 8–16): built-ins + baseline JIT, in parallel**
- `v12-engine` built-in breadth: enough stdlib to run real scripts (parallel lane;
  depends only on the now-stable heap API).
- `v12-jit-baseline`: template JIT, inline caches, OSR from the interpreter, stack
  maps wired to `v12-heap` from the first commit. Largest single item in the phase.
- Replace `cranelift-jit` with the production emission path.
- **Exit gate**: ≥85% Test262 overall, Tier 1 default-on with zero correctness
  delta vs interpreter across the full suite, JetStream2-class macro suite runs
  to completion.

**Phase 3 (months 4+): hardening, then Tier 2**
- Conformance long tail; benchmark gating vs V8/JSC (JetStream2/Speedometer-class).
- **Target: ≥95% overall**, remaining exclusions documented as explicit non-goals.
- Concurrent/GC maturity work only after everything else is stable.
- `v12-jit-opt`: speculative tier as its own project. Evaluate mmtk-core behind the
  `v12-heap` seam. (Judgment call: treat beating TurboFan as out of scope.)

**Operational matrix (grill session, 2026-08-26)**
- Macro gates: JetStream2-class + Speedometer3-class. Octane dropped — deprecated
  and gameable. Criterion micros cover IC dispatch, call overhead, GC pause length.
- CI targets: {x86_64, aarch64} × {linux, macOS, windows}; wasm32 is a
  compile-check only.
- MSRV pinned 1.90 (malachite's floor); CI runs stable; dependency-forced raises
  handled like any pinned-dep upgrade.
- `cargo-deny` enforced in CI: cranelift (Apache-2.0 + LLVM exception) and icu
  (Unicode-3.0) are known-good; v12 itself is dual MIT OR Apache-2.0.
- Fuzzing: local nightly cluster from Phase 0; OSS-Fuzz evaluated in Phase 3.

## 9. Where the effort actually lives

Revised from the previous revision: the parser slice went to ~zero (oxc), GC grew
(it is now ours).

- GC heap (handles, barriers, stack maps, mark-sweep): ~25%
- Bytecode compiler + interpreter: ~25%
- Baseline JIT (Cranelift integration, ICs, OSR, deopt): ~20%
- Object model (shapes, property storage, IC feedback): ~15%
- Built-ins + Test262 long tail (~50k tests): ~15%

Top risks, ranked:
1. **Deopt/OSR correctness** — largest single engineering item; no library helps.
2. **GC bugs under collection pressure** — mitigated by handles + non-moving first +
   fuzzing; still the likeliest source of heisenbugs.
3. **oxc churn** — pinned versions, deliberate upgrades; the coupling is permanent.
4. **`cranelift-jit` instability** — dev-only; production path is our own mapper.
5. **regress throughput ceiling** — backtracking worst cases match V8/JSC's baseline
   class; optimize in place only if profiling demands it.
