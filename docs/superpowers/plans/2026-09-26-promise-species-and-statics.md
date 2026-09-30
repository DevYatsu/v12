# Promise Species, Capability Observability, and New Statics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the remaining deferred Promise semantics — `NewPromiseCapability`, `Symbol.species` in `then`/`finally`, handler-result adoption, per-element `C.resolve`, `allSettled`, `any` (+ `AggregateError`), `withResolvers`, `try`, and `Reflect.construct(Promise, …)` — with zero regressions against the current 238/732 `built-ins/Promise` pass set.

**Architecture:** Interpreter owns the spec algorithm (`IsConstructor`, `Get`, `GetV`, `Call`, `Invoke`, `Construct`, `IsCallable`, iterator protocol); engine owns promise internal slots, `CreateResolvingFunctions` closures, reaction records, jobs, and combinator state via typed `NativeRegistry` methods. `JobCtx` holds `&mut Interp`, so a job can perform machine work even though a native cannot. No new cross-crate channel beyond one capability-record registry method.

**Tech Stack:** Rust workspace (`v12-interp`, `v12-engine`, `v12-heap`, `v12-native`, `v12-bytecode`), Test262 runner at `conformance/run.sh`.

**Spec:** ECMA-262 Promise Objects — ratified text is **clause 27.5** (older numbering 27.2): 27.5.1.3 CreateResolvingFunctions, 27.5.1.5 NewPromiseCapability, 27.5.2.1 NewPromiseReactionJob, 27.5.4.1 PerformPromiseAll, 27.5.4.2 PerformPromiseAllSettled, 27.5.4.3 PerformPromiseAny, 27.5.4.5 PerformPromiseRace, 27.5.4.8 Promise.try, 27.5.4.9 Promise.withResolvers, 27.5.5.3 finally, 27.5.5.4 then, 27.5.4.10 `Promise[Symbol.species]` accessor.

## Global Constraints
- Parent commit: `07612634`. Zero-loss parent TAP: `/tmp/v12-promise-species-parent.tap` (238/732). Every `ok` line in the parent must still be `ok` after every task.
- Workspace gate: `cargo nextest run --workspace` must stay at 839 passed, 0 failed, 0 skipped after every task.
- Zero new panics, zero new timeouts/STALLED. Treat `InternalError` failures as a distinct failure class when diffing.
- `promise.rs` and `call_setup.rs` are touched by every lane — **no parallel write lanes** (per the workspace incident log).
- No `git stash` / `reset` / `checkout`; commit verified work immediately; preserve untracked `.orchestrator-notes.md`, `ROADMAP.md`, `docs/`, `snip`.
- Use `snip` for shell. Never `cargo test`, never release mode. Big conformance runs use `--format human`; TAP diffs use `--format tap --tap-out <file>` and compare pass sets with the python pass-set diff (never dump TAP to stdout).
- Test262 pin: `d86b2294eb0a17eaa281ff12c73c473ec864c72f`.
- Ownership rule: interpreter drives the algorithm; engine owns the data it mutates.

---

### Task 0a: Spec-shaped resolving functions

**Files:**
- Modify: `crates/v12-engine/src/builtins/promise.rs:700-740` (`make_capability`)
- Modify: `crates/v12-engine/src/realm.rs:701-706` (add `function_proto()` beside `symbol_species()`)
- Modify: `crates/v12-engine/src/builtins/ctx.rs` (add `function_proto()` reader beside `symbol_species()`)
- Test: `crates/v12-engine/tests/promise_capability.rs` (new)

**Interfaces:**
- Consumes: existing `alloc_host` (`promise.rs:814-824`).
- Produces: `Realm::function_proto() -> JsValue`, `Ctx::function_proto() -> JsValue`; resolve/reject host closures with `length: 1`, `name: ""`, `[[Prototype]] = %Function.prototype%`, no own `prototype` property.

- [ ] **Step 1: Write the failing shape tests** in `crates/v12-engine/tests/promise_capability.rs`: for the capability passed to `new Promise(executor)`,
  - `Object.getOwnPropertyDescriptor(resolve, 'length').value === 1`, same for `reject`;
  - `resolve.name === ''`, `reject.name === ''`;
  - `Object.getPrototypeOf(resolve) === Function.prototype`, same for `reject`;
  - `Object.getOwnPropertyNames(resolve)` has no `'prototype'` entry and `resolve` is not constructible (`Reflect.construct(resolve, [])` throws — or `resolve.prototype === undefined`);
  - both are extensible and `length`/`name` are non-writable/non-enumerable/configurable.
