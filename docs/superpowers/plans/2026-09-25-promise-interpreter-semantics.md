# Promise Interpreter Semantics Implementation Plan

> **For agentic workers:** Execute task-by-task with one writer lane and review each pass set before continuing.

**Goal:** Improve `built-ins/Promise` coverage without breaking the single-realm interpreter by moving callback/iterator work through `Interp::call_value` and installing only the Promise surface that the current runtime can support honestly.

**Architecture:** Keep heap-only Promise state in `v12-engine` and route user-observable constructor/static methods through the existing `run_callback_builtin` interpreter seam. Add ordinary Promise prototype/statics before generalizing `all`/`race`; centralize thenable resolution only after the surface and iterable behavior are observable and tested. Defer cross-realm species, tick-exact ordering, and the remaining `allSettled`/`any`/`withResolvers` statics.

**Tech Stack:** Rust, v12 engine/interpreter, `cargo nextest`, Test262 runner.

**Spec:** `docs/superpowers/plans/2026-09-24-test262-coverage-acceleration.md` Task 8 and the ECMAScript Promise algorithms.

## Global Constraints

- Test262 pin: `d86b2294eb0a17eaa281ff12c73c473ec864c72f`.
- Canonical gate: `cargo nextest run --workspace`; never use `cargo test` as the workspace gate.
- Use `snip`; do not use `git stash`, `git reset`, or `git checkout`.
- One writer lane at a time; preserve untracked `.orchestrator-notes.md`, `ROADMAP.md`, `docs/`, and `snip`.
- Keep the single-realm v1 boundary; cross-realm constructor/species behavior is deferred.
- Every slice must compare against the saved Promise parent TAP set and report zero lost passes, zero new panics, and zero timeouts.
- Do not modify compiler residual code or claim the two pre-existing AsyncGeneratorPrototype request-queue panics are fixed.

---

### Task 1: Install the Promise prototype and static surface

**Files:**
- Modify: `crates/v12-engine/src/realm.rs` Promise intrinsic setup
- Modify: `crates/v12-engine/src/builtins/promise.rs` method tables/handlers
- Modify: `crates/v12-interp/src/property.rs` Promise synthesized-property fallback
- Create: `crates/v12-engine/tests/promise_surface.rs`

**Interfaces:**
- Preserve the landed `finally` watcher and existing reaction-job behavior.
- Ordinary installed properties take precedence over synthesized `promise_surface` reads.
- `Promise[Symbol.species]` and `Promise.prototype[Symbol.toStringTag]` are canonical per-realm symbol-keyed data properties with correct attributes.

- [x] **Step 1: Add failing surface tests.**

  Cover `Object.getOwnPropertyNames(Promise)`, `Promise.prototype` names, `Promise.resolve/reject/then/catch` callability and expected basic results, `Promise[Symbol.species] === Promise`, `Promise.prototype[Symbol.toStringTag]`, descriptor attributes, and deletion/override behavior.

- [x] **Step 2: Run the focused tests.**

  Run `cargo nextest run -p v12-engine --test promise_surface`; the new tests must fail before the surface is installed.

- [x] **Step 3: Install the surface.**

  Allocate the canonical species/toStringTag symbols once per realm, install `resolve`, `reject`, `then`, and `catch` on the appropriate prototype/constructor objects, and remove or defer synthesized reads whenever an ordinary property exists. Keep the engine handler available as a bare-heap fallback for standalone interpreters.

- [x] **Step 4: Run focused and Promise conformance gates.**

  Run `cargo nextest run -p v12-engine --test promise_surface`, `cargo nextest run -p v12-engine async_promise`, and the `built-ins/Promise` human filter. Compare the current TAP set with the saved parent; require zero lost passes.

- [x] **Step 5: Commit.**

  ```sh
  git add crates/v12-engine/src/realm.rs crates/v12-engine/src/builtins/promise.rs \
    crates/v12-interp/src/property.rs crates/v12-engine/tests/promise_surface.rs
  git commit -m "promises: install prototype and static surface"
  ```

