# Language Async Residuals Implementation Plan

> **For agentic workers:** Execute task-by-task with one writer lane and review each pass set before continuing.

**Goal:** Raise language Test262 coverage by fixing async `yield*`, general-iterable binding patterns, binding-pattern `IteratorClose`, and async-generator request FIFO correctness.

**Architecture:** Keep the change in the compiler/interpreter boundary. Lower binding patterns through the existing `GetIterator`/`IteratorStep`/`IteratorClose` protocol instead of adding a second iterator implementation. Mark async-generator suspension states in the existing generator object slots, replacing the single pending request slot with a rooted FIFO. Promise built-ins remain a separate later lane because their native registry signatures cannot perform interpreter re-entry.

**Tech Stack:** Rust, OXC AST, v12 bytecode/interpreter, `cargo nextest`, Test262 runner.

**Spec:** `docs/superpowers/plans/2026-09-24-test262-coverage-acceleration.md` Task 8 review and the ECMAScript iterator/async-generator algorithms.

## Global Constraints

- Test262 pin: `d86b2294eb0a17eaa281ff12c73c473ec864c72f`.
- Canonical gate: `cargo nextest run --workspace`; never use `cargo test` as the workspace gate.
- Use `snip`; do not use `git stash`, `git reset`, or `git checkout`.
- One writer lane at a time; preserve untracked `.orchestrator-notes.md`, `ROADMAP.md`, `docs/`, and `snip`.
- Do not implement WeakMap, the global Iterator family, cross-realm species, or Promise registry refactors in this plan.
- Every slice must compare its current TAP pass set to the saved parent and report zero lost passes, zero panics, and zero timeouts.

---

### Task 1: Await async `yield*` values and preserve iterator results

**Files:**
- Modify: `crates/v12-bccompiler/src/expr.rs:269-387,865-908`
- Modify: `crates/v12-interp/src/execute.rs:1043-1162`
- Modify: `crates/v12-interp/src/generator_async.rs:274-327,572-727`
- Create: `crates/v12-interp/tests/async_yield_star.rs`

**Interfaces:**
- `YieldExpression { delegate: true }` remains lowered by `expr.rs`.
- `SuspendYield` continues to mark a user-visible yield; `Await` remains the internal suspension mechanism.
- Async-generator request settlement continues through `settle_async_gen_request_slot`.

- [x] **Step 1: Add failing async yield-star tests.**

  Add tests for an async generator yielding a delegated promise, awaiting a promise sent back through `next(value)`, a delegated iterator returning `{ value, done: true }`, delegated `return(value)`, and a delegated iterator whose `next` throws. Assert the exact iterator-result values and final generator completion.

- [x] **Step 2: Run the focused tests to confirm the current failures.**

  Run:

  ```sh
  cargo nextest run -p v12-interp --test async_yield_star
  ```

  Expected: the new tests fail because the raw delegated promise is yielded and the delegated completion is not consistently returned/awaited.

- [x] **Step 3: Lower async yield-star suspension through `Await`.**

  In the delegate loop, after reading `result.value`, emit `Await` into the yielded-value register when the current function is an async generator, then emit `SuspendYield`. On resume, await the injected `next(value)` before forwarding it to the delegated iterator. Keep sync generators on the existing direct `SuspendYield` path. Preserve `emit_delegate_return_path` and its existing `GenResumeMode` ordering.

- [x] **Step 4: Preserve async-generator completion settlement.**

  Ensure the first request promise is settled only after the awaited yield value is available. Keep internal awaits parked and real yields settled. Do not change sync generator result wrapping or the existing return/finalizer trampoline.

- [x] **Step 5: Run focused and sync-generator control tests.**

  Run:

  ```sh
  cargo nextest run -p v12-interp --test async_yield_star
  cargo nextest run -p v12-interp --test generator_next
  cargo nextest run -p v12-interp --test generator_suspend
  cargo check --workspace --all-targets
  ```