- [ ] **Step 2: Run `cargo nextest run -p v12-engine --test promise_capability`; the `prototype`/`name` assertions must fail (the current closures carry the default host shape).**
- [ ] **Step 3: Implement** — in `make_capability`, after `alloc_host`, write `length = 1` then `name = ""` in that order (own-property order is observable: `reject-function-property-order.js`), set the function's `[[Prototype]]` to `Ctx::function_proto()`, and leave any own `prototype` property off. Add `Realm::function_proto()` returning the stored `%Function.prototype%` handle and the `Ctx` reader.
- [ ] **Step 4: Run the focused test, then `cargo nextest run -p v12-engine`, `cargo nextest run --workspace`, and the Promise filter; require zero pass-set loss.**
- [ ] **Step 5: Commit** `promises: build spec-shaped resolving functions`.

### Task A1: Pin engine-owned promise objects (GC soundness)

**Files:**
- Modify: `crates/v12-heap/src/gc.rs` (mirror the `module_roots` pattern at `:305`, `add_module_root` at `:479-483`, marking at `:1404-1406`, init at `:386-389`)
- Modify: `crates/v12-engine/src/builtins/promise.rs` (root sites `create_promise:93,103`, `promise_then:204`, `make_capability:712,717`, `alloc_host:822`, `queue_microtask:237`, `start_thenable_adoption:500,501`, `enqueue_thenable_adoption:532,533`, `promise_finally:1344`; module doc `:32-40`)
- Test: `crates/v12-engine/tests/promise_capability.rs` (append the soundness regressions)

**Interfaces:**
- Produces `Heap::add_pinned_root(&mut self, h: Handle<JsObject>)` and a `pinned_roots: Vec<Handle<JsObject>>` field marked in `force_collect`. Pinning is permanent (never unpinned): an over-approximation that trades reclamation for soundness, which is the contract the promise module doc already declares.
- Root cause: `Interp::gc_protect` (`crates/v12-interp/src/lib.rs:2318-2379`) does `roots.clear()` then republishes only stack/frames/pending lists/11 cached natives, so every `Heap::add_root` from the engine is discarded before `safepoint()` collects. The graph trace is correct; only the root set is wrong. Rejected alternatives: fixing `gc_protect` (the interp cannot see `Ctx`; `Heap` owns the root set), `is_alive` guards in `settle` (papers over a use-after-free), reusing `add_module_root` (conflates module-linker ownership with promise liveness).

- [ ] **Step 1: Reproduce and pin the failure.** The three-line repro panics with no async generator involved — `var n=0; Promise.resolve(1).then(function(v){n=v;}); Promise.resolve(2).then(function(v){n=v;}).then(function(v){n=v;});` panics at `gc.rs:595` via `promise.rs:339`. Also confirm `./target/debug/v12 --expose-gc /tmp/t3.js` (`async function* g(){ yield 1; } var it=g(); it.next().then(function(r){ it.next().then(function(r2){ it.next(); }); });`) gives the `promise.rs:340` out-of-bounds face, and the captured-`resolve` idiom (`var resolve; var p = new Promise(function(r){ resolve = r; }); p.then(function(x){ got = x; }); resolve(42);`) gives `TypeError: callee is not a function`. Add all three as nextest regressions in `promise_capability.rs`, using whatever GC-forcing affordance the existing engine tests already use (grep `force_collect` / `expose_gc` in `crates/v12-engine/tests` and `crates/v12-cli` first).
- [ ] **Step 2: Confirm the tests fail** with a stale-handle panic / OOB / dead-callee error before the fix.
- [ ] **Step 3: Implement** the `pinned_roots` field, `add_pinned_root`, the `force_collect` marking loop, and switch the nine promise-module `add_root` sites to `add_pinned_root`. Update the module doc to state the pinning contract explicitly and name the `gc_protect` interaction.
- [ ] **Step 4: Run the new regressions, `cargo nextest run --workspace` (839 + new tests, 0 failed), the Promise filter, and the AsyncGeneratorPrototype filter; require zero losses and expect large gains. Verify with the three CLI repros above (build first, then run the scripts) that no panic remains.**
- [ ] **Step 5: Commit** `promises: pin engine-owned promise objects across collections` (only `crates/v12-heap/src/gc.rs`, `crates/v12-engine/src/builtins/promise.rs`, `crates/v12-engine/tests/promise_capability.rs`).

### Task A2: Queue re-entrant async-generator requests while executing

