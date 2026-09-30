# Test262 Coverage Acceleration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Keep each task independently reviewable and stop at its validation gate before starting the next writer task.

**Goal:** Raise the direct Test262 `test/language/*` pass rate from the current remeasured baseline to at least 78% while preserving every previously passing test, eliminating engine panics/timeouts, and closing the highest-yield semantic gaps in execution context, native callbacks, modules, arguments objects, and built-ins.

**Architecture:** Separate coverage work into semantic slices with a reproducible evidence path. Build the shared caller-environment and interpreter/native call capabilities first; then migrate the remaining built-ins and module machinery onto those capabilities. Do not treat historical failure-bucket counts as additive: every phase must establish a fresh before/after pass set on the current commit.

**Tech Stack:** Rust 1.90 workspace; `oxc_parser`/`oxc_semantic` 0.147.0; v12 compiler, interpreter, heap, engine, and native crates; `cargo nextest`; the repository Test262 runner using the `runner` profile with `panic = "unwind"`.

**Spec:** `CONTEXT.md` (ubiquitous language and v1 scope), `ROADMAP.md` (scope and non-goals), `conformance/fix-log.md` (historical measurements), and the Evidence Contract below.

## Global Constraints

- Use `cargo nextest run --workspace` as the workspace gate; do not substitute `cargo test`.
- Run large conformance slices with the default human format; write TAP/JSON to a file and use it only for small pass-set diffs.
- Report direct `language/*` results separately from the substring filter `language`, which also matches Annex B and Intl paths.
- Use the debug build for local Rust checks; never use a release binary for engine correctness claims.
- Do not run `git stash`, `git reset`, `git checkout`, or workspace-wide formatting commands.
- Commit each verified task immediately. Do not fan out writers that touch `crates/v12-interp/src/call_setup.rs`, `property.rs`, `lib.rs`, `crates/v12-bccompiler/src/collect.rs`, `stmt.rs`, or `crates/v12-engine/src/module_loader.rs` at the same time.
- Keep harness, display, and verdict changes separate from engine semantics. A changed failure count caused by a more honest verdict is not a conformance gain.
- Do not target Workers, `SharedArrayBuffer`, `Atomics`, TCO, resizable ArrayBuffer, identifier writes inside `with`, full cross-realm support, or broad Intl/Unicode work in this roadmap.
- Do not claim a percentage target until the direct `language/*` denominator and the current-HEAD pass set have been recorded.

---

## Current-State Diagnosis

The work was not blocked inside one unfinished feature. It was a sequence of verified conformance lanes, followed by a queue of semantic extensions:

- `HEAD` is `7665144` (`interp: callback-seam array results link %Array.prototype%`), with no tracked modifications. `ROADMAP.md` and `.orchestrator-notes.md` are untracked planning notes.
- The latest completed broad substring run is `17,318 / 7,231 / 41` at `4f2f788`, not at current `HEAD`; it is evidence of progress, not a current baseline.
- Current independently re-measured slices at `HEAD` are Array `1,243 / 1,935 / 154`, Object `2,326 / 1,081 / 7`, and expressions `7,755 / 3,357 / 16`; nextest is reported as `640 passed / 0 skipped` but is not a substitute for a current conformance run.
- The immediate unfinished queue is: canonical Array shape allocation for plain-object `Object.keys`/`Object.entries`, deferred-module namespace read triggering, live module bindings/re-exports, and the environment work behind direct `eval`, eval `arguments`, and Annex B block functions.
- The largest shared leverage is not a single builtin. It is the missing caller environment for direct eval and the missing general native-to-interpreter call capability. These foundations also unlock generator arguments, Map/Set callbacks, Promise thenables, and user-defined coercions.

### Confidence-adjusted yield map

| Priority | Slice | Historical evidence | Planning range | Dependency |
|---:|---|---|---:|---|
| 1 | Direct-eval caller environment | about 160 Annex B failures plus non-Annex-B scope failures | +160–300 | caller-context design |
| 2 | Eval `arguments` early errors | 136 tests in the recorded `language/eval-code` bucket | +100–140, overlapping #1 | caller-context design |
| 3 | Generator and async-generator `arguments` | 92–115 direct failures, plus mapped arguments | +90–150 | shared frame argument model |
| 4 | Native/interpreter call capability | Object getter, Date, JSON, Promise, and collection callbacks | +200–350 unique | call router and abrupt-completion contract |
| 5 | Map/Set/Iterator prototypes and iterable constructors | structural method surface and ignored constructor iterables | +120–220 | call capability |
| 6 | Module live bindings, re-exports, and defer reads | snapshots, skipped re-exports, and about 40 defer trigger tests | +80–150 | environment cells and module namespace seam |
| 7 | Function-scope Annex B B.3.3 block functions | about 117 function/global-code failures; two prior lanes failed | +100–120 | var/lexical environment model; global descriptors deferred |
| 8 | Promise thenables, species, and iterator close | 632 residual Promise tests, with overlapping async buckets | +80–180 | call capability and collection iterators |
| 9 | Array callback residuals | holes, abrupt completion, `thisArg`, species, iterator close | +80–180 | call capability; do not rewrite passing `sort` |
| 10 | Proxy and per-index descriptor residue | 37 Proxy failures and about 110 descriptor failures | +40–120 | object/heap representation |

The ranges are deliberately non-additive. The delivery gates below require a fresh TAP pass-set diff and zero lost passes.

## Evidence Contract

Before changing a semantic slice, record the current Test262 commit and a TAP pass set for the exact slice. Use these commands:

```sh
# Confirm the direct language denominator before using a percentage.
./conformance/run.sh \
  --test262-root conformance/test262 \
  --filter 'language/*' --list

# Human summary for a narrow semantic slice.
./conformance/run.sh \
  --filter 'language/eval-code' --jobs 4 --format human

# Machine-comparable pass set; never print a large TAP file to stdout.
./conformance/run.sh \
  --filter 'language/eval-code' --jobs 4 --format tap \
  --tap-out /tmp/eval-before.tap \
  >/tmp/eval-before.stdout 2>/tmp/eval-before.stderr

grep '  message:' /tmp/eval-before.tap | sort | uniq -c | sort -nr

comm -23 \
  <(grep '^ok ' /tmp/eval-before.tap | sort) \
  <(grep '^ok ' /tmp/eval-after.tap | sort)
```

A slice is accepted only when all of the following are true:

1. The intended behavior is covered by focused engine or compiler tests, not only by a score increase.
2. The TAP diff contains no previously passing test in the `before → after` lost set.
3. The slice has zero new `panicked at`, `STALLED`, or unexplained timeout lines.
4. `cargo nextest run --workspace`, `cargo clippy --workspace --all-targets`, and `cargo fmt --check` are green.
5. The `conformance/fix-log.md` entry records the exact commit, before/after counts, filters, and remaining bucket.

---

## Delivery Sequence

1. **Measurement and low-risk closure:** pin the corpus, fix Object result arrays, and complete the small error surface.
2. **Execution environment:** introduce one direct-eval caller context, then consume it for eval early errors and function-scope Annex B block functions; defer global/eval Annex B descriptors.
3. **Call capability and frames:** establish one interpreter call router; use it for generators, arguments, collections, and native coercions.
4. **Modules and async:** move namespaces to live cells, finish defer/reexports, then close Promise/async residuals.
5. **Exotic objects and descriptors:** repair Proxy, arguments, and per-index descriptors only after the shared seams are stable.
6. **Final evidence:** run direct full language, affected built-ins, and the workspace gate; update scoreboards only from current-HEAD artifacts.

Tasks touching the same files are sequential. Read-only reconnaissance and result analysis may run in parallel with no write scope.

### Task 0: Pin the Test262 denominator and make coverage artifacts reproducible

**Files:**
- Create: `scripts/test262-coverage-baseline.sh`
- Modify: `conformance/run.sh:18-46`
- Modify: `.github/workflows/ci.yml:37-47`

**Interfaces:**
- Produces a script accepting one artifact label and writing `summary.txt`, `list.txt`, and one TAP file per selected filter under `target/test262-coverage/<label>/`.
- Prints the exact Test262 git commit and the number of discovered `language/*` files.
- CI uses the repository gitlink, not an unpinned latest clone, and uploads the TAP artifact even when the runner exits nonzero.

- [ ] **Step 1: Add the baseline script**

```bash
#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN="$ROOT/conformance/run.sh"
LABEL="${1:-manual}"
OUT="$ROOT/target/test262-coverage/$LABEL"
mkdir -p "$OUT"

"$RUN" --test262-root "$ROOT/conformance/test262" --filter 'language/*' --list \
  >"$OUT/list.txt"
printf 'test262=%s\n' "$(git -C "$ROOT/conformance/test262" rev-parse HEAD)" \
  >"$OUT/summary.txt"
printf 'language_files=%s\n' "$(wc -l <"$OUT/list.txt" | tr -d ' ')" \
  >>"$OUT/summary.txt"

for spec in \
  'language/eval-code' \
  'annexB/language/eval-code' \
  'language/arguments-object' \
  'language/module-code' \
  'built-ins/Array' \
  'built-ins/Object' \
  'built-ins/Promise'; do
  safe="${spec//\//_}"
  set +e
  "$RUN" --filter "$spec" --jobs 4 --format human \
    >"$OUT/$safe.human" 2>"$OUT/$safe.stderr"
  human_status=$?
  "$RUN" --filter "$spec" --jobs 4 --format tap \
    --tap-out "$OUT/$safe.tap" \
    >"$OUT/$safe.stdout" 2>"$OUT/$safe.stderr.tap"
  tap_status=$?
  set -e
  printf '%s human=%s tap=%s\n' "$spec" "$human_status" "$tap_status" \
    >>"$OUT/summary.txt"
done
```

- [ ] **Step 2: Make `run.sh` report and pin the checkout**