- [x] **Step 6: Run the language pass-set gate.**

  Run `language/expressions/async-generator` and `language/statements/async-generator` in human format, compare against `/tmp/v12-task8-parent-async-expression.tap` and `/tmp/v12-task8-parent-async-statement.tap`, and require zero lost passes.

- [x] **Step 7: Commit.**

  ```sh
  git add crates/v12-bccompiler/src/expr.rs crates/v12-interp/src/execute.rs \
    crates/v12-interp/src/generator_async.rs crates/v12-interp/tests/async_yield_star.rs
  git commit -m "async-generators: await delegated yields"
  ```

**Task 1 implementation record (2026-09-25):**
- Implementation: `03343e71`; formatting follow-up: `bd908331`.
- Focused async-yield tests: `7/7`; sync generator controls passed; workspace gate: `743 passed`; clippy and fmt passed.
- Pass sets versus the Task 8 parent: async-generator expressions `337→426` (`+89`, zero losses); async-generator statements `160→204` (`+44`, zero losses); for-of `1149→1149`; AsyncGeneratorPrototype `15→15`.
- The two intermediate parent flips were corrected: manual async-iterator values remain raw, while AsyncFromSyncIterator values are awaited; finalizer return completion values survive an intermediate yield.
- Test262 pin: `d86b2294eb0a17eaa281ff12c73c473ec864c72f`.

---

### Task 2: Lower array binding patterns over general iterables

**Files:**
- Modify: `crates/v12-bccompiler/src/stmt.rs:452-485,502-539`
- Modify: `crates/v12-interp/src/object_ops.rs:10-45,332-396,478-516,594-606`
- Modify: `crates/v12-bccompiler/src/expr.rs:1356-1415` only if assignment lowering shares the helper
- Create: `crates/v12-interp/tests/binding_iterables.rs`

**Interfaces:**
- Reuse `GetIterator`, `IteratorStep`, `IteratorNext`, and `IteratorClose`; do not add a native-only iterable path.
- `RequireObjectCoercible` remains the behavior for `null`/`undefined` sources.
- `CopyArrayRest` remains available for array spread/assignment paths that require array semantics.

- [ ] **Step 1: Add failing binding tests.**

  Cover `let [a, b] = Set`, `var [head, ...tail] = new Set([1, 2, 3])`, a custom iterator with a callable `return`, nested patterns, defaults that throw, and `null`/`undefined` sources. Include a sync-generator control showing that ordinary array binding remains unchanged.

- [ ] **Step 2: Run the focused tests to confirm the current failures.**

  Run:

  ```sh
  cargo nextest run -p v12-interp --test binding_iterables
  ```

  Expected: non-array rest reports `spread/rest source is not an array` and fixed patterns do not consume general iterables.

- [ ] **Step 3: Add a compiler helper for iterator-backed array binding.**

  Replace the fixed-index `GetProperty` plus `CopyArrayRest` sequence in `array_pattern_store` with a lowered loop: obtain the iterator once, call `next` for each non-elision element, read `done` and `value`, lower the nested binding/default against the value, and collect rest values into a canonical array. For an all-elision pattern, still emit `RequireObjectCoercible` before returning.

- [ ] **Step 4: Preserve abrupt completion and close guards.**

  Wrap iterator acquisition, step, value access, default evaluation, and binding stores in the existing exception-range shape. Use the existing `close_guard` convention so a throw path closes once, preserves the original exception, and does not double-close when a close error re-enters the handler.

- [ ] **Step 5: Run focused tests and compiler gates.**

  Run:

  ```sh
  cargo nextest run -p v12-interp --test binding_iterables
  cargo nextest run -p v12-bccompiler
  cargo check --workspace --all-targets
  ```

- [ ] **Step 6: Run language gates.**

  Run `language/statements/for-await-of`, `language/expressions/async-generator`, and `language/statements/async-generator`; compare with all three saved Task 8 parent TAP sets and require zero losses.

