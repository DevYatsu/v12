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
- **Conformance output format** — for large runs (unfiltered, `language`, `built-ins`, …) always use the default **`--format human`**. Never use `--format json` or `--format tap` on big amounts of tests: they emit one record per test (tens of thousands) and consume far too many tokens. Reserve `tap`/`json` for small slices, and write them to a file via `--tap-out` instead of stdout.
- **Test262 pass rate** — `language/expressions` verified slice: **4 112/11 190 (36.8 %)** after the async-verdict work (2026-09-05, `60857f3`): +58 real passes (async tests execute and pass; `import()` rejections), −36 false passes removed (tests that only passed because a missing async verdict auto-passed sync `Ok` evals), 167 formerly-skipped `$DONE` tests now execute. Earlier: Step 8 (`24e838f`) + harness sta.js/assert.js fix + P0 (`1b0f73b`, with-statement `CompileError` honestly costs ~108 false passes on this slice) + P1 shared `lower_default`. Full `language` still times out in CI; last completed full-language score was 8 919/24 446 (36.5 %) after Step 7b (hidden-key; ~8 800 spec-correct). Baseline 19.9 % (4 858). Verified via `./conformance/run.sh --filter language/expressions --jobs 4`. Nextest gate is canonical.
- **cargo nextest** — **569 passed, 0 skipped** (`cargo nextest run --workspace`). Covers `v12-bytecode` decode sweeps (1.7 s with new WideOps 11-14 width 4), `v12-bccompiler` (133), `v12-interp`, `v12-engine` builtins, `v12-jit-*`, `v12-cli` spawns. Count drifted 563 → 569 via built-ins expansion lane (+6: cross-realm tests from the concurrent session).
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
- **P1 refactor** — intrinsic table is now single-copy in `v12_bytecode::GLOBAL_INTRINSICS` (18 realm names; `GLOBAL_VAR_OFFSET` derived from it) with the compiler superset as `GLOBAL_ACCESS_INTRINSICS`; all 87 `NATIVE_*` alias consts deleted (use `v12_native::NativeId::Variant` directly); bccompiler gained `FnCtx::global_name_id`/`with_loop`/`lower_default`; stmt.rs for-of helper copies deleted. Riding fix: assignment-target destructuring defaults (`[a = 1] = []`, `{x = 5} = {}`) previously silently skipped — now lowered via `lower_default`. Remaining P1: realm.rs `wire_callable`/`alloc-root` helpers, string.rs helpers, HostFn alias, proxy trap macro, `js_bool`, JIT op-arm tables; full binding-pattern/assignment-target walker merge deferred.

- **V12_JIT tier-1 wiring** — `V12_JIT` env var (default unset = pure interpreter; non-empty ≠ `0` = on) makes the engine install `v12_engine::jit_tier::JitTierHooks` on every interpreter it drives. On tier-up (1024 entries/loop crossings) the hook compiles the hot function once via `v12-jit-baseline` and caches the `CompiledFn`; shared stats handle exposes tier-ups/compiled/refused. The hook never *executes* compiled code — the baseline executor is heap-agnostic (strings → NaN, no interp re-entry), so execution delegation waits on OSR/deopt (post-v1). 3 tests in `jit_tier`.

- **P2/P3 refactor completed** (2026-09-05, through commit `9fa7d6b`) — P2 dead code deleted: jit-opt lost its entire Cranelift dependency set (`build_ssa_ir` output was discarded; speculative tier-2 API kept behind per-item `#[allow(dead_code)]`), jit-baseline dropped `cranelift-jit/module/native`, heap lost `inline_props`/`overflow`/`EnginePromise`/`JsObject::new`, engine lost `install_core`/`translate_value`/`eval_unwrap_value`, v12-native lost `native_table!`/`NativeSig`/`RuntimeRegistry` (~330 lines). P3 splits: v12-bytecode lib.rs 2,226 → 750 (opcode/wide/builder/analysis); v12-interp lib.rs 5,787 → ~1,600 (execute/property/call_setup/object_ops/globals/generator_async); engine.rs 1,617 → 926 (engine/eval, host_fn, display as child modules — field privacy preserved); builtins registry → builtins/registry.rs. P4: all mechanical clippy lints cleaned (`sort_by_cached_key` in array sort, collapsible_if, needless_borrow, etc.); remaining clippy output is the accepted unwrap/expect/panic policy set. `language/expressions` slice re-verified unchanged at 36.8 % after all of it.