At startup, read the gitlink object with `git -C "$REPO_ROOT" ls-tree HEAD conformance/test262`. If the checkout is absent and the object is present, initialize `conformance/test262`, add the upstream remote, fetch that exact object, and check it out detached before running the test. If the object cannot be fetched, fail with a message naming the expected object; do not silently replace it with the latest upstream commit. On every run, print `Using Test262 checkout ... at <sha>` after resolving the checkout.

- [ ] **Step 3: Make CI retain the evidence**

Add `submodules: recursive` to the checkout step. Run the conformance command with stdout and stderr redirected to `/tmp/t262.stdout` and `/tmp/t262.stderr`, capture its exit status in `/tmp/t262.status`, upload `/tmp/t262.tap`, `/tmp/t262.stdout`, `/tmp/t262.stderr`, and `/tmp/t262.status` with `actions/upload-artifact@v4` under `if: always()`, and finish with a step that exits with the captured status. This preserves the current red-on-any-failure semantics while retaining the diagnostic artifact.

- [ ] **Step 4: Verify the evidence affordance**

Run `bash -n scripts/test262-coverage-baseline.sh`, list `language/*`, and confirm the current checkout reports 23,724 files. Run one narrow slice and confirm its TAP file contains both `ok` and `not ok` records without flooding the terminal.

- [ ] **Step 5: Commit**

```sh
git add scripts/test262-coverage-baseline.sh conformance/run.sh .github/workflows/ci.yml
git commit -m "test262: pin corpus and retain coverage evidence"
```

### Task 1: Close the current Object result-array and Error surface queue

**Files:**
- Modify: `crates/v12-engine/src/builtins/object.rs:309-407,807-821`
- Modify: `crates/v12-engine/src/builtins/mod.rs:83-123,897-905`
- Modify: `crates/v12-engine/src/realm.rs:143-190`
- Create: `crates/v12-engine/tests/object_results.rs`

**Interfaces:**
- Use `Ctx::alloc_array(Vec<JsValue>) -> Handle<JsObject>` for every JavaScript-visible array created by Object statics.
- Use `Ctx::alloc_array(vec![key, value])` for each `Object.entries` pair.
- Install `Error.prototype.toString` through the existing native install family; no new global intrinsic slot is allowed.

- [ ] **Step 1: Write focused failing tests**

```rust
#[test]
fn object_results_are_canonical_arrays() {
    let mut engine = v12_engine::Engine::new();
    let value = engine
        .eval("Object.keys({a: 1}).constructor === Array && Object.entries({a: 1})[0].constructor === Array")
        .expect("Object result arrays evaluate");
    assert_eq!(engine.to_display_string(value), "true");
}

#[test]
fn error_prototype_to_string_is_installed() {
    let mut engine = v12_engine::Engine::new();
    let value = engine
        .eval("new TypeError('x').toString()")
        .expect("error method evaluates");
    assert_eq!(engine.to_display_string(value), "TypeError: x");
}
```

- [ ] **Step 2: Run the focused tests and confirm the current failures**

Run: `cargo nextest run -p v12-engine object_results`

Expected: at least the `Object.entries` canonical-array assertion or the Error prototype installation assertion fails before the implementation.

- [ ] **Step 3: Route all Object arrays through the canonical allocator**

Replace the direct `heap.alloc(JsObject::array(...))` calls in `object_keys`, the `Object.entries` pair loop, and `array_value` with `ctx.alloc_array`. Keep the existing enumerable-key order and value lookup unchanged; this is an allocation-shape fix, not a property-order rewrite.

- [ ] **Step 4: Install `Error.prototype.toString`**

Add the `ErrorProtoToString` length entry if it is absent, install the method on `error_proto` after the Error constructor/prototype link, and leave the existing implementation in `error.rs` unchanged. The test must cover empty name, empty message, both fields present, and a custom `name`/`message` override.

- [ ] **Step 5: Run focused and affected conformance slices**

```sh
cargo nextest run -p v12-engine object_results
./conformance/run.sh --filter 'built-ins/Object' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Error' --jobs 8 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Record exact before/after TAP pass sets; do not count a newly exposed honest failure as a regression.

- [ ] **Step 6: Commit**

```sh
git add crates/v12-engine/src/builtins/object.rs crates/v12-engine/src/builtins/mod.rs \
  crates/v12-engine/src/realm.rs crates/v12-engine/tests/object_results.rs
git commit -m "builtins: canonicalize Object result arrays and install Error toString"
```

### Task 2: Execute direct eval in the caller interpreter with compiler binding metadata

**Files:**
- Modify: `crates/v12-bytecode/src/lib.rs:158-193`
- Modify: `crates/v12-bytecode/src/builder.rs` (new metadata field initialization)
- Modify: `crates/v12-bccompiler/src/model.rs:198-254,285-310`
- Modify: `crates/v12-bccompiler/src/collect.rs:1812-1964`
- Modify: `crates/v12-bccompiler/src/expr.rs` (call lowering)
- Modify: `crates/v12-bccompiler/src/unit.rs:363-394`
- Modify: `crates/v12-bccompiler/src/lib.rs:158-192,421-428,503-521`
- Modify: `crates/v12-native/src/registry.rs:17-64`
- Modify: `crates/v12-engine/src/builtins/registry.rs:247-284`
- Create: `crates/v12-interp/src/eval_context.rs`
- Modify: `crates/v12-interp/src/lib.rs:59-67,403-444,463-585,895-910`
- Modify: `crates/v12-interp/src/execute.rs:583-608`
- Modify: `crates/v12-interp/src/call_setup.rs:79-88,133-153,1542-1560`
- Create: `crates/v12-engine/tests/eval_context.rs`
- Create: `crates/v12-bccompiler/tests/eval_metadata.rs`

**Interfaces:**
- Keep `NativeRegistry::eval` as the global/indirect evaluator. Add a compiler-only seam returning `CompiledEval`; direct eval registers that program in the current interpreter and pushes a child frame instead of constructing a second `Interp`.
- Add `EvalCallerContext` inside `v12-interp`, not to the native trait:

```rust
pub(crate) struct EvalCallerContext {
    pub this: JsValue,
    pub env: Option<Handle<JsObject>>,
    pub new_target: Option<JsValue>,
    pub arguments: Option<EvalBindingRef>,
    pub strict: bool,
    pub bindings: Vec<(String, EvalBindingRef)>,
}

pub(crate) enum EvalBindingRef {
    Register { base: usize, reg: u16 },
    Env { env: Handle<JsObject>, slot: u16 },
    Global(PropKey),
    Arguments { frame: usize },
}
```

- Add `eval_context: Option<EvalCallerContext>` and a stable `home_env` marker to `Frame`. Register references use absolute positions in the current `Interp.stack`; environment references use the existing traced `Environment` objects. Do not add `Rc<RefCell<JsValue>>` cells or copy the caller stack into a global object.
- Add `EvalFunctionMetadata { bindings, direct_eval_sites, eval_reachable }` to `FunctionBytecode`; `BindingLocation` is `Register(u16)`, `Env(u16)`, or `Global`.
- Add `NativeRegistry::compile_eval(..., EvalCompileFacts) -> Result<CompiledEval, Throw>` with `direct`, `strict`, `non_simple_parameters`, and caller binding descriptors. The existing `eval` method remains the global-only path.

- [x] **Step 1: Add failing direct-eval tests before changing execution**

Add compiler/bytecode tests for direct-call marking and environment promotion. Add engine tests for caller register writes, lexical reads, eval-created closures, `this`, `new.target`, implicit and explicit `arguments`, strict callers, direct-versus-alias behavior, nested eval, and indirect eval remaining global-scoped:

```js
function f(a) { eval("a = 3"); return a; }
f(1) === 3

function g() { let x = 1; return eval("x"); }
g() === 1

function h() { let x = 1; return eval("(function () { return x; })"); }
const closure = h();
closure() === 1

const receiver = { x: 1 };
function receiverThis() { return eval("this.x"); }
receiverThis.call(receiver) === 1

function C() { return eval("new.target"); }
new C() === C

function argsProbe(a) { return eval("arguments[0]"); }
argsProbe(7) === 7
```

Also assert `(0, eval)("typeof x") === "undefined"` when `x` is only a caller local, and that a strict caller throws `ReferenceError` for an undeclared assignment.

- [x] **Step 2: Mark direct eval sites and promote escaping bindings**

In the collector, set `may_direct_eval` only for a syntactic callee identifier named `eval`, propagate `eval_reachable` to enclosing units, and promote non-global bindings in eval-reachable units to `Environment` storage during `finalize`. Emit binding descriptors from `UnitPlan` in `unit.rs`. In the call lowering, record the call-header PC in `direct_eval_sites`; keep `Opcode::Call` unchanged so bytecode/JIT decoding does not acquire a new opcode.

- [x] **Step 3: Add the compile-only eval seam**

Implement `compile_eval` in the engine registry. It compiles with an explicit direct/indirect flag, effective caller strictness (`source directive OR caller strictness`), return leniency only for direct eval inside a function, and the caller binding descriptors. It returns `CompiledEval { main, functions, strings }`; it does not execute the program. Keep `engine/eval.rs` and `Engine::run_compiled` unchanged because they are embedder/script entry points.

- [x] **Step 4: Push a direct-eval frame in the same interpreter**

Add `Interp::capture_eval_context(&mut self)` and `Interp::run_compiled_child(CompiledEval, EvalCallerContext)`. The child frame must initialize `r0` from the captured lexical `this`, copy the captured `new.target`, and resolve `arguments` from the nearest non-arrow owner. Mark the call site in `execute.rs`, pass a `direct_eval` boolean to `prepare_call`, and take the direct path only when the marked callee resolves to the actual eval native. Use the existing `stop_at_frames`/`execute` boundary and preserve thrown values and completion values.

- [x] **Step 5: Keep indirect and realm eval global-scoped**

Leave `const e = eval; e(...)`, `(0, eval)(...)`, `globalThis.eval(...)`, `CallApply`, and `FunctionTarget::RealmEval` on the existing global-only path. Add a regression test proving indirect eval cannot see a caller local. Do not change `engine/eval.rs` in this task.

- [x] **Step 6: Run the focused and workspace gates**

```sh
cargo nextest run -p v12-bccompiler eval_metadata
cargo nextest run -p v12-engine eval_context
./conformance/run.sh --filter 'language/eval-code' --jobs 4 --format human
./conformance/run.sh --filter 'annexB/language/eval-code' --jobs 4 --format human
./conformance/run.sh --filter 'language/arguments-object' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Accept only a zero-lost TAP diff, zero new panics/timeouts, and a passing eval-created-closure probe. Tasks 3A/3B/3C (eval early errors, eval-local vars, and function-scope Annex B), Task 5 (generators), module bindings, and `engine/eval.rs` remain separate tasks; detached-arrow owner capture and global/eval Annex B descriptors are deferred.

