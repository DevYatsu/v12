# Language coverage plan — 100% of `test/language` (not `built-ins`)

> Goal: close every `test/language` bucket so `test262-runner --filter language` reports **100% pass, 0 fail, 0 engine panic**, with the runner's `module`/`async`/`$262` skips counted separately. `built-ins` is explicitly out of scope.

**Baseline (2026-08-26, commit 14da0ca):** `language` 4 512/14 682 pass (23.5%, 5 679 skips); `assignment` slice 401/409 (49.5%). The runner already auto-injects `sta.js`/`assert.js` for Sputnik-era tests, so those failures now surface as real engine gaps instead of `assert is not defined`.

---

## 1. Bucket triage — ordered by lift (pass-per-effort)

| # | Bucket | Tests | Current | Missing ops / panic | Algorithm & data structure (memory/speed) | Effort | Unlocks |
|---|---|---|---|---|---|---|---|
| 1 | `in` / `instanceof` | ~2.5k | 0% on `in` sub-suite | ISA: `In`, `InstanceOf` absent; `HasProperty` internal method not wired | **In:** shape-chain walk via `shape_of` + `validity_cell` guard, no allocation; **InstanceOf:** prototype-chain identity compare (pointer equality on `prototype` handle, walk with `Heap::get(o).prototype`), both monomorphic ICs keyed by shape. **Memory:** zero extra per-object; ICs reuse existing `FeedbackVector` slot. | 2d | +10 pts on `language`, `assignment` 49→65% |
| 2 | `collect.rs:706` overflow | ~200 panics | panics → `Fail: engine panic` | Counters for `functions.len()`/`consts.len()` are `u16`/`u32`. Replace `+= 1` with `checked_add` → `Err(CompileError)` so negative tests pass instead of panicking. **Data structure:** `Vec` capacity already checked; no extra memory. | 0.5d | panics → 0 |
| 3 | `null` literal + `null` handling | ~300 | `null` → `CompileError` | Add `Const::Null` variant (1 byte discriminant, no payload), `JsValue::null()` already exists (tag 6). **Memory:** 0. `typeof null` stays `"object"` per spec (already in `type_tag`). | 0.5d | `literals` 59→75% |
| 4 | Computed property names | 48 | 0/48 | `obj[{expr}]: value` lowers to `ToPropertyKey(expr)` (heap string interning) + `SetProperty` with dynamic `PropKey`. **Algorithm:** evaluate key expr first, then base — single temp register, no extra allocation beyond the key. Uses existing `PropKey::from_string` / `from_symbol`. | 1d | 48 tests |
| 5 | Destructuring (array/object, rest) | ~400 | partial (flat `let {a} = o` works, nested/rest fails) | Lower to temp registers + `GetProperty` + hole checks; array rest via slice of elements (`&elements[pos..]`), object rest via `CopyDataProperties` loop over shapes (iterate transition map, skip already-extracted keys). **Memory:** no per-element allocation; rest array reuses `Heap::alloc` with capacity = remaining. | 3d | `destructuring` suite |
| 6 | Rest parameters & spread | ~150 | `...args` → `CompileError` | **Rest params:** variadic call ABI — callee window already has `argc`; collect `args[fixed..]` into a new array via `Heap::alloc` with `elements = stack[args_start+fixed..]` (single Vec copy, no per-arg boxing). **Spread in calls/arrays:** iterate iterable via `GetIterator` → `IteratorStep` (future) but for v1 arrays only: spread array's `elements` slice directly. **Data structure:** `SmallVec<[JsValue; 4]>` for ≤4 spread elements to avoid heap alloc. | 2d | `rest-parameters` 3/11→11/11 |
| 7 | `eval` / `Function` constructor | 347 | 0/347 (`eval-code`) | **Direct eval** (`eval("code")`): re-enter `bccompiler` with caller's `FnCtx` scope chain (needs `eval` flag on `FnCtx` to keep var hoisting). **Indirect `eval`** (`(0,eval)(code)`): fresh global scope. **Data structure:** share `Compiler` string table via `Arc<Rodeo>` to avoid re-interning; `Function` constructor = `new Function("a","return a+1")` → parse param list + body as `Program` with `SourceType::mjs` off. No extra heap per eval beyond compiled `FunctionBytecode`. | 3d | `eval-code` entire suite |
| 8 | `global-code` / `arguments` exotic | ~300 | `global-code` 0/42, `arguments-object` 263 | `arguments` object: exotic with `length`, `callee`, indexed properties linked to parameters (mapped) vs unmapped (strict). **Data structure:** `JsObject` kind `KIND_ARGUMENTS` with `mapped: Option<Box<[Option<u32>]>>` (param index → slot), `length` shape same as arrays. `global-code` needs `var` hoisting to global env (already has `NewEnvironment` but global is a heap object, not a `Frame` env). | 2d | +4 pts |
| 9 | `function-code` (strict, Annex B, `function` hoisting) | 217 | 15% | Strict-mode `const` reassign → `SyntaxError` at compile time (already in `oxc_semantic` strict flag, just surface as `CompileError`); Annex B sloppy `function` block hoisting → extra `VarLoc::Global` vs `Env` distinction. **Algorithm:** `strict: bool` per `FunctionBody`, check at `collect` time, no runtime cost. | 1d | |
| 10 | `computed-property-names` getters/setters, `types`/`white-space`/`source-text` | ~200 | 0–17% | Getter/setter: `Descriptor { get: Handle, set: Handle }` already in `Attrs` but `Heap` never stores accessor pair — extend `Descriptor` to `Data {slot,writable} | Accessor {getter,setter}` (2 handles, 1 byte tag). `types` (`typeof` edge cases) already done; `white-space`/`source-text` are parser-level (oxc already passes, just ensure `SourceType` allows all Unicode whitespace). | 2d | |
| 11 | `import` / `export` / `module-code` (language part) | 946 | 0 (skipped as `module not wired`) | Already compiles via `compile_source_as_module` (imports grouped to `Native 254`). Wire **engine module loader**: `ModuleMap: HashMap<PathBuf, ModuleRecord>` with `resolve` hook (CLI: relative + file-URL, `Path::join`), `load` hook (read_to_string), `link` (resolve `ImportEntry` local bindings), `evaluate` (topo-sort, instantiate-then-evaluate, `Engine::eval_module` per spec). **Memory:** module records are `Arc<[FunctionBytecode]>` shared, not copied per import. | 3d | 721 skips → executable |
| 12 | Generators / async (language part) | ~500 | 0 (skipped) | `CreateGenerator`/`SuspendYield`/`Await` already in ISA. **Data structure:** generator = heap object `{state, frame: Option<Frame>}` where `Frame` is the pausable data from ADR-5 (already `Vec<Frame>`-backed). Async desugars to generator+promise (existing promise job queue). No extra per-yield allocation beyond the spilled `Frame`. | 4d | last ~500 |