**Task 1 implementation record (2026-09-25):**
- Surface implementation: `1880fdf9`; 11 focused surface tests passed.
- Promise TAP: `103→133` (`+30`, zero losses); workspace gate `798 passed`; clippy and fmt passed.
- Installed `resolve`/`reject`/`then`/`catch`, canonical `Symbol.species`/`Symbol.toStringTag`, and realm-aware Promise native errors. Cross-realm species and interpreter-created async-result prototypes remain deferred.

---

### Task 2: Generalize `Promise.all` and `Promise.race` inputs

**Files:**
- Modify: `crates/v12-interp/src/call_setup.rs` native callback routing
- Modify: `crates/v12-interp/src/object_ops.rs` iterator helpers only if needed
- Modify: `crates/v12-engine/src/builtins/promise.rs` standalone fallback handlers
- Create: `crates/v12-engine/tests/promise_combinators.rs`

**Interfaces:**
- Intercept `NativeId::PromiseAll` and `NativeId::PromiseRace` in `run_callback_builtin`, following the Task 7.3 `MapConstruct`/`SetConstruct` pattern.
- Use `get_iterator`, `op_iterator_next`, `op_iterator_close`, and `Interp::call_value`; keep the existing heap-only handlers as standalone fallbacks.

- [ ] **Step 1: Add failing combinator tests.**

  Cover `Promise.all(new Set([...]))`, a custom iterable, `Promise.race` with a custom iterable, `next`/value/adder-like resolution failures, iterator close, and input order. Assert result ordering and exact rejection values.

- [ ] **Step 2: Run the focused tests.**

  Run `cargo nextest run -p v12-engine --test promise_combinators`; current array-only behavior should fail the general-iterable cases.

- [ ] **Step 3: Implement the interpreter seam.**

  Resolve the input iterable once, consume values in order, call each element's constructor/resolve capability through `Interp::call_value`, and preserve the existing reaction watcher (`push_record`). Use canonical `call::alloc_array` for the result array. On abrupt iteration/resolution, call the existing close path and preserve error precedence.

- [ ] **Step 4: Run focused and regression gates.**

  Run the new test, `cargo nextest run -p v12-engine async_promise`, `cargo nextest run -p v12-interp`, and the Promise conformance filter. Compare TAP pass sets against the Task 2 parent; require zero losses.

- [ ] **Step 5: Commit.**

  ```sh
  git add crates/v12-interp/src/call_setup.rs crates/v12-interp/src/object_ops.rs \
    crates/v12-engine/src/builtins/promise.rs crates/v12-engine/tests/promise_combinators.rs
  git commit -m "promises: accept general iterables in all and race"
  ```

**Task 2 implementation record (2026-09-25):**
- Implementation: `3d7489fd`; 14 combinator tests passed.
- Promise TAP: `133→136` (`+3`, zero losses); workspace gate `812 passed`; clippy and fmt passed.
- Interpreter seam owns iterable draining and close; engine handler retains reaction aggregation. Sub-species, cross-realm, and thenable behavior remain deferred.

---

### Task 3: Add thenable adoption without a second scheduler

**Files:**
- Modify: `crates/v12-engine/src/builtins/promise.rs` resolution/reaction helpers
- Modify: `crates/v12-engine/src/job_queue.rs` only if the existing pushed-job sink is insufficient
- Modify: `crates/v12-interp/src/generator_async.rs` await resolution path
- Create: `crates/v12-engine/tests/promise_thenables.rs`

**Interfaces:**
- A single resolution helper reads an arbitrary object's `then` through the ordinary property path and validates callability.
- Already-settled engine Promises retain the existing reaction-job behavior; do not settle synchronously in a new path.
- `await` on a thenable must resume through the existing pending-await/settlement machinery; do not spin synchronously on a pending Promise.