- [x] **Step 7: Commit**

```sh
git add crates/v12-bytecode/src/lib.rs crates/v12-bytecode/src/builder.rs \
  crates/v12-bccompiler/src/model.rs crates/v12-bccompiler/src/collect.rs \
  crates/v12-bccompiler/src/expr.rs crates/v12-bccompiler/src/unit.rs \
  crates/v12-bccompiler/src/lib.rs crates/v12-native/src/registry.rs \
  crates/v12-native/src/lib.rs crates/v12-engine/src/builtins/registry.rs \
  crates/v12-interp/src/eval_context.rs crates/v12-interp/src/lib.rs \
  crates/v12-interp/src/execute.rs crates/v12-interp/src/call_setup.rs \
  crates/v12-engine/tests/eval_context.rs crates/v12-bccompiler/tests/eval_metadata.rs
git commit -m "interp: execute direct eval in the caller frame"
```

**Implementation record (2026-09-24):**
- Initial implementation: `14018ead1260c621474de7c94f9b3ca145666e4d`.
- Regression fix: `d96d842a1aa832ed26df746f3b02d2e3e3ae3200`.
- Parent/current TAP comparison: direct eval `245 → 257` passes, Annex B eval `229 → 229`, arguments `86 → 88`; lost-pass set empty.
- Focused regression tests: compiler eval metadata `4 passed`; engine eval context `5 passed`; workspace `652 passed, 0 skipped`.

### Task 3A: Add phase- and site-aware direct-eval declaration early errors

**Scope:** This slice closes only direct-eval declaration-instantiation early errors. It does not implement full `EvalDeclarationInstantiation`, eval-owned variable persistence, detached-arrow owner capture, global Annex B descriptors, or cross-script bindings.

**Files:**
- Modify: `crates/v12-bytecode/src/lib.rs` (site facts and function metadata)
- Modify: `crates/v12-bccompiler/src/collect.rs` (scope/phase-aware site collection and declaration validation)
- Modify: `crates/v12-bccompiler/src/model.rs` (site plans and declaration metadata)
- Modify: `crates/v12-bccompiler/src/expr.rs` (attach emitted call PCs to site plans)
- Modify: `crates/v12-bccompiler/src/unit.rs` (emit per-site facts and `VarDeclaredNames`)
- Modify: `crates/v12-bccompiler/src/lib.rs` (direct-only fact selection and compilation)
- Modify: `crates/v12-interp/src/eval_context.rs` (select facts by call PC)
- Modify: `crates/v12-interp/src/execute.rs` and `src/call_setup.rs` (pass the logical call PC)
- Modify: `crates/v12-bccompiler/tests/eval_early_errors.rs`
- Modify: `crates/v12-engine/tests/eval_context.rs`

**Interfaces:**
- Keep the shared DTO in `v12-bytecode` and preserve the `v12-native` re-export. Replace the function-wide `direct_eval_sites: Vec<u32>` with one authoritative table:

```rust
pub enum EvalPhase { Parameter, Body }

pub struct EvalSiteFacts {
    pub pc: u32,
    pub phase: EvalPhase,
    pub caller: EvalCallerFacts,
}

pub struct EvalFunctionMetadata {
    pub bindings: Vec<(String, BindingLocation)>,
    pub direct_eval_sites: Vec<EvalSiteFacts>,
    pub var_declared_names: Vec<String>,
    pub eval_reachable: bool,
}
```

- `EvalCallerFacts` keeps ordered `(name, EvalBindingKind)` entries and `arguments_object_needed` means “an implicit arguments binding exists at this call site,” not merely “the body mentions arguments.” `Parameter`, `Var`, and `Lexical` conflicts always reject eval `var arguments`; `ImplicitArguments` rejects only when that flag is true.
- Record each syntactic direct-eval site with its Oxc scope id, AST span, and `Parameter`/`Body` phase while walking parameters or the body. After collection, resolve visible symbols through the semantic scope tree, preserving nearest lexical scope and excluding nested function bodies.
- `var_declared_names` for the eval main unit includes `var`, `for-in`/`for-of` variables, and direct function declarations, but never declarations from nested function units.
- Gate caller-specific validation on `facts.direct`; indirect eval receives no caller facts. Keep the existing strict `var eval` check.

- [x] **Step 1: Add failing site/phase tests**

Add compiler tests for:
- an arrow parameter-site eval with a body `let/var/function arguments` (legal);
- an arrow body-site eval with those same body bindings (early error where visible);
- an eval outside a nested block containing `let arguments` (legal);
- non-simple ordinary parameters and a body `arguments` reference (implicit binding, `arguments_object_needed = true`);
- a simple ordinary function with no existing `arguments` binding (legal `var arguments`);
- explicit parameter/var/lexical caller bindings (early error);
- `direct: false` with misleading caller facts (legal);
- `for-in`/`for-of` and nested-function `VarDeclaredNames`.

Add engine tests for side-effect suppression and preserve the existing alias/indirect, strict/sloppy, `this`, `new.target`, and closure tests.

- [x] **Step 2: Implement semantic site collection**

In `collect.rs`, track scope id and `walking_params` while visiting program, function/arrow, block, loop, catch, and `with` nodes. Record the direct-eval AST span and phase, then use `Scoping::scope_ancestors`, `get_bindings`, and `find_binding` in a post-pass to build nearest-visible ordered facts. Do not infer visibility from the flat `lexical_names` set.

- [x] **Step 3: Emit PCs and validate declarations**

In `expr.rs`, attach the logical call-header PC to the matching site plan. In `unit.rs`, emit only sites with a PC and the eval main unit’s `var_declared_names`. In `collect_with_eval_facts`, reject `var arguments` only for direct eval when a visible `Parameter`/`Var`/`Lexical` binding exists, or an `ImplicitArguments` binding with `arguments_object_needed = true`. Return the existing `CompileError` form so the engine produces a realm-linked `SyntaxError` before child-frame execution.

- [x] **Step 4: Select facts at runtime**

Pass the call PC from `execute.rs` through `prepare_call` into `capture_eval_context`. Look up the matching `EvalSiteFacts`; use its phase, strictness, visible names, and `arguments_object_needed` rather than a function-wide facts set. Keep existing `EvalBindingRef` routing and direct/indirect detection unchanged.

- [x] **Step 5: Run focused and regression gates**

```sh
cargo nextest run -p v12-bccompiler --test eval_early_errors
cargo nextest run -p v12-bccompiler --test eval_metadata
cargo nextest run -p v12-engine --test eval_context
./conformance/run.sh --filter 'language/eval-code' --jobs 4 --format human
./conformance/run.sh --filter 'annexB/language/eval-code' --jobs 4 --format human
./conformance/run.sh --filter 'language/arguments-object' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Compare parent/current TAP pass sets for all three slices. Require zero lost passes, zero new panics/timeouts, and all legal arrow/block-scope probes.

- [x] **Step 6: Commit**

```sh
git add crates/v12-bytecode/src/lib.rs crates/v12-bccompiler/src/collect.rs \
  crates/v12-bccompiler/src/model.rs crates/v12-bccompiler/src/expr.rs \
  crates/v12-bccompiler/src/unit.rs crates/v12-bccompiler/src/lib.rs \
  crates/v12-interp/src/eval_context.rs crates/v12-interp/src/execute.rs \
  crates/v12-interp/src/call_setup.rs crates/v12-bccompiler/tests/eval_early_errors.rs \
  crates/v12-engine/tests/eval_context.rs