**Files:**
- Modify: `crates/v12-interp/src/generator_async.rs` (slot consts at `:31,36`, enqueue at `:231-252`, pop at `:258-283`, `generator_next:400-455`, `generator_return:535-545`, `generator_throw` resume arm)
- Test: `crates/v12-interp/tests/async_generator_request_queue.rs` (append)

**Interfaces:**
- Adds an `ASYNC_GEN_EXECUTING_SLOT` set around `resume_generator`; while set, `generator_next`/`generator_return`/`generator_throw` only `enqueue_async_gen_request` and return the request promise without resuming (27.6.3.4 `AsyncGeneratorEnqueue`). Settling stays FIFO through the existing `pop_oldest_async_gen_request`.
- Disjoint write scope from every other lane: only `generator_async.rs` (plus its slot consts).

- [ ] **Step 1: Write the failing tests** mirroring the three Test262 shapes: a `next()` issued from inside the body while executing resolves after the in-flight request and before a later one; the same with `return(42)`; the `throw` sibling must reject rather than throw.
- [ ] **Step 2: Run** `cargo nextest run -p v12-interp --test async_generator_request_queue`; the re-entrant ordering assertions must fail.
- [ ] **Step 3: Implement** the executing-state gate.
- [ ] **Step 4: Run the focused test, `cargo nextest run --workspace`, and the AsyncGeneratorPrototype filter; require zero losses and that `request-queue-order-state-executing` (next/return/throw) all pass.**
- [ ] **Step 5: Commit** `async-generators: queue re-entrant requests while executing`.

### Task A3: Run the public `new Promise` executor synchronously

**Files:**
- Modify: `crates/v12-interp/src/call_setup.rs` (new `run_promise_construct`; arm in `run_callback_builtin` ~`:1663-1745`; `args_slice` rooting)
- Modify: `crates/v12-engine/src/builtins/promise.rs` (split `promise_construct` ~`:690-744` into `promise_capability`; delete the job push ~`:733-742`; delete the divergence doc ~`:682-689`; add `settle_now`)
- Modify: `crates/v12-native/src/registry.rs` + `crates/v12-engine/src/builtins/registry.rs` (`capability_settle`)
- Test: `crates/v12-engine/tests/promise_capability.rs` (append)

**Interfaces:**
- 27.5.1.1 steps 3-7: `callable_or_err` (reuse `call_setup.rs:3117`) → `is_capability_constructor` (reuse `:2005`) → `promise_capability` → synchronous `call_object(executor, undefined, [resolve, reject])` → return the promise unconditionally.
- Intercepting `NativeId::PromiseConstruct` in `run_callback_builtin` reaches it from `prepare_construct:1207-1210` **and** from `construct_object:2118-2123`, so `Reflect.construct(Promise, …)` starts working as a side effect.
- **Rooting trap:** `args_slice` (`call_setup.rs:1207`) is a Rust `Vec` local that `gc_protect` does not see. Push executor/resolve/reject onto `self.stack`, `gc_protect()`, then `truncate(base)` — the `stream_combinator` pattern (`:2965-2966`, doc `:2903-2906`).
- **Fix the pre-existing infinite loop in the same lane:** `new Promise(r => r({then: null}))` spins for the full 15 s deadline. `thenable_job`'s not-callable branch (`promise.rs:654-663`) calls `capability_settle`, which re-enters `start_thenable_adoption` (`:454-458`), reads `then` again, and enqueues forever. Add a `settle_now` entry point that skips adoption and use it there.
- Do **not** implement "constructor never calls the executor ⇒ TypeError": it is not in 27.5.1.1, `executor-not-callable.js` already passes (it only requires the *argument* to be non-callable), and `new Promise(function(){})` is legal and used by 4 passing tests.

- [ ] **Step 1: Write the failing tests** — captured `resolve` callable in the same turn; the executor runs *before* the statement after `new Promise`; a throwing executor rejects; `executor-call-context-strict` sees `this === undefined`; `new Promise(r => r({then: null}))` terminates instead of spinning.
- [ ] **Step 2: Fix the `capability_settle` loop first** and prove `new Promise(r => r({then: null}))` terminates. It is DoS-grade and is abort criterion 3 below.
- [ ] **Step 3: Re-take the baseline** — `call_setup.rs` is written by other lanes, so take `/tmp/exec-A0.tap` on your own HEAD, not a stale file.
- [ ] **Step 4: Implement** `run_promise_construct` + the engine split, with the stack-rooted call.
- [ ] **Step 5: A/B measure.** Parent: the 244-pass TAP for the current HEAD. Run `race_settles_with_the_first_element_in_reaction_order` in isolation first. Then diff `built-ins/Promise`, `built-ins/Proxy/construct` (a flip there was pinning a bug — read it before reverting), `language/async-function`, `built-ins/AsyncGeneratorPrototype`. Expected: ~44 Promise gains, zero losses.
- [ ] **Step 6: Abort and revert if** any `ok` becomes `not ok` (especially the 12 tick-order fixtures, `executor-call-context-strict.js`, `undefined-newtarget.js`); any `ok` is lost in `built-ins/Proxy/construct`; any new `InternalError` or clean-failure → 5 s STALLED appears; or the workspace gate is not green.
- [ ] **Step 7: Commit** `promises: run the constructor executor synchronously`.