- **Built-ins expansion** (2026-09-06, `docs/builtins-plan.md`) — hybrid architecture: pure natives via `define_builtins!` (`NativeHandler = fn(&mut Heap, JsValue, &[JsValue])`), callback-taking methods (Array map/filter/reduce/sort/…) intercepted at the interp seam (`Interp::run_callback_builtin` in `crates/v12-interp/src/call_setup.rs`, wired into BOTH call seams), context-centric `&mut Cx` refactor deferred as separate ADR. Verified conformance (human format, `--jobs 8`): Array 712/3 332 (21.4 %, now completes — was hanging on huge-length/sparse receivers), Math 95/327 (29.1 %, was 10.1 %), Number 137/340 (40.3 %, was 18.6 %), Object 552/3 414 (16.2 %, was 1.7 %), String 346/1 341 (25.8 %, was 7.7 %), JSON 28/165 (17.0 %, was 11.7 %), Boolean 13/51 (25.5 %, was 22 %), global 14/29 (48.3 %). Key fixes: RegExt-merged operands (`GetGlobal`/`SetGlobal`/`CallApply`/`CopyObjectRest`/`CreateGenerator`/`SuspendYield`/`Await` arms in `execute.rs` read `instr.a()` instead of merged `ra`/`rb`/`rc` — only past 255 registers); array-hang hardening (`dense_bound` helper in `builtins/array.rs`: max of element-store len and shape-bound integer keys, clamped to len, wired into scan loops + `callback_len` in call_setup.rs — engine deadline only covers dispatch loop); realm-global bias (`is_realm_global`/`realm_global_intrinsic_read` in globals.rs, `realm_globals` in gc.rs, ic_lookup intrinsic fallback, RealmEval arms ×5, Eval/Function seam arms); call_setup program-table (~35 Array entries in methods.rs); elements overflow clamp (sparse store shorter than length property — clamp all `elems[..]` slicing); Symbol constructor + statics (symbol.rs, `SymbolPrim`, realm wiring); Map/Set additions (forEach/clear/entries/keys/values + iterator wrappers); Iterator prototype methods (toArray/take/drop/from + callback seam helpers). Known minor: `map` results display as `[object Object]` in console.log (display-string path).

- **Async verdict + Promise surface** (2026-09-05, `2992c9d`+`60857f3`) — the runner executes async test262 tests: `doneprintHandle.js` injected for async/`$DONE` tests, verdict from `Test262:AsyncTest*` print markers (no marker = honest fail). Engine: `new Promise(executor)` via capability host closures sharing the registry pending-jobs sink (executor runs as a microtask job — natives cannot re-enter the interpreter); `Promise.prototype.catch`; await on a pending promise polls (re-queues) until it settles, with promise-adoption chains; promise state slots are Smis; dynamic `import()` returns a rejected promise. `language/expressions` 4 054 → 4 112 (36.8 %; 36 old false passes removed). Remaining async gaps: `Promise.all/race/finally`, async-generator `.next()` promise semantics, timers, async iteration. Module loader (`ModuleMap` resolve/link/evaluate) still pending — export-value extraction needs a module-env capture path in `run_compiled` (see `docs/language-coverage-plan.md` §1b row 11).

## Incident log

- **2026-09-02 workspace reset** — parallel fixer lanes shared one working tree and one lane ran `git stash` + `reset` to "preserve WIP", wiping the uncommitted Step 8 diff (and the fix-log Steps 1–7c backfill). Recovery: `stash@{0}` restored ~half, the erased files were rebuilt spec-first, everything landed as `24e838f`; the fix-log backfill was reconstructed from git-log commit messages. `stash@{0}` is retained untouched as a safety copy (its content now also lives in `24e838f`). Rules: commit verified work immediately; never fan out write lanes that touch the same files in parallel; no lane may run `git stash`/`reset`/`checkout` or workspace-wide `cargo fmt`.