git commit -m "compiler: make direct eval errors site-aware"
```

**Implementation record (2026-09-24):**
- Initial site-aware implementation: `a6a04e668111f6f8f331cc4924b1b7969070f4e0`.
- Arrow-owner arguments correction: `9d783bc9b50823ba42c3435a7de3caff117af121`.
- Exact emitted call-PC correction: `f1004c87c44a28eb9ad3464f784174e8131b34f8`.
- Focused tests: compiler eval early errors `15 passed`; eval metadata `5 passed`; engine eval context `8 passed`; `cargo fmt --check` passed.
- Parent/current TAP comparisons: direct `294/595`; Annex B eval `229/469`; arguments-object `88/263`; lost/gained sets empty for all three slices.
- Workspace gate: `671 passed, 0 skipped`; clippy passed with the repository's accepted warning set.
- Remaining arrow body-var failures are the explicitly deferred Task 3B global-routing issue, not a Task 3A regression.

### Task 3B: Route eval-owned `var` declarations without global leakage

**Scope:** This slice fixes the legal arrow/default-parameter regression where an eval-owned `var` falls through to `globalThis`. It does not add detached-arrow owner capture or global Annex B descriptor semantics.

**Files:**
- Modify: `crates/v12-bytecode/src/lib.rs` (explicit eval-payload marker)
- Modify: `crates/v12-bccompiler/src/lib.rs`, `src/model.rs`, and `src/unit.rs` (payload mode and no eager global initialization)
- Modify: `crates/v12-interp/src/eval_context.rs` (eval-local binding route and caller-context publication)
- Modify: `crates/v12-interp/src/execute.rs` only if an existing global opcode hook needs mechanical adjustment
- Modify: `crates/v12-bccompiler/tests/eval_early_errors.rs`
- Modify: `crates/v12-engine/tests/eval_context.rs`

**Interfaces:**
- Keep `is_eval_payload` as provenance metadata only, and add `EvalVarTarget::{Global, CallerFunction, EvalLocal}` to the shared compile facts. Never infer the target from a nonzero program id, `fn_idx == 0`, or payload provenance alone.
- Add `EvalBindingRef::EvalLocal { env, slot }`. Existing caller `Register`/`Env`/`Arguments` bindings remain authoritative; eval-owned names allocate environment slots only for `CallerFunction`/`EvalLocal` targets, while `Global` falls through to ordinary `SetGlobal` behavior.
- Classify targets as `Global` for top-level direct eval, `CallerFunction` for ordinary-function direct eval, and `EvalLocal` for an arrow with no non-arrow owner. Nested direct eval inherits the parent target.
- Emit normal `global_var_init_ids` for eval payloads only when the target is `Global`; suppress eager global initialization for function/arrow-local targets and initialize local slots to `undefined` before executing the payload.

- [x] **Step 1: Add failing storage probes**

Add engine tests for:
```js
((a = 1) => eval("var arguments; 1"))() // 1
function f() { eval("var arguments; 1") } // 1; no SyntaxError
function f() { return eval("var n = 1; eval('n = 2; n')") } // 2
function f() { var x = 1; eval("var x = 2; x"); return x; } // 2
```
Assert `globalThis.arguments` and `globalThis.n` are unchanged by eval-owned declarations, while an ordinary top-level direct-eval `var` retains the existing global behavior. Keep indirect eval global-scoped.

- [x] **Step 2: Implement payload mode and slot routing**

Add the explicit payload marker, suppress eager global initialization for payload main units, and add `EvalLocal` handling to `eval_read_global`/`eval_write_global`. In `run_compiled_child`, allocate/reset eval-local slots for names in `var_declared_names` that are not already caller bindings, inherit nested maps, and publish the merged context under the caller environment handle. No new bytecode opcode is permitted.

- [x] **Step 3: Run focused and regression gates**

```sh
cargo nextest run -p v12-engine --test eval_context
cargo nextest run -p v12-bccompiler --test eval_early_errors
./conformance/run.sh --filter 'language/eval-code' --jobs 4 --format human
./conformance/run.sh --filter 'annexB/language/eval-code' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Require zero lost passes against the accepted Task 3A parent, no global-property leakage, and no new panics/timeouts.

- [x] **Step 4: Commit**

```sh
git add crates/v12-bytecode/src/lib.rs crates/v12-bccompiler/src/lib.rs \
  crates/v12-bccompiler/src/model.rs crates/v12-bccompiler/src/unit.rs \
  crates/v12-interp/src/eval_context.rs crates/v12-interp/src/execute.rs \
  crates/v12-bccompiler/tests/eval_early_errors.rs crates/v12-engine/tests/eval_context.rs
git commit -m "interp: route eval-owned vars locally"
```

**Implementation record (2026-09-24):**
- Initial eval-local routing: `cd09e73226cfb07203b8fedbf96974f1481a5c6f`.
- Environment/frame-context separation: `5a235c2badd0bea704a047e05b1becef01940893`.
- Frame-context cleanup: `e8335494a3db88d6acffda0113d25710a9d06ada`, `593d941a9425d245d628586f046bdb6ea5d00645`.
- Target-classification correction: `c2cf5022b8488c6f7a5cd84cec89b9e06c4d7baf`.
- Focused tests: compiler eval metadata/early errors `20 passed`; engine eval context `12 passed`; exact arrow/global probes pass; `cargo fmt --check` passed.
- Parent/current TAP comparisons against `f1004c8`: direct `294 → 319` with zero lost; Annex B eval `229 → 245` with zero lost; arguments-object `88 → 88` with zero lost.
- Workspace gate: `675 passed, 0 skipped`; clippy passed with the repository's accepted warning set.
- Global-init/descriptor gaps not fixed by this slice remain explicitly deferred; no lost parent passes are hidden by the gain count.

### Task 3C: Implement function-scope Annex B block-function initialization

**Scope:** This slice covers sloppy block-level function declarations in ordinary function bodies. Global/cross-script B.3.2 descriptors, `with`, generators/async functions, and eval/global binding persistence remain deferred; they are not acceptance targets of this task.

**Files:**
- Modify: `crates/v12-bccompiler/src/model.rs` (structured block-function plan metadata and dual locations)
- Modify: `crates/v12-bccompiler/src/collect.rs` (scope/symbol classification and declaration collection)
- Modify: `crates/v12-bccompiler/src/expr.rs` (reference-scope access routing)
- Modify: `crates/v12-bccompiler/src/stmt.rs` (block initialization and conditional write-back)
- Modify: `crates/v12-bccompiler/src/unit.rs` (emit metadata and binding writes)
- Create: `crates/v12-bccompiler/tests/annexb_block_functions.rs`

**Interfaces:**
- Preserve symbol identity and scope identity while collecting declarations; do not replace scope information with a flat set of names.
- Add an ordered `BlockFunctionPlan` per nested ordinary function unit with declaration span, declaration scope id, Oxc symbol id, child function index, a separate `Option<VarLoc>` lexical location, a separate `Option<VarLoc>` function-scope target, and the skip predicate (`ParameterCollision`, `LexicalCollision`, or `Eligible`). The two locations are required; a boolean lexical flag or reuse of one symbol slot is insufficient for block independence.
- Resolve the declaration scope from the parent of the function body scope, not the symbol's post-Oxc-hoisting home. Collect candidates before nested-function descent and resolve parameter/var/lexical collisions after the complete statement walk.
- Keep block lexical initialization and optional Annex B function-scope write-back as separate operations. A block binding is initialized in its block slot; the function-scope target is touched only when eligible. Append new slots after existing named slots/registers and leave `global_var_init_ids` unchanged.
- Add reference-scope-aware identifier access in `expr.rs` (reads, updates, assignments, logical assignments, destructuring targets, and `typeof` fast paths) while preserving `FnCtx::access(sym)` as the default/function-scope route for Task 3B compatibility.
- Apply Task 3C only to nested ordinary function units, never eval payload unit 0; do not alter `var_declared_names`, `var_symbols`, `lexical_symbols`, `EvalVarTarget`, or `run_compiled_child`. Do not add a new opcode.

- [x] **Step 1: Write the failing function-scope Annex B tests**

Cover these exact probes:

```js
(function (f) {
  var before = f;
  if (true) function f() {} else function _f() {}
  return [before, f];
})(123); // [123, 123]

(function () {
  let f = 123;
  { function f() {} }
  return f; // 123
})();
```

Also cover duplicate block functions, `for`, `for-in`, `for-of`, catch, default/rest parameters, an uninitialized caller binding, and a block function whose name has no var extension. Preserve the currently passing existing-var update case.

- [x] **Step 2: Collect scope-aware block-function metadata**

Extend `UnitPlan` with ordered `BlockFunctionPlan` entries. Record candidates during collection, resolve them after the complete statement walk, and allocate separate lexical/function-scope `VarLoc` values where a block binding exists. Use the declaration scope (the parent of the function body scope), retain declaration order, and distinguish function-scope names from block lexical names. Do not let nested function bodies or unrelated units contribute block-function plans to the current unit.

- [x] **Step 3: Emit initialization and write-back separately**

In `stmt.rs`/`unit.rs`, always emit the block lexical closure initialization into the plan’s lexical `VarLoc`. For an eligible Annex-B candidate, emit a second store of the same closure into the function-scope `var_loc` immediately afterward; parameter/lexical collision branches perform only the lexical store. Preserve declaration order and existing-var update/no-init behavior. In `expr.rs`, route identifier references and writes to the nearest matching block-function plan location, falling back to `FnCtx::access(sym)` for ordinary/function-scope bindings. Do not change global property descriptors or Task 3B eval routing.

- [x] **Step 4: Run focused and regression gates**

```sh
cargo nextest run -p v12-bccompiler --test annexb_block_functions
cargo nextest run -p v12-bccompiler --test eval_early_errors
./conformance/run.sh --filter 'annexB/language/function-code' --jobs 4 --format human
./conformance/run.sh --filter 'annexB/language/eval-code' --jobs 4 --format human
./conformance/run.sh --filter 'annexB/language/global-code' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Compare before/after TAP pass sets for each affected slice. Require zero lost passes, zero new panics/timeouts, and explicit reporting of any global/descriptor cases intentionally left deferred.

- [x] **Step 5: Commit**

```sh
git add crates/v12-bccompiler/src/model.rs crates/v12-bccompiler/src/collect.rs \
  crates/v12-bccompiler/src/expr.rs crates/v12-bccompiler/src/stmt.rs \
  crates/v12-bccompiler/src/unit.rs crates/v12-bccompiler/tests/annexb_block_functions.rs