### Task 0b: `NewPromiseCapability` via the capability executor, `super()` in place, `Reflect.construct`

**Files:**
- Modify: `crates/v12-native/src/registry.rs` (trait methods + `EmptyNativeRegistry` refusal)
- Modify: `crates/v12-engine/src/builtins/registry.rs` (implementations)
- Modify: `crates/v12-engine/src/builtins/promise.rs:641-695` (`promise_construct` split into two arms), `promise.rs:1092-1166` (element closures consumed later)
- Modify: `crates/v12-interp/src/call_setup.rs:2005-2027` (new `new_promise_capability`), `call_setup.rs:2031-2045` (`native_constructor_id` gains `PromiseConstruct`)
- Test: `crates/v12-engine/tests/promise_capability.rs` (append)

**Interfaces:**
- Produces:
  - `fn capability_executor(&mut self, heap, state) -> Result<JsValue, Throw>` — the `GetCapabilitiesExecutor` builtin (27.5.1.5 step 4-5): `length: 2`, `name: ""`, records `(resolve, reject)` into `state[0]`/`state[1]`, throws `TypeError` if either is already set (executor-called-twice), returns `undefined`.
  - `fn capability_pair(&mut self, heap, state) -> Result<(JsValue, JsValue), Throw>`.
  - `Interp::new_promise_capability(&mut self, ctor: JsValue) -> Result<(JsValue, JsValue, JsValue), JSException>` returning `(promise, resolve, reject)`; step 1 `IsConstructor`, step 6 `Construct(ctor, «executor»)` in place, step 7/8 both functions callable.

- [ ] **Step 1: Write the failing tests**: subclass construction `new (class extends Promise { constructor(a){ super(a); this.tag = 1; } })` fulfills and `instanceof` holds; `Reflect.construct(Promise, [executor])` fulfills; `Reflect.construct(Subclass, [executor])` yields the subclass; executor-called-twice throws `TypeError`; executor-not-callable (constructor never calls the executor) throws `TypeError`; capability resolve/reject are the exact functions the constructor received.
- [ ] **Step 2: Run the focused test; subclass and `Reflect.construct` cases must fail** (current `promise_construct` requires `new` and ignores `REG_THIS`).
- [ ] **Step 3: Implement**:
  - `state` is a plain 2-slot ordinary object `[undefined, undefined]` — a PromiseCapability Record. Build it with `Interp::alloc` and `gc_protect` it across `construct_object`.
  - `new_promise_capability`: `is_capability_constructor(ctor)` → `natives.capability_executor(heap, state)` → `construct_object(ctor, &[executor], ctor)` → `capability_pair` → two `callable_or_err` checks. The executor is a host function, so it runs synchronously inside `Construct` and needs no job.
  - `promise_construct` split: **Arm A** `this` is `Kind::Function` with `Bytecode(PromiseConstruct)` — unchanged; **Arm B** `this` is an object that is not `Kind::Function` and not already a promise — initialize `this` in place (`shape = heap.promise_shape()`, `kind = Kind::Promise`, `properties = [smi(PENDING), undefined, reactions]`) and return it. Gate arm B on both conditions; add a nextest test pinning the chosen behaviour for `Promise.call({})` (arm B initializes it as a promise; document as the one v1 divergence, real engines throw).
  - Add `NativeId::PromiseConstruct` to `native_constructor_id` so `run_reflect_construct` routes through `construct_object`; then delete the now-dead `PromiseConstruct` special case in `is_capability_constructor` and its apologetic comment.
- [ ] **Step 4: Run the focused test, `cargo nextest run --workspace`, `built-ins/Proxy/construct` before/after (measure; a flip was pinning a bug — read it before reverting), and the Promise filter; require zero loss.**
- [ ] **Step 5: Commit** `promises: construct capabilities through the executor`.

### Task 1a: `Symbol.species` accessor + `species_constructor` in `then`/`finally`