- [ ] **Step 1: Add failing thenable tests.**

  Cover `Promise.resolve({then(resolve){...}})`, a throwing `then` getter, nested adoption, `await {then(resolve){...}}`, rejection propagation, and a never-settling thenable that leaves the checkpoint quiescent without hanging.

- [ ] **Step 2: Run the focused tests.**

  Run `cargo nextest run -p v12-engine --test promise_thenables`; current engine-Promise-only adoption should fail the arbitrary-thenable cases.

- [ ] **Step 3: Implement adoption through the existing job/settlement seam.**

  Read `then` with `get_property`, reject a present non-callable value, and enqueue the continuation through the existing pending sink. For interpreter `await`, recognize a thenable and park/resume through the current await queue; keep engine Promise state and reaction ordering unchanged.

- [ ] **Step 4: Run focused and language gates.**

  Run the new test, `cargo nextest run -p v12-engine async_promise`, `cargo nextest run -p v12-interp async`, and Promise/async language filters. Require zero lost passes against the current parent.

- [ ] **Step 5: Commit.**

  ```sh
  git add crates/v12-engine/src/builtins/promise.rs crates/v12-engine/src/job_queue.rs \
    crates/v12-interp/src/generator_async.rs crates/v12-engine/tests/promise_thenables.rs
  git commit -m "promises: adopt arbitrary thenables"
  ```

**Task 3 implementation record (2026-09-25):**
- Thenable adoption: `6171a0a9`; streaming combinator correction: `e66793f7`; reaction-order/constructor fix: `07612634`.
- Promise TAP: `136→238` (`+102`, zero losses, zero STALLED); combinator tests `26/26`; thenable tests `14/14`.
- Workspace gate: `839 passed`; clippy and fmt passed. Reserved Promise shapes and rooted async-generator requests fixed two engine-slot/GC hazards exposed by the streaming tests.
- Engine fallback paths, species/capability observability, cross-realm behavior, and reaction-handler return adoption remain deferred.

---

### Task 4: Close the Promise lane and record remaining statics

**Files:**
- Modify: `docs/superpowers/plans/2026-09-24-test262-coverage-acceleration.md`
- Create: `target/test262-coverage/promise-final.txt` only as a local artifact

- [x] **Step 1: Run final gates.**

  ```sh
  cargo nextest run --workspace
  cargo clippy --workspace --all-targets
  cargo fmt --check
  ./conformance/run.sh --filter 'built-ins/Promise' --jobs 8 --format human
  ./conformance/run.sh --filter 'language/expressions/async-generator' --jobs 4 --format human
  ./conformance/run.sh --filter 'language/statements/async-generator' --jobs 4 --format human
  ./conformance/run.sh --filter 'language/statements/for-await-of' --jobs 4 --format human
  ```

- [x] **Step 2: Record exact deltas and known limits.**

  Record the commit, Test262 pin, filters, pass/fail/skip counts, TAP artifacts, zero-loss result, pre-existing AsyncGeneratorPrototype panics, and deferred cross-realm/species/tick-order work. Do not add `allSettled`, `any`, or `withResolvers` in this lane.

- [x] **Step 3: Commit the plan record.**

  ```sh
  git add docs/superpowers/plans/2026-09-24-test262-coverage-acceleration.md
  git commit -m "docs: record Promise coverage gains"
  ```

**Task 4 final record (2026-09-25):**
- Final Promise human gate: `238/732` (32.5%), zero STALLED; TAP versus the Promise parent `136/732`: `+102`, zero losses.
- Final async gates: async-generator expressions `451/623`; statements `215/301`; for-await-of `1191/1235`; AsyncGeneratorPrototype `20/48`.
- Workspace: `839 passed`; clippy and fmt passed.
- Remaining deferred work: species/capability observability, cross-realm constructors, `allSettled`/`any`/`withResolvers`, reaction-handler return adoption, and the two pre-existing AsyncGeneratorPrototype request-queue panics.