git commit -m "compiler: implement function-scope Annex B block functions"
```

**Implementation record (2026-09-24):**
- Initial dual-location implementation: `3ea63c6c3a741e2144eb7111f18cdb18c98651ca`.
- Nested enclosing-block collision correction: `3473774a59598f416a673269fcc63470a87bfcd3`.
- Focused tests: Annex B block functions `10 passed`; compiler eval early errors/metadata `20 passed`; `cargo fmt --check` passed.
- Parent/current TAP comparisons against c2cf: function-code `92 → 110` with 18 gains and zero losses; global-code `75 → 75`; Annex B eval `245 → 245`; direct eval `319 → 319`; arguments-object unchanged.
- Workspace gate: `685 passed, 0 skipped`; clippy passed with the repository's accepted warning set.
- Global/cross-script descriptors, `with`, generators/async, detached arrows, and eval/global persistence remain deferred.

**Deferred from the original Task 3:** detached-arrow `arguments` lifetime capture, global Annex B binding descriptors, cross-script persistence, `with` behavior, and eval/global binding persistence remain separate follow-ups because they require broader frame/environment or global-environment changes.

### Task 4: Establish the single native-to-interpreter call capability

**Files:**
- Create: `crates/v12-interp/src/call_capability.rs`
- Modify: `crates/v12-interp/src/lib.rs` (module/API, `call_object` adapter, boundary docs)
- Modify: `crates/v12-interp/src/call_setup.rs` (value callback consumers and bound recursion audit)
- Modify: `crates/v12-interp/src/ops.rs` (user coercion calls)
- Modify: `crates/v12-interp/src/object_ops.rs` (iterator method calls)
- Create: `crates/v12-interp/tests/call_capability.rs`
- Do not modify `v12-engine/src/builtins/ctx.rs` or `registry.rs` in this slice.

**Interfaces:**
- Add `Interp::call_value(callee: JsValue, this: JsValue, args: &[JsValue]) -> Result<JsValue, JSException>` as the fully executing value-call capability. Make `call_object(handle, ...)` a compatibility adapter that delegates to it.
- Keep parked opcode preparation (`prepare_call`, `prepare_call_apply`), direct-eval origin handling, generator/async creation, construction (`prepare_construct`/`construct_object`), proxy construct traps, jobs, `RealmEval`, and `dispatch_native` control IDs in their existing internal interfaces. Do not duplicate their target branching into a fourth dispatcher.
- The nested-call implementation must save/restore `stop_at_frames`, respect `MAX_CALL_DEPTH`, clean frame call arguments/contexts on every unwind, and place callee/this/args on the value stack before the first safepoint. Bound forwarding must use the private nested implementation to avoid recursive public-adapter depth gaps.
- Migrate value consumers (`run_array_callback`, collection/iterator/sort callbacks, `Reflect.apply`, Function.prototype.call/apply, user coercions, and iterator next/return) through `call_value`; preserve exact `JSException` values. Keep `Ctx` as heap/realm/conversion state only.

- [x] **Step 1: Add call-routing tests**

Test a bytecode callback, `thisArg`, bound prefix, proxy callable, native/host target, throwing callback, nested re-entry, generator/async callee, depth/deadline cleanup, and successful reuse after an error. Every test must assert completion/thrown value and frame cleanup, not only absence of panic.

- [x] **Step 2: Implement the wrapper-first capability**

Create `call_capability.rs` with `call_value` and the private nested-call lifecycle. Make `call_object` delegate to it. Preserve `prepare_call` parked-opcode semantics and direct eval; audit bound branches to use the private nested implementation.

- [x] **Step 3: Migrate value callback consumers**

Change array/collection/iterator/sort callbacks, `Reflect.apply`, Function.prototype.call/apply, user coercions, and iterator next/return to call `call_value`. Root callbacks, receivers, and accumulators before safepoints; preserve exact abrupt values. Leave construct/proxy-construct, Promise job scheduling, `call_inline`/accessor consolidation, engine Reflect fallback, and `Ctx`/registry changes for follow-up slices.

- [x] **Step 4: Run focused and affected slices**

```sh
cargo nextest run -p v12-interp call_capability
./conformance/run.sh --filter 'built-ins/Reflect' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Object' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Array' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/JSON' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Promise' --jobs 8 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

- [x] **Step 5: Commit**

```sh
git add crates/v12-interp/src/call_capability.rs crates/v12-interp/src/call_setup.rs \
  crates/v12-interp/src/lib.rs crates/v12-interp/src/ops.rs \
  crates/v12-interp/src/object_ops.rs crates/v12-interp/tests/call_capability.rs
git commit -m "interp: centralize native callback call capability"
```

**Implementation record (2026-09-24):**
- Wrapper-first call capability: `47eac1a77793314b7d508c7228ba9393b133a14d`.
- Focused tests: `v12-interp` call capability `7 passed`; async-generator throw `1 passed`; generator tests `5 passed`; `cargo fmt --check` passed.
- Parent/current TAP comparisons against c2cf: Reflect `132/154`, Object `2334/3414`, Array `1397/3332`, JSON `81/165`, Promise `103/732`; all five slices had zero lost and zero gained tests.
- Workspace gate: `692 passed, 0 skipped`; clippy passed with the repository's accepted warning set.
- Construction, proxy-construct, jobs, accessor/`call_inline` consolidation, and engine registry fallback remain follow-up slices.

### Task 5: Materialize generator arguments and fix the basic arguments-object surface

**Scope:** Materialize the full actual-argument list for synchronous generators, async functions, and async generators; restore it into every resumed frame; and correct the basic arguments-object surface. Mapped exotic argument synchronization, queued concurrent async-generator requests, detached-arrow ownership, and the obsolete `caller` property remain deferred.

**Files:**
- Modify: `crates/v12-interp/src/lib.rs` (argument construction/rooting and frame lifecycle)
- Modify: `crates/v12-interp/src/call_setup.rs` (generator/async creation and actual-argument capture)
- Modify: `crates/v12-interp/src/generator_async.rs` (header slot 8 and resume restoration)
- Modify: `crates/v12-interp/src/generator.rs` (suspension cleanup)
- Modify: `crates/v12-interp/src/property.rs` (basic indexed/own-property surface only)
- Modify: `crates/v12-heap/src/object.rs` only if the existing arguments object storage needs a mechanical field/accessor adjustment
- Modify: `crates/v12-bytecode/src/lib.rs`/builder only if simple-parameter metadata is required for correct `callee` classification
- Create: `crates/v12-interp/tests/generator_arguments.rs`

**Interfaces:**
- Reserve generator header slot `8` for the retained arguments value. Keep slots 4–7 unchanged (async completion/return/request/yield metadata), use typed accessors, and never infer a Promise from `properties.len()`.
- At generator/async creation, copy the complete actual argument slice before Task 4 truncates its temporary stack, materialize through the existing argument factory when needed, root the value, and store it in slot 8. On resume, restore the value into `Frame.arguments`; on suspension, drop the frame root while retaining the header reference.
- `make_arguments_object` must link `%Object.prototype%`, use a configurable `length`, install the appropriate own `callee` behavior, and expose an own `Symbol.iterator` using the existing iterator surface. Do not add `caller`; do not enable `arguments_mapped` in this slice.
- Preserve direct-eval owner lookup and Task 4 `call_value` semantics. No new bytecode opcode or heap-layout redesign.

- [x] **Step 1: Write focused generator/arguments tests**

Cover full actual arguments (including extra actuals), sync generators, async functions, async generators, resume/suspend visibility, direct eval inside a generator, GC stress, `length` configurability, prototype linkage, `callee`, own `Symbol.iterator`, strict versus sloppy behavior, and reuse after generator completion. Do not assert mapped alias/delete/defineProperty synchronization in this slice.

- [x] **Step 2: Add typed header slots and materialization**

Add slot-8 accessors and ensure all generator/async creation paths snapshot actuals before the caller stack is truncated. Restore the stored object in every resumed frame and clear frame roots on suspension/ordinary completion. Keep async slots 4–7 and all Task 4 call boundaries unchanged.

- [x] **Step 3: Correct the basic arguments-object surface**

Link the object prototype, install configurable `length`, own `callee` with the correct strict/sloppy behavior, and own `Symbol.iterator` with the existing iterator helper. Keep `arguments_mapped` unset; record mapped exotic work as deferred.

- [x] **Step 4: Run focused and regression gates**

```sh
cargo nextest run -p v12-interp generator_arguments
cargo nextest run -p v12-interp call_capability async_generator_throw generator_next generator_suspend
./conformance/run.sh --filter 'language/arguments-object' --jobs 4 --format human
./conformance/run.sh --filter 'language/expressions/async-generator' --jobs 4 --format human
./conformance/run.sh --filter 'language/statements/async-generator' --jobs 4 --format human
./conformance/run.sh --filter 'built-ins/Promise' --jobs 8 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Compare affected TAP pass sets against the accepted Task 4 parent; require zero lost passes, zero new panics/timeouts, and explicit reporting of deferred mapped-argument cases.

- [x] **Step 5: Commit**

```sh
git add crates/v12-interp/src/lib.rs crates/v12-interp/src/call_setup.rs \
  crates/v12-interp/src/generator.rs crates/v12-interp/src/generator_async.rs \
  crates/v12-interp/src/property.rs crates/v12-heap/src/object.rs \
  crates/v12-bytecode/src/lib.rs crates/v12-bytecode/src/builder.rs \
  crates/v12-interp/tests/generator_arguments.rs