**Files:**
- Modify: `crates/v12-engine/src/realm.rs:417-434` (replace the species data property with an accessor)
- Modify: `crates/v12-engine/src/builtins/ctx.rs:563-600` (generalise `define_size_getter` to a symbol key and caller-chosen name)
- Modify: `crates/v12-engine/src/builtins/mod.rs` (`builtin_length` entry for the getter)
- Modify: `crates/v12-interp/src/call_setup.rs` (intercept `PromiseThen | PromiseCatch | PromiseFinally`; new `species_constructor`)
- Test: `crates/v12-engine/tests/promise_surface.rs` (rewrite three clauses), `crates/v12-engine/tests/promise_capability.rs` (append)

**Interfaces:**
- Produces: `NativeId::SymbolSpeciesGet` (`length: 0`, `name: "get [Symbol.species]"`); `Interp::species_constructor(&mut self, obj, default_ctor) -> Result<JsValue, JSException>` — 27.5.5.4 step 3: `Get(obj,"constructor")` → proxy-aware `IsConstructor` or throw → `undefined` → default → not-an-Object → throw → `Get(C, @@species)` → `null`/`undefined` → default → `IsConstructor` or throw. Keep it private; do not export a phantom abstract operation.

- [ ] **Step 1: Write the failing tests**: `Promise[Symbol.species]` is an accessor (`typeof descriptor.get === 'function'`, `descriptor.set === undefined`, `descriptor.value === undefined`); `Promise.then.call(p, ...)` on a subclass whose `Symbol.species` is a custom constructor yields that constructor's promises; `then` with a poisoned `constructor` getter rejects; with a non-constructor `constructor` rejects; `constructor` getter access count is 1.
- [ ] **Step 2: Run the focused tests; the descriptor and subclass cases fail.**
- [ ] **Step 3: Implement** the accessor and `species_constructor`; **rewrite in the same commit** the three workspace tests that pin the current data-property shape (`promise_surface.rs:85-97`, `100-132`; re-check `ordinary_promise_statics_override_and_deletion_win`).
- [ ] **Step 4: Run focused tests, `cargo nextest run -p v12-engine`, workspace, and the Promise filter; require zero loss.**
- [ ] **Step 5: Commit** `promises: select species in then and finally`.

### Task 1b: Handler-result adoption in reaction jobs

**Files:**
- Modify: `crates/v12-engine/src/builtins/promise.rs:177-226` (`promise_then` → `perform_promise_then` with capability), `promise.rs:250-275` (`reaction_job`), `promise.rs:830-847` (delete the dummy derived promise in `push_record`), `promise.rs:1389-1408` (`finally_settle_job`)
- Modify: `crates/v12-interp/src/call_setup.rs:3136-3161` (`run_promise_resolve` → full 27.5.4.7.1)
- Test: `crates/v12-engine/tests/promise_thenables.rs` (append)

**Interfaces:**
- Reaction record widens from `[fulfill, reject, derived]` to `[fulfill, reject, derived, resolve, rejectFn]`.
- `reaction_job` (27.5.2.1): `handlerResult = Call(handler, undefined, «arg»)`; abrupt → `Call(capability.[[Reject]], undefined, «err»)`; else → `Call(capability.[[Resolve]], undefined, «value»)` — the resolve closure is 27.5.1.3's, so thenables route through the existing `read_then` + `enqueue_thenable_adoption` with no new code.
- Do **not** route `settle_async_completion` (`promise.rs:750-763`) through the new path in this batch; do **not** make the public `new Promise(executor)` executor synchronous.

- [ ] **Step 1: Write the failing tests**: an `onFulfilled` returning a thenable adopts it; returning an object with a throwing `then` getter rejects the derived promise; returning `undefined` fulfills with `undefined`; an `onRejected` handler that returns a value fulfills the derived promise; handler-realm/derived-promise identity; `finally` still passes through values and rejections.
- [ ] **Step 2: Run `cargo nextest run -p v12-engine --test promise_thenables`; the adoption cases fail (the handler result is discarded).**
- [ ] **Step 3: Implement** `perform_promise_then` per 27.5.5.4.1 (species → capability → `PerformPromiseThen`), widen the record, call the capability from `reaction_job` through `JobCtx::interp_mut()`, and give `finally` the same treatment (`Call(callback)`, then `PromiseResolve(ctor, result)` + `Invoke(p,"then",…)` or `Call(capability.[[Reject]], …)`).
- [ ] **Step 4: Run `cargo nextest run -p v12-engine -E 'binary(async_promise) or binary(engine_async) or binary(promise_thenables)'`, `cargo nextest run -p v12-interp`, the `built-ins/Promise/prototype` filter, and the full Promise filter; require zero loss.**
- [ ] **Step 5: Commit** `promises: adopt handler results in reaction jobs`.