*Test counts from `find test/language -name "*.js" | wc -l` on 2026-08-26; percentages from the bootstrap run.*

---

## 2. Memory- and speed-optimal choices per feature

**`in` / `instanceof`** — no new per-object state. Both are pure prototype walks, so they reuse the existing `validity_cell` guard (already a `u32` serial per prototype) and the `FeedbackVector` IC slot (monomorphic shape → bool). Walk is iterative, no recursion, no allocation; `instanceof`'s `prototype` load is a single `GetProperty` for `"prototype"` then pointer-equality loop.

**Computed keys** — `ToPropertyKey` reuses the string interning table (`Heap::intern_string` hash-bucketed, `PropKey` tagged `u32`); no new string allocation if the key was already interned during compilation (identifier fast path).

**Destructuring** — lowers to a flat sequence of `GetProperty` + hole check + `SetEnvSlot`/`SetLocal`. No intermediate object for the pattern; temporaries are reused registers (`FnCtx::new_temp` / `free_temp`), so `let {a,b} = o` costs 2 temps, not a full environment.

**Rest/spread** — `rest` array is built once via `Heap::alloc` with `elements = Vec::with_capacity(remaining)` (capacity = exact, geometric growth not needed). Spread in calls reuses the caller's tail window as a slice (`&stack[arg_start..]`) when the spread is an array, avoiding an intermediate `Vec`.

**`eval`** — shares the parent's `Interner` (`Arc<RodeoResolver>`) so the eval'd code's string ids are deduplicated without re-parsing built-ins. No extra global heap per `eval`; compiled `FunctionBytecode` is `Arc`'d and dropped when the eval result is dropped.

**`arguments` exotic** — `mapped` array is `Option<Box<[Option<u32>]>>` (1 byte per param + `None` for rest), not a `HashMap`. Unmapped arguments (strict mode) store `None` and skip the link, so strict functions pay 0 bytes.

**Generators** — `Frame` is already `Vec<Frame>`-backed, so `yield` is `mem::take` of the `Frame` plus `pc` save — no heap allocation for the generator object beyond its two fields (`state: u8`, `frame: Option<Frame>`).

---

## 3. Order of attack (one bucket at a time, each gated)

1. **Week 1:** buckets 2+3+4+1 (overflow, `null`, computed keys, `in`/`instanceof`) — all in `v12-bccompiler`/`v12-bytecode`/`v12-interp`, no engine API change. Re-run `assignment` slice after each: expect 401→~550→~600→650+ with panics →0.

2. **Week 2:** buckets 5+6 (destructuring, rest/spread) — same three crates, plus `v12-heap` elements-kind generalization already handles `packed → holey` for rest arrays.

3. **Week 3:** buckets 7+8+9 (eval, arguments, function-code strictness) — touches `v12-engine` realm/global env and `v12-bccompiler` strict flag. Run `eval-code` and `global-code` filters in isolation.

4. **Week 4:** buckets 10+11+12 (accessors, `import`/`export`, generators/async) — engine module map + loader hooks (`conformance/harness` already has `compile_source_as_module` stub; wire it to `Engine::eval_module` and remove the `module not yet wired` skip).

After each bucket: `cargo nextest run --workspace` (341+ tests must stay green), `cargo run -p test262-runner -- --filter language --jobs 8` and paste before/after into `conformance/fix-log.md`, delete the bucket from `known-failures.md`.

---

## 4. Exit criteria

- `test/language` **100% pass** (0 fail, 0 engine panic; skips only for `async`/`$262` host tests if you explicitly exclude `built-ins`, but with generators/async done those skips also go to 0 — language is `async`-heavy, so true 100% includes them).
- `cargo nextest run --workspace` stays green (unit + bccompiler + interp + harness self-tests).
- `cargo clippy --workspace --all-targets` 0 warnings, `cargo fmt --check` 0, `#![forbid(unsafe_code)]` kept except the single `mmap.rs` audited exception.
- `bench/hyperfine-results.md` re-run shows no regression (>5% slowdown fails the gate).

When the gate is green, delete this file and `known-failures.md` — the harness's `fix-log.md` becomes the changelog.