git commit -m "interp: materialize generator arguments"
```

**Implementation record (2026-09-24):**
- Generator/async materialization: `13a3f8343540331eaf190f9d0a01982598e87785`.
- JsObject initializer fix: `21dfea42c034886257ef2af74a594ab833931882`.
- Sloppy callee overwrite fix: `70a64135af33d2b36df22dac9eae416bb0d24a09`.
- Focused generator tests: `generator_arguments` `5 passed`; call capability `7 passed`; async-generator throw `1 passed`; generator tests `7 passed`; `cargo fmt --check` passed.
- Parent/current TAP comparisons against Task 4 parent: arguments-object `88 → 195` with zero losses; async-generator expressions `337 → 337`; async-generator statements `160 → 160`; Promise `103 → 103`; direct eval `319 → 319`.
- Workspace gate: `697 passed, 0 skipped`; clippy passed with the repository's accepted warning set.
- Mapped exotic synchronization, queued async-generator requests, detached-arrow ownership, and the strict callee descriptor integration remain deferred.

### Task 6: Linked module binding cells, then deferred evaluation

**Scope:** Replace module value snapshots with GC-visible binding cells for evaluated modules, then add an interpreter-owned deferred-evaluation trigger. Do not add a heap-only native trigger: deferred evaluation requires `&mut Interp`. Defer `@@toStringTag` until the realm has one canonical well-known symbol, and defer JSON/bytes import attributes and ambiguous `export *` handling.

**Files:**
- Modify: `crates/v12-heap/src/object.rs:24-83,108-200,837-911` (namespace/cell kinds, GC tracing, explicit test initializers)
- Modify: `crates/v12-bccompiler/src/model.rs:133-173,285-310,379-386` (module binding plan and indirect exports)
- Modify: `crates/v12-bccompiler/src/collect.rs:1129-1395` (import/export source forms and binding classification)
- Modify: `crates/v12-bccompiler/src/unit.rs:512-672` (cell-backed module lowering; remove value snapshots)
- Modify: `crates/v12-interp/src/lib.rs:1493-1517,2231-2285` (cell-aware env reads/writes and roots)
- Modify: `crates/v12-interp/src/eval_context.rs:347-413` (cell-aware direct-eval access)
- Modify: `crates/v12-interp/src/property.rs:120-253,945-1279,1588-1611,2918-2998` (namespace read/write/delete/descriptor dispatch)
- Modify: `crates/v12-engine/src/module_loader.rs:61-389` (linked records, state, cell installation, phase-aware requests)
- Modify: `crates/v12-engine/src/module_namespace.rs:1-155` (namespace identity and cell-backed exports)
- Modify: `crates/v12-engine/src/builtins/object.rs:1197-1307`, `reflect.rs`, and `internal_methods.rs` (no cell leakage in descriptors)
- Create: `crates/v12-engine/tests/module_live_bindings.rs`

**Interfaces:**
- `LinkedModule` owns a stable id, namespace/entry/seed-environment handles, request list, evaluation state, and cached error. `BindingCell` stores the current value plus initialized/TDZ state and owner module id.
- `ModuleBindingPlan` forces module-local exported/imported bindings to environment slots seeded with cells. `env_read`/`env_write` and direct-eval access dereference/update those cells; ordinary script environments remain unchanged.
- Allocate one cell per local exported binding. Named aliases and explicit re-exports point to the source cell, not copied values. Namespace properties hold cell references but all external descriptor/value readers dereference them.
- `Interp` owns an optional `ModuleEvaluator` capability. The engine adapter sets the loader referrer and executes a deferred module through the existing `Interp::call_value` lifecycle. Namespace reads skip evaluation for symbols/`"then"`; other deferred string reads call `ensure_evaluated` exactly once.
- Module namespace `[[Set]]`, `[[Delete]]`, and `[[DefineOwnProperty]]` remain spec-shaped; descriptor values never expose `BindingCell`. Do not add `NativeId::ModuleNamespaceGet`.

#### Task 6A: Live cells for evaluated modules

- [x] **Step 6A.1: Write focused live-binding tests**

Cover a setter update visible through a namespace, self-import/cycle identity, a re-export chain and explicit alias, initialized/uninitialized cell reads, descriptor value dereferencing, and GC stress. Assert side-effect order and namespace identity, not only final values.

- [x] **Step 6A.2: Produce and execute a cell-backed module binding plan**

Add compiler metadata for local/imported/exported bindings and indirect export source names. Force module-main bindings to seeded environment slots; make env reads/writes and direct-eval access cell-aware while preserving ordinary script behavior.

- [x] **Task 6A.3: Link one cell per exported binding**

Create linked module records and install source-cell aliases in namespaces during linking/evaluation. Remove completion-time value copying for the covered live paths. Track initialization/TDZ and owner module identity; retain self-import/cycle namespace identity.

- [x] **Task 6A.4: Close namespace property and descriptor paths**

Use `Kind::ModuleNamespace` before ordinary shape lookup. Update Object/Reflect/own-descriptor helpers so they report the cell’s current value with `{writable:true, enumerable:true, configurable:false}`, reject writes/deletes/definitions, and do not expose cell handles. Defer `@@toStringTag` until canonical symbol support exists.

**6A implementation record (2026-09-25):**
- Cell-backed module bindings and linked records: `5adfafc741e7f3abd68f8121bd59c8a953f39cc2`.
- Parent regression fixes: `9ccee4ecf59af4fc5320ab2120df08a145d60ad7`.
- `export *`/ambiguous-error compatibility fixes: `9d8035908b4588871e4d9764b8689986357e73ef`.
- Formatting-only cleanup: `e903d2e47509d2bc1bcc73e2d1875ef34b7ee115`.
- Parent/current TAP comparison against 6A parent: import `57→57` with zero lost; module-code `447→478` with zero lost; top-level-await `201→202` with zero lost; dynamic-import `753→804` with zero lost.
- Focused tests: module live bindings `9 passed`; module namespace `6 passed`; eval context `12 passed`; interpreter property `12 passed`; compiler module tests passed.
- Workspace gate: `706 passed, 0 skipped`; clippy passed with the accepted warning set; `cargo fmt --check` and `git diff --check` passed.
- Deferred from 6A: interpreter-owned deferred evaluation, full `export *` materialization, JSON/bytes import attributes, and canonical `@@toStringTag`.

#### Task 6B: Interpreter-owned deferred evaluation

- [x] **Step 6B.1: Establish one shared module record with two namespace identities**

Replace the duplicated eager/deferred execution state with one canonical record per normalized path containing the environment, cells, request metadata, link/evaluation status, and cached error. Keep distinct eager and deferred namespace objects referencing the same record/cells. Deduplicate graph requests by `(specifier, deferred)`. Preserve existing 6A live-cell, cycle, and Task 4/5 behavior.

**6B.1 implementation record (2026-09-25):**
- Shared linked records and distinct namespace variants: `105c495d`.
- Parent/current TAP comparison: import `57→58` with zero lost; module-code `478→478`; top-level-await `202→202`; dynamic-import `804→804`.
- Focused module-deferred/live-binding/compiler tests and `cargo check --workspace --all-targets` passed.
- Workspace gate at this stage: `710 passed, 0 skipped`; clippy and fmt passed.
- Deferred read triggers, cached runtime errors, and evaluator installation remain for 6B.2–6B.4.

**6B.2–6B.4 implementation record (2026-09-25):**
- Interpreter-owned deferred evaluator and observing-method triggers: `cff98304`.
- Formatting-only cleanup: `cb56fcb0`.
- Parent/current TAP comparison: import `57→81` with zero lost; module-code `478→478`; top-level-await `202→202`; dynamic-import `804→804`.
- Focused engine module tests: `21 passed`; compiler library tests: `148 passed`; workspace check passed.
- Final workspace gate: `718 passed, 0 skipped`; clippy passed with the accepted warning set; `cargo fmt --check` and `git diff --check` passed.
- Explicit deferrals: import attributes/JSON/bytes semantics, canonical `@@toStringTag`, full `export *` materialization, fresh-heap indirect-eval module loading, and broad SCC redesign.

- [x] **Step 6B.2: Complete linking before deferred exposure**

Factor linking so deferred records receive export-star validation, star aliases, and binding-cell links before any trigger. Cache evaluation errors on the shared record and root the cached `JsValue`; repeated reads/imports must observe the same error without rerunning the module. Set/restore the referrer around execution and retain the existing async-transitive-dependency path.

- [x] **Step 6B.3: Add the interpreter-owned evaluator capability**

Define `ModuleEvaluator` in `v12-interp`, store `Option<Rc<dyn ModuleEvaluator>>` on `Interp`, and install the engine adapter at every same-heap execution entry. Clone the `Rc` before passing `&mut Interp`; never retain a loader `Ref`/`RefMut` across module execution. Do not add a heap-only native ID.

- [x] **Step 6B.4: Trigger from all observing internal methods**

Before reading or enumerating a deferred namespace, evaluate it for string `Get`, `HasProperty`, `GetOwnProperty`, and `OwnPropertyKeys`; symbols and `"then"` skip keyed triggers, while OwnPropertyKeys always triggers. Leave Set/Delete/DefineOwnProperty/PreventExtensions/SetPrototypeOf non-triggering. Cover Object/Reflect, `hasOwn`, prototype/`super`, and descriptor paths through the existing interpreter seam.

- [x] **Step 6B.5: Add focused deferred-read tests**

Cover no side effect before read, exactly-once evaluation, symbol/`"then"` skips, absent-export triggers, key enumeration, cached errors, self-trigger TypeError, re-exported deferred namespaces, eager+deferred identity, referrer-correct nested imports, and existing async-transitive-defer behavior.

- [x] **Step 6B.6: Run module gates**

```sh
cargo nextest run -p v12-engine module_live_bindings
cargo nextest run -p v12-engine module_deferred
./conformance/run.sh --filter 'language/import' --jobs 4 --format human
./conformance/run.sh --filter 'language/module-code' --jobs 4 --format human
./conformance/run.sh --filter 'language/module-code/top-level-await' --jobs 1 --format human
./conformance/run.sh --filter 'language/expressions/dynamic-import' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Compare module TAP pass sets against the accepted 6A parent; require zero lost passes, zero new panics/timeouts, and retain the explicit 6A/6B deferrals.

- [x] **Step 6B.7: Commit**

```sh
git add crates/v12-heap/src/object.rs crates/v12-bccompiler/src/model.rs \
  crates/v12-bccompiler/src/collect.rs crates/v12-bccompiler/src/unit.rs \
  crates/v12-interp/src/lib.rs crates/v12-interp/src/eval_context.rs \
  crates/v12-interp/src/property.rs crates/v12-engine/src/module_loader.rs \
  crates/v12-engine/src/module_namespace.rs crates/v12-engine/src/builtins/object.rs \
  crates/v12-engine/src/builtins/reflect.rs crates/v12-engine/src/builtins/internal_methods.rs \
  crates/v12-engine/tests/module_live_bindings.rs crates/v12-engine/tests/module_deferred.rs
git commit -m "modules: link live bindings and defer evaluation through interp"
```