### Task 2a: Settle combinators through the capability

**Files:**
- Modify: `crates/v12-engine/src/builtins/promise.rs:1035-1072` (`combinator_settle`), `promise.rs:1092-1166` (delete `all_element`'s reject half and all of `race_element`)
- Modify: `crates/v12-interp/src/call_setup.rs:2907-2946` (`run_promise_combinator` order)
- Test: `crates/v12-engine/tests/promise_combinators.rs` (append)

**Interfaces:**
- `run_promise_combinator` order: `ctor = this` → `NewPromiseCapability(ctor)` → `GetPromiseResolve(ctor) = ? Get(ctor,"resolve")` (IsCallable or throw) → `GetIterator(iterable)` → stream → return `capability.promise`. Delete the standalone pre-check at `2928-2933` (`new_promise_capability` step 1 does it).
- Aggregate settlement goes through `Call(capability.[[Resolve]]|[[Reject]], …)` — the interpreter's `Call`, since the capability may be user code.
- Do **not** reintroduce the engine watcher removed in `07612634` (see the comment at `promise.rs:972-985`).

- [ ] **Step 1: Run `cargo nextest run -p v12-engine -E 'binary(promise_combinators)'` in isolation first — `race_settles_with_the_first_element_in_reaction_order` pins the exact `input,tick,tick2,handler:a` order and is the R1 canary.**
- [ ] **Step 2: Write the failing tests**: `all/call-resolve-element*` observes `callCount` of the constructor-provided resolve; the aggregate settles only through the capability (a constructor whose resolve/reject are counted).
- [ ] **Step 3: Implement** the new order and capability-driven settlement. If the R1 canary flips, bisect by keeping the per-element reject closure for one commit.
- [ ] **Step 4: Run the canary in isolation, the full combinator suite, workspace, and the Promise filter; require zero loss.**
- [ ] **Step 5: Commit** `promises: settle combinators through the capability`.

### Task 2b: `GetPromiseResolve` once, `nextPromise` `then` read

**Files:**
- Modify: `crates/v12-interp/src/call_setup.rs:3036-3048` (`combinator_element_calls` and the element pair)
- Modify: `crates/v12-native/src/registry.rs` (new method)
- Modify: `crates/v12-engine/src/builtins/promise.rs` (`combinator_step` element bookkeeping)
- Test: `crates/v12-engine/tests/promise_combinators.rs` (append)

**Interfaces:**
- `fn combinator_element_settled(&mut self, heap, state, index, fulfilled: bool, value: JsValue) -> Result<Option<JsValue>, Throw>` — engine mutates results + remaining count atomically and returns the value the aggregate's `[[Resolve]]` must be called with when this was the last element (`None` otherwise).
- Per element: `nextPromise = Call(promiseResolve, ctor, «element»)`; `then = GetV(nextPromise, "then")`; not callable → settle bookkeeping, continue; callable → `Invoke(nextPromise,"then",«pair»)` with pair `[onFulfilled, capability.[[Reject]]` for `all`/`allSettled` and `[capability.[[Resolve]], capability.[[Reject]]]` for `race`. The onFulfilled half becomes an interpreter-intercepted native carrying the state handle that performs the bookkeeping then `Call`s the returned value.

- [ ] **Step 1: Write the failing tests**: `invoke-resolve-get-once-*` (`getCount === 1`, `callCount === 0` for an empty iterator), `all/call-resolve-element`, `race/resolve-non-callable`, `same-reject-function` (`all` passes `resultCapability.[[Reject]]` verbatim to every element).
- [ ] **Step 2: Run focused; the get-once and call-resolve cases fail (the `Get` is per-element today).**
- [ ] **Step 3: Implement** — hoist `GetPromiseResolve` before `GetIterator`, read `then` off `nextPromise`, add the registry method, and rewire the element pair. Slot layout in `promise_combinator.rs` is unchanged.
- [ ] **Step 4: Run focused, workspace, and the Promise filter; require zero loss. Treat `InternalError` results as a separate failure class when diffing.**
- [ ] **Step 5: Commit** `promises: resolve combinator elements once`.

### Task 3a: `allSettled`, `withResolvers`, `try`

**Files:**
- Modify: `crates/v12-native/src/promise_combinator.rs` (add `KIND_ALL_SETTLED = 2`, `SLOT_ERRORS = 5`, bump `STATE_SLOTS` to 6 — append-only)
- Modify: `crates/v12-native/src/registry.rs`, `crates/v12-engine/src/builtins/registry.rs`
- Modify: `crates/v12-engine/src/builtins/promise.rs` (allSettled element closures sharing one `alreadyCalled` record)
- Modify: `crates/v12-engine/src/realm.rs:409-416` (statics table), `crates/v12-engine/src/builtins/mod.rs` (`builtin_length`)
- Modify: `crates/v12-interp/src/call_setup.rs:1690-1692` (combinator arm widened)
- Test: `crates/v12-engine/tests/promise_statics.rs` (new)

**Interfaces:**
- `allSettled` (27.5.4.2.1): result elements are ordinary objects with `status` + `value`/`reason` as `{writable, enumerable, configurable}` data properties; never rejects; empty → `[]`.
- `withResolvers` (27.5.4.9): `NewPromiseCapability(ctor)` + three `CreateDataPropertyOrThrow` with `Attrs::DEFAULT` (writable+enumerable+configurable — `withResolvers/result.js` verifies enumerable).
- `try` (27.5.4.8): `Call(callback, undefined, args)` then `NewPromiseCapability(ctor)` + `Call([[Reject]], …)` or `PromiseResolve(ctor, result)`.
- Do not add these statics to `promise_surface`'s synthesized read (`property.rs:405-450`) — bare-heap interpreters get none of them.

- [ ] **Step 1: Write the failing tests** in `promise_statics.rs`: `allSettled` result shapes and status values, never-rejects, empty iterable, iterable error → rejection; `withResolvers` result property attributes and resolvability; `try` resolve/reject/throw paths and argument forwarding.
- [ ] **Step 2: Run focused; the statics do not exist.**
- [ ] **Step 3: Implement** the new kind, the two per-element closures, the three statics, and the realm/native wiring.
- [ ] **Step 4: Run focused, workspace, and the Promise filter; require zero loss.**
- [ ] **Step 5: Commit** `promises: add allSettled, withResolvers and try`.

### Task 3b: `AggregateError` and `Promise.any`

**Files:**
- Modify: `crates/v12-bytecode/src/lib.rs:392-420` (append `"AggregateError"` at `GLOBAL_INTRINSICS` index 23 — append-only, never mid-insert)
- Modify: `crates/v12-engine/src/builtins/` (new `aggregate_error.rs`: `install_native` + `install_ctor`, `errors` at index 0, `name` `"AggregateError"` on the prototype)
- Modify: `crates/v12-native/src/promise_combinator.rs` (`KIND_ANY = 3`)
- Modify: `crates/v12-engine/src/realm.rs`, `crates/v12-interp/src/call_setup.rs` (`any` static + combinator arm)
- Test: `crates/v12-engine/tests/promise_statics.rs` (append)

**Interfaces:**
- `any` (27.5.4.3.1): `onFulfilled` **is** `capability.[[Resolve]]`; only `onRejected` is per-element and builds the `AggregateError` at zero; empty → `AggregateError` with `errors: []`; `errors` is `{configurable: true, enumerable: false, writable: true}`.
- `AggregateError` does not exist in this engine (zero hits) — it is a prerequisite, implemented minimally: only what `any` needs. `built-ins/AggregateError/*` as a full pass family stays out.

- [ ] **Step 1: Write the failing tests**: `new AggregateError([e1,e2])` shape (`errors`, `message`, `name`, `instanceof Error`); `any` resolves with the first fulfilled value, rejects with `AggregateError` carrying all reasons, empty → `AggregateError` with `errors: []`, iterable error → rejection.
- [ ] **Step 2: Run focused; the constructor is missing.**
- [ ] **Step 3: Implement** — append the intrinsic, add the constructor, then add `any` reusing the Task 2b element path with the `KIND_ANY` bookkeeping (snapshot `errors` at zero).
- [ ] **Step 4: Run focused, workspace, `built-ins/AggregateError` (new family, informational only), and the Promise filter; require zero loss in the Promise family.**
- [ ] **Step 5: Commit** — `errors: add AggregateError`, then `promises: add Promise.any` as two separate commits.

## Known limits carried forward
- **Cross-realm species and handler-realm errors**: single-realm v1 has no second `Realm` with a different `Symbol.species` identity; `Promise[Symbol.species]` returning `this` is the cross-realm mechanism and now works, but the tests stay deferred. Document in the Task 1a commit and in `realm.rs`.
- **`allKeyed` / `allSettledKeyed`** (89 tests): need Iterator Helper Objects (27.1.3.2), not built here — deferred.
- **`HostPromiseRejectionTracker`**: no unhandled-rejection reporting exists — deferred.
- **Env-slot `InternalError`** (`crates/v12-interp/src/lib.rs:1548`/`1583`): reproduced by `Promise.resolve.bind(Promise)` after a class-based custom `resolve`; makes 4 `invoke-resolve-on-promises-every-iteration-of-custom` tests crash rather than fail. Separate bug, separate commit; do not fix inside a Promise lane.
- **Public `new Promise(executor)` deferral is removed by Task A3.** An oracle A/B measured the claim that making it synchronous shifts `all`/`race` microtask ordering to be **false**: the `pending` sink is FIFO and adopted at the top of each drain iteration (`job_queue.rs:152-157`), so a deferred executor's follow-ups land where a synchronous settlement's would. All 12 tick-order fixtures in the pass set are byte-identical either way. The real risk is GC rooting across the call, not ordering.
- **`all/call-resolve-element*` and `all/resolve-before-loop-exit*` are unfixable, not deferred.** Those six tests assert the obsolete ES6-draft rule (`25.4.4.1.1`/`.2`) that `C.resolve` invokes the *executor-provided* resolve; the ratified spec calls `Get(C, "resolve")`. Do not chase them, and do not count them as a lane miss.
- Two pre-existing AsyncGeneratorPrototype request-queue panics: root-caused and fixed by **Task A1** (GC soundness: `gc_protect` wiping `Heap::add_root`) and **Task A2** (missing `executing`-state gate in `AsyncGeneratorEnqueue`). A1 is a soundness fix that also recovers a large share of the Promise failures, so it runs first.

## Final record (2026-09-27) — all tasks landed

`built-ins/Promise` **238 → 560 / 732 (32.5 % → 76.5 %)**, every lane zero-loss against its own parent TAP.

| Task | Commit | Promise Δ |
|---|---|---|
| 0a spec-shaped resolving functions | `0edcd0c9` | 238 → 244 |
| A1 GC pinned roots (soundness) | `d63341c7` | 244 (0 lost) |
| E1 `error_value` use-after-free (soundness) | `4a90b86e` | 244 (0 lost) |
| A2 async-generator re-entrant queue | `e5f26fd3` | 244 (0 lost) |
| A3 synchronous constructor executor | `4fdd8c6d` | 244 → 296 |
| 0b `NewPromiseCapability` + `super()` | `5fdcafc0` | 296 → 297 |
| 2a capability-driven combinator settlement | `7432a030` | 297 → 328 |
| 2b `Invoke` `then` on `nextPromise` | `e32319d7` | 328 → 337 |
| 3a `allSettled`/`withResolvers`/`try` | `9ccdf12a` | 337 → 448 |
| AggregateError | `1b5d8065` | 448 (0 lost) |
| 3b `Promise.any` | `9c7d0e79` | 448 → 532 |
| 1a `Symbol.species` accessor | `f4c8a654` | 532 → 539 |
| 1b handler-result adoption | `24923e9e` | 539 → 560 |

**Final gates:** workspace `1009 passed / 0 failed / 0 skipped`; clippy within the accepted policy set; `cargo fmt --check` clean; Promise STALLED 0.

**Other families:** `built-ins/AggregateError` 18/25, `built-ins/AsyncGeneratorPrototype` 23/48, `built-ins/Proxy/construct` 26/30, `language/statements/async-generator` 215/301, `language/expressions/async-generator` 451/623.

**Newly-found engine bugs, left for separate commits** (each is general, not Promise-specific):
- `Sub.resolve(1)` / `Promise.resolve.call(Sub, 1)` ignore the receiver — `promise_resolve` reads `heap.get(ctor).prototype` where the construct path reads the JS `C.prototype` property, so the derived promise's `[[Prototype]]` is the class object.
- A `let` declared before a `class` reads back clobbered after any later `Object.defineProperty(x, k, {get|set})`.
- Env-slot `InternalError` (above) — still crashing rather than failing.
- `call_capability` swallows a throwing `[[Resolve]]`/`[[Reject]]`, so a species constructor whose resolve throws leaves the derived promise pending forever.
- `promise::is_promise` requires exactly three internal slots, so a promise with a user-added own property stops answering as one (pinned by several `ctor-null`/`uniq-ctor` tests — deliberate, not a bug to fix quietly).
- No global has an own-property descriptor, so `verifyProperty` fails for every global — this is what keeps `prop-desc.js` failing across families.