- [ ] **Step 7: Commit.**

  ```sh
  git add crates/v12-bccompiler/src/stmt.rs crates/v12-interp/src/object_ops.rs \
    crates/v12-bccompiler/src/expr.rs crates/v12-interp/tests/binding_iterables.rs
  git commit -m "bindings: lower array patterns over iterables"
  ```

**Task 2 implementation record (2026-09-25):**
- Implementation: `71af3b4e`; focused binding tests `7/7`; compiler tests `184/184`.
- Pass sets versus the Task 2 parent: for-await-of `1149→1191` (`+42`, zero losses); async-generator expressions `426→434` (`+8`, zero losses); statements `204→208` (`+4`, zero losses).
- Workspace gate: `750 passed`; clippy and fmt passed. The ordinary-array fast path and sync-generator controls remain green.

---

### Task 3: Close iterators on binding-pattern abrupt completion

**Files:**
- Modify: `crates/v12-bccompiler/src/stmt.rs:452-485,660-793`
- Modify: `crates/v12-interp/src/object_ops.rs:518-585`
- Modify: `crates/v12-interp/tests/binding_iterables.rs`

**Interfaces:**
- `IteratorClose` uses the existing throw-path flag: close errors are swallowed when the original completion is a throw and validated/propagated for normal break/return completions.
- The existing for-of `close_guard` remains the single implementation pattern.

- [ ] **Step 1: Add close-behavior tests.**

  Use iterators whose `return` increments a counter, throws, returns a primitive, or is absent. Cover errors from `next`, `value`, a nested default initializer, a binding store, and a `return` method that re-enters the same pattern.

- [ ] **Step 2: Verify the current close gap.**

  Run the new tests and confirm that a binding-pattern error leaves the iterator open where the test expects one close.

- [ ] **Step 3: Implement the close range.**

  Extend the iterator-backed binding helper with the same exception-range and `close_guard` layout used by `for_of_loop`. Preserve the original error on the throw path and prevent a second close if a close error re-enters the handler.

- [ ] **Step 4: Run focused and regression gates.**

  Run:

  ```sh
  cargo nextest run -p v12-interp --test binding_iterables
  cargo nextest run -p v12-interp --test async_generator_throw
  cargo nextest run -p v12-bccompiler
  cargo check --workspace --all-targets
  ```

- [ ] **Step 5: Run language gates and commit.**

  Re-run the three Task 8 language filters, compare against the saved parents, require zero losses, then:

  ```sh
  git add crates/v12-bccompiler/src/stmt.rs crates/v12-interp/src/object_ops.rs \
    crates/v12-interp/tests/binding_iterables.rs
  git commit -m "bindings: close iterators on abrupt completion"
  ```

**Task 3 implementation record (2026-09-25):**
- Iterator close: `62b522de`; nested array fast-path preservation: `d7838862`.
- Focused tests: binding iterables `22/22`; array binding close `8/8`; nested array binding `8/8`; compiler `184/184`; standalone `v12-interp` green.
- Pass sets versus the Task 3 parent: async-generator expressions `434→438` (`+4`, zero losses); statements `208→210` (`+2`, zero losses); for-await-of `1191→1191`.
- Workspace gate: `781 passed`; destructuring slice `18 passed, 1 skipped`; clippy and fmt passed.
- The direct array fast path now carries known-array information through nested patterns; general iterables still use the iterator protocol.

---

### Task 4: Make async-generator requests FIFO and rooted

**Files:**
- Modify: `crates/v12-interp/src/generator_async.rs:11-20,160-249,274-327,360-473`
- Create: `crates/v12-interp/tests/async_generator_request_queue.rs`

**Interfaces:**
- Replace the single `ASYNC_GEN_REQUEST_SLOT` value with a rooted queue object/array stored in a dedicated generator slot.
- `generator_next`, `generator_return`, and `generator_throw` append request promises; settlement removes the oldest request.

- [x] **Step 1: Add concurrent-request tests.**

  Queue two or three `next()` calls while an async generator is parked on `await Promise.resolve(...)`; assert every returned promise settles in FIFO order. Add equivalent `return()`/`throw()` cases and a GC/no-GC pair.