### Task 7: Map/Set prototypes, live collection semantics, and iterable construction

**Scope:** Materialize real `%Map.prototype%` and `%Set.prototype%`, make collection storage/iteration live and observable, and implement iterable constructors through the interpreter seam. Defer the global `Iterator` constructor and `%IteratorPrototype%`/`%MapIteratorPrototype%`/`%SetIteratorPrototype%` family; those require a separate intrinsic carrier.

**Files:**
- Modify: `crates/v12-engine/src/realm.rs:36-74,198-220,265-348`
- Modify: `crates/v12-engine/src/builtins/mod.rs:34-71,481-497,745-1113`
- Modify: `crates/v12-engine/src/builtins/map.rs:25-229`
- Modify: `crates/v12-engine/src/builtins/iterator.rs:28-207`
- Modify: `crates/v12-engine/src/display.rs` (tombstone and Set-entry display)
- Modify: `crates/v12-interp/src/property.rs:119-260,817-916` (ordinary-property precedence and structural fallback)
- Modify: `crates/v12-interp/src/call_setup.rs:1651-1724,2733-2779` (constructor and live forEach seams)
- Modify: `crates/v12-interp/src/object_ops.rs:332-559` only if iterator helper reuse requires it
- Create: `crates/v12-engine/tests/collections_surface.rs`

**Interfaces:**
- Add `map`, `map_proto`, `set`, and `set_proto` to `BuiltinTargets`; allocate/root prototypes and link constructors with `install_ctor`.
- Allocate canonical `Symbol.iterator` and `Symbol.toStringTag` once per Realm and reuse their handles across prototype installation and interpreter lookup. Preserve lazy standalone-interpreter fallback.
- Install ordinary Map/Set methods, `size` accessors, and iterator aliases through realm/symbol-aware paths; structural `map_set_surface` remains only for standalone/non-realm fallback. Use `heap.is_realm_global(global)` as the gate, never `global == None`.
- `new Map(iterable)` / `new Set(iterable)` run through an interpreter callback seam: allocate and root the collection, dynamically resolve the ordinary `set`/`add` method, obtain/advance/close the iterator, validate entry objects and indices, and invoke the adder with the new collection as `this` through `Interp::call_value`.
- Use tombstones for deletion, live entry counting, live `forEach`/iterator traversal, and `ITER_KIND_SET_ENTRIES` yielding fresh `[value, value]` arrays. Keep Map/Set iterator prototypes deferred.

- [x] **Step 7.1: Implement internal collection semantics**

Add tombstones, live `size`/`forEach`/iteration, Set entry pairs, NaN `SameValueZero`, and display handling. Add focused tests before changing realm surfaces.

**7.1 implementation record (2026-09-25):**
- Live collection storage/iteration/forEach: `dafa2abc`.
- Parent/current TAP comparison: Map `64→69` with zero lost; Set `147→149` with zero lost; Iterator `14→14`; for-of `523→523`.
- Focused collection tests passed (`collections_surface` 6; collection engine tests 2; relevant interpreter tests 9).
- Workspace gate: `724 passed, 0 skipped`; clippy and fmt passed.
- Deferred: Map/Set prototype materialization, canonical symbols, iterable constructors, and global Iterator family.

- [x] **Step 7.2: Materialize Map/Set prototypes and canonical symbols**

Allocate/root Map/Set prototypes, extend `BuiltinTargets`, install constructors/methods/accessors/iterator aliases, and make ordinary own/prototype properties win over structural synthesis. Verify prototype/constructor identity and descriptors.

**7.2 implementation record (2026-09-25):**
- Prototype materialization: `ff68aa54`; iterator receiver validation: `f59709a9`.
- Fresh Test262 pin: `d86b2294eb0a17eaa281ff12c73c473ec864c72f`.
- TAP comparison against the accepted 7.1 parent: Map `69→128` with 61 gains and two raw losses; Set `149→196` with 47 gains; Iterator `14→14`; for-of `523→523`.
- The two Map losses are `clear/context-is-weakmap-object-throws.js` and `delete/context-is-weakmap-object-throws.js`. Their prior `ok` results were vacuous: the old structural `Map.prototype` access threw before evaluating `new WeakMap()`. The current ReferenceError is correct because this checkout has no `WeakMap` binding; no argument-order special case was added.
- Focused `collections_surface` 13/13; engine and native suites passed; workspace gate `731 passed`; clippy and fmt passed.
- Accepted as a semantic improvement with the two explicitly documented false-pass flips; WeakMap implementation remains a separate roadmap item.

- [x] **Step 7.3: Implement iterable constructors and close behavior**

Route `MapConstruct`/`SetConstruct` through the interpreter seam with dynamic adder lookup, iterator validation, `IteratorClose`, error precedence, callable proxies, and generator-return safety. Preserve existing `forEach` callback routing and Task 4 call boundaries.

**7.3 implementation record (2026-09-25):**
- Iterable construction: `87aa447f`; empty-iterable/adder ordering fix: `e31db728`.
- Fresh comparison against the accepted 7.2 parent: Map `128→146` with 18 gains and zero losses; Set `196→218` with 22 gains and zero losses; Iterator `14→14`; for-of `523→523`.
- Focused gates: `collections_surface` 18/18; interpreter 91/91; workspace `736 passed`; clippy and fmt passed.
- Human collection gates: Map `146/215` (67.9%); Set `218/394` (55.3%); Iterator `14/654` (2.1%); for-of `518 passed, 5 skipped, 229 failed` (69.3% executable pass rate); no panic/timeout markers.
- The two documented 7.2 WeakMap false-pass flips remain the only intentional raw exception; global Iterator and WeakMap remain deferred.

- [x] **Step 7.4: Run collection gates**

```sh
cargo nextest run -p v12-engine collections_surface
./conformance/run.sh --filter 'built-ins/Map' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Set' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Iterator' --jobs 8 --format human
./conformance/run.sh --filter 'language/statements/for-of' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

Compare Map/Set/Iterator/for-of pass sets against the accepted parent; require zero lost passes, zero new panics/timeouts, and explicit reporting of the deferred global Iterator family.

- [x] **Step 7.5: Commit**

```sh
git add crates/v12-engine/src/realm.rs crates/v12-engine/src/builtins/mod.rs \
  crates/v12-engine/src/builtins/map.rs crates/v12-engine/src/builtins/iterator.rs \
  crates/v12-engine/src/display.rs crates/v12-interp/src/property.rs \
  crates/v12-interp/src/call_setup.rs crates/v12-interp/src/object_ops.rs \
  crates/v12-engine/tests/collections_surface.rs
git commit -m "builtins: install collection prototypes and iterable constructors"
```

### Task 8 rescope (2026-09-25)

The original Task 8 Promise slice is retained below for the later built-in lane, but it is not the next execution slice. Baseline review showed the largest language-coverage blockers are shared compiler/runtime defects: async `yield*`, general-iterable binding patterns, binding-pattern `IteratorClose`, and async-generator request FIFO. Execute those first using `docs/superpowers/plans/2026-09-25-language-async-residuals.md`; then return here for Promise registry work.

### Task 8: Close the remaining Promise and async semantics after the call seam

**Files:**
- Modify: `crates/v12-engine/src/builtins/promise.rs:84-196,603-739`
- Modify: `crates/v12-engine/src/job_queue.rs:1-260`
- Modify: `crates/v12-interp/src/call_setup.rs:1575-1668`
- Modify: `crates/v12-interp/src/generator_async.rs:126-220,526-695`
- Create: `crates/v12-engine/tests/promise_semantics.rs`

**Interfaces:**
- Preserve the landed `Promise.all`, `Promise.race`, and `Promise.prototype.finally` implementations.
- Add arbitrary thenable adoption and general iterable input through `Interp::call_value`.
- Add species/subclass selection and iterator-close paths without changing the single-realm v1 rule.

- [ ] **Step 1: Write reaction-order and thenable tests**

Cover a custom thenable, a thenable that throws on `then`, nested adoption, general iterables, iterator close, `Symbol.species` subclassing, `finally` callback throw, and a promise that never settles. Assert reaction order and exact rejection values.

- [ ] **Step 2: Implement thenable adoption**

Resolve a thenable by reading `then` with the ordinary property path, checking callability, and enqueueing a reaction job through the existing pending sink. Do not treat an arbitrary object as already fulfilled and do not spin synchronously on a pending promise.

- [ ] **Step 3: Generalize `all`/`race` inputs**

Consume any iterable with `GetIterator`/`IteratorStep`, preserve input order, and close the iterator on abrupt completion. Allocate result arrays through the canonical array allocator. Read `constructor` and `Symbol.species` through normal property access so a user override is observed.

- [ ] **Step 4: Close async-generator and await edges**

Await a yielded promise in async-generator `yield`, preserve the original request promise on an internal await, and run async `IteratorClose` through the checkpoint. Keep the already-landed request-promise and double-close protections intact.

- [ ] **Step 5: Run Promise and async gates**

```sh
cargo nextest run -p v12-engine promise_semantics
./conformance/run.sh --filter 'built-ins/Promise' --jobs 8 --format human
./conformance/run.sh --filter 'language/statements/for-await-of' --jobs 4 --format human
./conformance/run.sh --filter 'language/expressions/async-generator' --jobs 4 --format human
./conformance/run.sh --filter 'language/statements/async-generator' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

- [ ] **Step 6: Commit**

```sh
git add crates/v12-engine/src/builtins/promise.rs crates/v12-engine/src/job_queue.rs \
  crates/v12-interp/src/call_setup.rs crates/v12-interp/src/generator_async.rs \
  crates/v12-engine/tests/promise_semantics.rs
git commit -m "builtins: complete Promise thenable and async semantics"
```

### Task 9: Repair Array callback completion, descriptors, and species residuals

**Files:**
- Modify: `crates/v12-interp/src/call_setup.rs:2493-2665`
- Modify: `crates/v12-interp/src/property.rs:175-205,1090-1140,1260-1290`
- Modify: `crates/v12-engine/src/builtins/array.rs:540-840`
- Create: `crates/v12-interp/tests/array_callbacks.rs`

**Interfaces:**
- Use `Interp::call_value` for every callback.
- Use `Ctx::alloc_array`/`call::alloc_array` for every JavaScript-visible result.
- Preserve the already-passing `Array.prototype.sort` implementation and its stability tests.

- [ ] **Step 1: Write callback and abrupt-completion tests**

Cover holes, inherited values, length mutation, `thisArg`, reverse traversal, `findLast`, `flatMap` flattening one level, callback throw, reducer without an initial value, species constructors, and iterator close for array-like consumers.

- [ ] **Step 2: Snapshot length and validate the callback method**

Read and validate the callback before iteration, snapshot the receiver length once, and skip holes through the ordinary property lookup rather than assuming only the dense element store is authoritative. Do not let a nested callback change the iteration bound mid-call.

- [ ] **Step 3: Preserve completion and result allocation**

Propagate callback exceptions immediately, close iterators where the operation owns one, and route all result arrays through the canonical allocator. Use species only where the built-in contract requires a result constructor; do not introduce it into methods that return booleans, numbers, or `undefined`.

- [ ] **Step 4: Run Array gates**

```sh
cargo nextest run -p v12-interp array_callbacks
./conformance/run.sh --filter 'built-ins/Array' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Array/prototype/map' --jobs 4 --format human
./conformance/run.sh --filter 'built-ins/Array/prototype/flatMap' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

- [ ] **Step 5: Commit**

```sh
git add crates/v12-interp/src/call_setup.rs crates/v12-interp/src/property.rs \
  crates/v12-engine/src/builtins/array.rs crates/v12-interp/tests/array_callbacks.rs
git commit -m "arrays: complete callback completion and result semantics"
```

### Task 10: Close descriptor and Proxy residuals without broad cross-realm expansion

**Files:**
- Modify: `crates/v12-heap/src/object.rs:24-83,108-196`
- Modify: `crates/v12-interp/src/property.rs:945-1145,1560-1620`
- Modify: `crates/v12-interp/src/call_setup.rs:32-110,1670-2050`
- Modify: `crates/v12-engine/src/builtins/proxy.rs:1-260`
- Modify: `crates/v12-engine/src/realm.rs:202-240,259-342`
- Create: `crates/v12-interp/tests/proxy_descriptor_residuals.rs`

**Interfaces:**
- Preserve the landed `has/get/set/defineProperty/getOwnPropertyDescriptor/delete/apply/construct` trap routing.
- Add `JsObject::indexed_descriptors: Option<Box<rustc_hash::FxHashMap<u32, IndexedDescriptor>>>` with `IndexedDescriptor { value: Option<JsValue>, getter: Option<Handle<JsObject>>, setter: Option<Handle<JsObject>>, attrs: Attrs }`; use it only for indexed properties that are not represented by the fast element lattice.
- Link Proxy/revocable/bound-function objects to the correct Function prototype and preserve the single-realm boundary.

- [ ] **Step 1: Write the residual matrix**

Create tests for primitive-backed proxy targets, `%Proxy%`/`Proxy.revocable` prototype and constructor attributes, bound-function construction, nested proxy invariants, revocable targets, per-index descriptors, and `getOwnPropertyDescriptor` results on arrays/arguments.

- [ ] **Step 2: Link callable exotic prototypes**

Correct the realm links for Proxy, revocable functions, and bound functions without adding a `Proxy.prototype` property. Ensure `new Proxy(...)` validates an object target, revoked proxies throw on every internal method, and a bound target’s constructability follows its target.

- [ ] **Step 3: Add descriptor storage for indexed properties**

Keep the existing fast element lattice for ordinary reads/writes. When `getOwnPropertyDescriptor`, `defineProperty`, or `delete` observes an indexed property, consult `indexed_descriptors`; keep the descriptor’s value/accessor/attributes synchronized with the element lattice and preserve array `length` invariants and proxy trap forwarding.

- [ ] **Step 4: Run focused and regression gates**

```sh
cargo nextest run -p v12-interp proxy_descriptor_residuals
./conformance/run.sh --filter 'built-ins/Proxy' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Reflect' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Object/prototype' --jobs 4 --format human
./conformance/run.sh --filter 'language/arguments-object' --jobs 4 --format human
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

- [ ] **Step 5: Commit**

```sh
git add crates/v12-heap/src/object.rs crates/v12-interp/src/property.rs \
  crates/v12-interp/src/call_setup.rs crates/v12-engine/src/builtins/proxy.rs \
  crates/v12-engine/src/realm.rs crates/v12-interp/tests/proxy_descriptor_residuals.rs
git commit -m "objects: close proxy and indexed descriptor residuals"
```

### Task 11: Reconcile the final baseline, scoreboard, and roadmap

**Files:**
- Modify: `conformance/fix-log.md`
- Modify: `ROADMAP.md`
- Modify: `README.md`
- Modify: `CONTEXT.md`
- Create: `target/test262-coverage/final-summary.txt` only as a local artifact; do not commit large TAP files

**Interfaces:**
- Every documented score names the commit, Test262 commit, exact filter, pass/fail/skip counts, and whether the filter is direct or substring matching.
- The roadmap marks a bucket closed only when the current-HEAD pass set and focused behavioral tests agree.

- [ ] **Step 1: Run the final direct language sweep**

```sh
set +e
./conformance/run.sh \
  --filter 'language/*' --jobs 1 --format human \
  >/tmp/test262-language-final.human \
  2>/tmp/test262-language-final.stderr
human_status=$?
./conformance/run.sh \
  --filter 'language/*' --jobs 1 --format tap \
  --tap-out /tmp/test262-language-final.tap \
  >/tmp/test262-language-final.stdout \
  2>/tmp/test262-language-final.tap.stderr
tap_status=$?
set -e
printf 'human=%s tap=%s\n' "$human_status" "$tap_status"
```

Use the human output for the final score and the TAP file for the pass-set comparison; a nonzero runner status is expected while any executable Test262 test still fails, but the logs must contain zero new panics or unexplained timeouts.

- [ ] **Step 2: Run the affected built-in slices**

```sh
./conformance/run.sh --filter 'built-ins/Array' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Object' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Map' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Set' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Promise' --jobs 8 --format human
./conformance/run.sh --filter 'built-ins/Proxy' --jobs 8 --format human
```

- [ ] **Step 3: Run the complete workspace gate**

```sh
cargo nextest run --workspace
cargo clippy --workspace --all-targets
cargo fmt --check
```

- [ ] **Step 4: Verify no regressions and no hidden failures**

Check that `grep -c 'panicked at'` and `grep -c 'STALLED'` are zero in the final logs, compare final and pre-phase TAP pass sets, and inspect every newly failing test that was previously passing. A false-pass exposure must be recorded as a correctness improvement, not silently counted as a regression.

- [ ] **Step 5: Update documentation from artifacts**

Replace stale `32.1%`, `36.8%`, and old nextest totals only with the final current-HEAD measurements. Keep the explicit non-goals and mark deferred buckets with their remaining test counts.

- [ ] **Step 6: Commit the evidence and scoreboard**

```sh
git add conformance/fix-log.md ROADMAP.md README.md CONTEXT.md
git commit -m "docs: record current Test262 coverage baseline"
```

## Roadmap Gates and Stop Conditions

| Gate | Required outcome before proceeding |
|---|---|
| Measurement | Direct `language/*` list and current-HEAD TAP pass set recorded; Test262 commit pinned |
| Task 1 | Object result arrays and Error toString focused tests pass; Object/Error slices have no lost passes |
| Task 2 | Direct eval reads/writes caller bindings; strict/sloppy, `this`, `new.target`, closures, and `arguments` probes pass |
| Task 3A–3C | Phase/site-aware eval early errors, eval-local vars, and function-scope Annex B cases pass without false passes; detached-arrow and global/eval descriptor work remains explicitly deferred |
| Task 4 | One call router handles user callbacks, getters, throws, nested execution, jobs, and deadlines |
| Task 5 | Generator/async-generator `arguments` and mapped aliases pass |
| Task 6 | Module live bindings, defer triggers, re-exports, and namespace invariants pass |
| Task 7–9 | Collection, Promise, and Array slices improve with zero lost passes |
| Task 10 | Proxy/descriptor residual matrix passes without broad scope expansion |
| Final | Direct language target reached or the remaining gap is explicitly measured; all canonical Rust gates green; scoreboard current |

Stop a semantic task immediately when it requires changing the Test262 harness verdict, adding a new skip, or broadening v1 scope. Record that as a separate follow-up instead of hiding it in a coverage patch.

## Expected Outcome

The first three execution phases target a conservative net gain of roughly 500–1,000 direct-language passes after remeasurement, with the largest gains coming from eval context, generator arguments, and native callback re-entry. The full roadmap is designed to target a cumulative 1,500–2,500 unique direct-language passes and a final rate near or above 78%, but the acceptance criterion is the current-HEAD pass set and zero regressions—not the historical estimate.

This ordering follows composition over inheritance and separation of concerns: one environment model serves eval, modules, and Annex B; one call capability serves native coercion and callbacks; one canonical array allocator serves Object, callbacks, collections, and modules. It deliberately leaves Tier-2, cross-realm breadth, typed arrays, Unicode normalization, Workers, and `with` identifier writes outside the coverage acceleration path.