- [x] **Step 2: Run the tests to confirm the single-slot failure.**

  Run:

  ```sh
  cargo nextest run -p v12-interp --test async_generator_request_queue
  ```

  Expected: the second request is overwritten or never settles.

- [x] **Step 3: Implement the rooted FIFO.**

  Root each request immediately after allocation. Store request handles in a canonical array/queue slot, append on each request, and pop/settle from the front on yield, completion, or rejection. Preserve the existing async-generator yield-vs-await flag and the existing pending-settlement sink.

- [x] **Step 4: Run focused and language gates.**

  Run the new test, `cargo nextest run -p v12-interp`, and all three Task 8 language filters. Compare against the saved parent TAP sets and require zero lost passes.

- [x] **Step 5: Commit.**

  ```sh
  git add crates/v12-interp/src/generator_async.rs \
    crates/v12-interp/tests/async_generator_request_queue.rs
  git commit -m "async-generators: queue concurrent requests"
  ```

**Task 4 implementation record (2026-09-25):**
- Rooted FIFO request queue: `83e98340`.
- Focused tests: request queue `6/6`; async yield-star `7/7`; interpreter `126/126`; compiler `184/184`.
- Pass sets versus the Task 4 parent: async-generator expressions `438→443` (`+5`, zero losses); statements `210→211` (`+1`, zero losses); for-await-of `1191→1191`; AsyncGeneratorPrototype `15→20` (`+5`).
- Workspace gate: `787 passed`; clippy and fmt passed.
- Two `request-queue-order-state-executing` panics remain pre-existing; they reproduce with the parent `generator_async.rs` restored and are recorded, not attributed to this slice.

---

### Task 5: Close Task 8 language gates and hand off Promise work

**Files:**
- Modify: `docs/superpowers/plans/2026-09-24-test262-coverage-acceleration.md`
- Create: `target/test262-coverage/task8-language-final.txt` only as a local artifact

- [x] **Step 1: Run the final focused and workspace gates.**

  ```sh
  cargo nextest run --workspace
  cargo clippy --workspace --all-targets
  cargo fmt --check
  ./conformance/run.sh --filter 'language/expressions/async-generator' --jobs 4 --format human
  ./conformance/run.sh --filter 'language/statements/async-generator' --jobs 4 --format human
  ./conformance/run.sh --filter 'language/statements/for-await-of' --jobs 4 --format human
  ```

- [x] **Step 2: Record exact parent/current deltas.**

  Record the commit, Test262 pin, filter, pass/fail/skip counts, raw TAP artifacts, and any intentional false-pass exposure. Require zero lost passes, zero new panics, and zero timeouts; document pre-existing failures separately.

- [x] **Step 3: Commit the plan record.**

  ```sh
  git add docs/superpowers/plans/2026-09-24-test262-coverage-acceleration.md
  git commit -m "docs: record language async coverage gains"
  ```

- [x] **Step 4: Re-open Task 8 Promise work separately.**

  Only after Tasks 1–4 are accepted, schedule a new Promise plan for interpreter-seam `all`/`race`, thenable adoption, and Promise prototype statics. Do not mix those changes into the compiler slices above.

**Task 5 final record (2026-09-25):**
- Final human gates: async-generator expressions `443/623` (71.1%); statements `211/301` (70.1%); for-await-of `1191/1235` (96.4%); AsyncGeneratorPrototype `20/48` (41.7%); destructuring `18/19` plus one skip.
- Workspace gate: `787 passed`; clippy and fmt passed.
- No new panics or timeouts. Two AsyncGeneratorPrototype `request-queue-order-state-executing` panics are pre-existing and reproduce with the parent `generator_async.rs`; they remain a separate follow-up.
- Overall language residuals delivered: async yield-star, general-iterable bindings, binding close semantics, nested-array fast-path preservation, and async-generator request FIFO.
- Promise built-ins are intentionally handed off to `docs/superpowers/plans/2026-09-25-promise-interpreter-semantics.md`.
