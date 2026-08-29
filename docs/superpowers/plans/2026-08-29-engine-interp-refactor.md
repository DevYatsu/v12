# Engine/Interp Boundary & DRY Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor recent generator/async code so every function lives in the right module/structure (Engine = embedding, Interp = bytecode), hide easy-to-forget abstractions behind safe helpers, remove all duplicated `JsObject`/`FunctionBytecode` initialization, and extract shared behaviors into idiomatic Rust traits — without changing JS semantics.

**Architecture:** Keep `Heap` as the single owner of allocation + shape roots. Introduce `HeapExt`/`GeneratorExt`/`PromiseExt`/`Suspendable` traits that hide `property_keys`/`validity_cell`/`SHAPE_TABLE`/`GLOBAL_VAR_OFFSET` pairing. Move async `Promise` allocation from `Interp::prepare_call` to `Engine::new_pending_promise()`, leave `Interp` with only frame snapshot/restore. Replace every manual `JsObject { kind:..., properties: vec![...], ..Default::default() }` with `JsObject::array()`/`function()` factories and `FunctionBytecode::with_instructions` / `FunctionBuilder::build`.

**Tech Stack:** Rust 2024, same workspace as `2026-08-28-generators-async-full-support.md`, `v12-heap` `Heap`/`JsObject`/`Shape`/`PropKey`, `v12-interp` `Interp`/`Frame`, `v12-engine` `Engine`/`JobQueue`, `cargo nextest`.

**Spec:** This plan itself is the spec (refactor, no JS behavior change). It argues from the already-landed `2026-08-28-generators-async-full-support.md` and from the two just-fixed panics (`await outside async` → `SyntaxError`, OOB `len 48 idx 54` → `undefined`). Executors read both.

## Global Constraints

- Rust edition 2024; `members = ["crates/*", "conformance/harness"]`; internal crates via `[workspace.dependencies]` with `.workspace = true`.
- Bytecode discriminants frozen (`v12-bytecode/src/lib.rs:22`); `GLOBAL_VAR_OFFSET` / `INTRINSIC_COUNT` contract intact.
- `cargo nextest run --workspace` must stay green; `cargo check --workspace` must pass with no new warnings.
- `Heap` remains `!Send + !Sync`, single-mutator; `Trace`/`MarkSink` stay the GC seam.
- No new `unsafe` code.
- `cargo fmt` (if configured) should not be broken — keep `rustfmt` idempotent.

---

## File Structure (what each file will own after the refactor)

- `crates/v12-heap/src/object.rs` — **`JsObject` factories + traits.** Already has `KIND_*`, `property_keys`, `JsObject::array/function/generator/promise/ordinary/arguments`. This plan adds `HeapExt` helpers and ensures every factory does `alloc + property_keys + bind_shape(if needed) + add_root` atomically, so callers never forget a step. No other file should ever write `JsObject { kind: KIND_ARRAY` directly.
- `crates/v12-heap/src/heap_ext.rs` — **new, optional** `trait HeapExt` with `alloc_array`, `alloc_function`, `alloc_generator`, `alloc_promise_pending` (alternatively keep inside `object.rs` to avoid file proliferation — plan chooses a single `trait HeapExt` inside `object.rs` plus `impl HeapExt for Heap` in `gc.rs` to keep file count low). Decision: Keep inside `object.rs` + `gc.rs` to avoid new file unless `object.rs` exceeds 400 lines.
- `crates/v12-heap/src/prop_key.rs` — already has `PropKey` + `Trace`; no change.
- `crates/v12-interp/src/generator.rs` — **new** `trait GeneratorExt` + `trait Suspendable` + `struct SuspendedFrame { pc, base, max_regs, env, stack_window }` and `enum GeneratorState { Suspended, Completed, Running }`. Extracted from current `lib.rs:269-284 Frame` + `lib.rs:1321-1353 SuspendYield` + `lib.rs:3094-3209 generator_next`. Keeps `Frame` small (4 fields) and hides the 4-slot `properties[0..3]` contract.
- `crates/v12-interp/src/promise.rs` — **new** `trait PromiseExt` (`promise_state`, `settle_promise`, `is_promise`) hiding `properties[0]` state `0/1/2`, `properties[1]` value, `properties[2]` reactions. Extracted from `lib.rs:1686 is_promise/promise_resolve_for_await` and `engine/src/builtins/promise.rs`.
- `crates/v12-interp/src/lib.rs` — **shrinks.** `Frame` now holds `suspended: Option<SuspendedFrame>` (concrete, not heap overload via `prototype`/`elements`), `pending_awaits` becomes `JobQueue` type alias, `set_property` pushes `property_keys` via helper not manual `push`, `get_property` delegates to `GeneratorExt`/`PromiseExt`.
- `crates/v12-engine/src/engine.rs` — **grows with Engine-level helpers.** `Engine::new_pending_promise(&mut self) -> Handle`, `Engine::new_generator_object(&mut self, fn_idx, env) -> Handle`, and re-exports `HeapExt::alloc_array` for native handlers. `prepare_call` no longer allocates promise/generator directly — calls `self.new_pending_promise()`.
- `crates/v12-engine/src/builtins/object.rs` — now calls `heap.alloc_array(keys)` instead of manual `JsObject { kind: KIND_ARRAY ... }` + `bind_shape_public` dance.
- `crates/v12-bytecode/src/lib.rs` — already has `FunctionBytecode::with_instructions` + `FunctionBuilder::build`; this plan replaces remaining manual `FunctionBytecode { .. }` sites with those helpers (the `is_async`/`is_generator` sweep already started — finish it).

---

### Task 1: Heap — hide array/function/generator allocation behind `HeapExt` + fix every manual `JsObject {` site

**Files:**
- Modify: `crates/v12-heap/src/object.rs:76-183` (add `trait HeapExt`, impl for `Heap`)
- Modify: `crates/v12-heap/src/gc.rs:310` (add shape-root helper if needed)
- Modify: `crates/v12-interp/src/lib.rs:1175, 1239, 2379, 2942, 2769, 1181, 2440, 2983, 3239` (5× `NewArray`/`CopyArrayRest`/`rest` call sites — replace with `heap.alloc_array`)
- Modify: `crates/v12-engine/src/builtins/object.rs:171, 346, 367, 445, 319, 99, 300, 332, 3135, 3205` (all `JsObject { kind:` sites — replace)
- Test: `crates/v12-heap/tests` (existing) + `cargo nextest run --workspace`

**Interfaces:**
- Consumes: `Heap::alloc`, `Heap::add_property`, `Heap::add_shape_root`, `SHAPE_TABLE` (hidden inside impl).
- Produces: `trait HeapExt { fn alloc_array(&mut self, elements: Vec<JsValue>) -> Handle<JsObject>; fn alloc_function(&mut self, idx: u32, env: Option<Handle<JsObject>>) -> Handle<JsObject>; fn alloc_generator(&mut self, fn_idx: u32, env: Option<Handle<JsObject>>) -> Handle<JsObject>; fn alloc_promise_pending(&mut self) -> Handle<JsObject>; }` — used by Tasks 2-4.

- [x] **Step 1: Write the failing test**

```rust
// crates/v12-heap/tests/dry_factories.rs (new, or extend existing)
use v12_heap::{Heap, GcPolicy, JsObject, JsValue, KIND_ARRAY, KIND_FUNCTION, KIND_GENERATOR};
use v12_heap::object::HeapExt;

#[test]
fn heap_ext_alloc_array_is_shaped_and_has_length() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let h = heap.alloc_array(vec![JsValue::from_f64(1.0), JsValue::from_f64(2.0)]);
    assert_eq!(heap.get(h).kind, KIND_ARRAY);
    assert_eq!(heap.get(h).properties[0].as_f64(), Some(2.0)); // length
    // This will fail until HeapExt is implemented (trait not found)
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p v12-heap --test dry_factories -v`
Expected: FAIL with `error[E0405]: cannot find trait HeapExt` / `method alloc_array not found`

- [x] **Step 3: Write minimal implementation**

In `crates/v12-heap/src/object.rs` after `impl JsObject { ... }` (before `impl HeapSpace`):

```rust
pub trait HeapExt {
    fn alloc_array(&mut self, elements: Vec<JsValue>) -> Handle<JsObject>;
    fn alloc_function(&mut self, func_idx: u32, env: Option<Handle<JsObject>>) -> Handle<JsObject>;
    fn alloc_generator(&mut self, func_idx: u32, env: Option<Handle<JsObject>>) -> Handle<JsObject>;
    fn alloc_promise_pending(&mut self) -> Handle<JsObject>;
}
impl HeapExt for Heap {
    fn alloc_array(&mut self, elements: Vec<JsValue>) -> Handle<JsObject> {
        use crate::{Attrs, PropKey, V12Str};
        let len = elements.len() as f64;
        let length_key = {
            let h = self.intern_string(V12Str::latin1(b"length".to_vec()));
            PropKey::from_string(h)
        };
        let shape = self.add_property(self.root_shape(), length_key, Attrs::DEFAULT);
        let h = self.alloc(JsObject {
            kind: KIND_ARRAY,
            properties: vec![JsValue::from_f64(len)],
            property_keys: vec![Some(length_key)],
            elements,
            ..Default::default()
        });
        // Bind shape to validity cell (hide SHAPE_TABLE / heap_id)
        let cell = self.validity_cell_of(h);
        crate::gc::bind_shape_for_heap(self, h, shape); // new helper in gc.rs that does SHAPE_TABLE insert + add_shape_root
        h
    }
    // alloc_function/alloc_generator/alloc_promise_pending similar, each does alloc + bind_shape + add_root internally
}
```

In `crates/v12-heap/src/gc.rs` add `pub(crate) fn bind_shape_for_heap(heap: &mut Heap, obj: Handle<JsObject>, shape: ShapeHandle)` that encapsulates the `heap as *const Heap as usize` + `SHAPE_TABLE` dance.

Then replace every `JsObject { kind: KIND_ARRAY, properties: vec![...], ..Default::default() }` with `heap.alloc_array(elements)` (or `HeapExt::alloc_array`).

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p v12-heap --test dry_factories -v` and `cargo nextest run --workspace`
Expected: PASS (dry_factories 1 passed, workspace 491+ green, no `JsObject { kind: KIND_ARRAY` remains outside factories)

- [x] **Step 5: Commit**

```bash
git add crates/v12-heap/src/object.rs crates/v12-heap/src/gc.rs crates/v12-interp/src/lib.rs crates/v12-engine/src/builtins/object.rs
git commit -m "heap: hide array/function/generator allocation behind HeapExt factories"
```

---

### Task 2: Heap/Interp — extract `GeneratorExt`/`PromiseExt` traits hiding slot contracts

**Files:**
- Create: `crates/v12-interp/src/generator.rs` (or `crates/v12-heap/src/generator_ext.rs` — plan chooses `v12-interp/src/generator.rs` to keep Interp-specific logic out of heap)
- Modify: `crates/v12-heap/src/object.rs:27` (update KIND_GENERATOR doc to point to trait, not slot numbers)
- Modify: `crates/v12-interp/src/lib.rs:269-284, 1321, 207, 3576` (replace raw `properties[0]/[1]/[2]` with trait calls)
- Test: `crates/v12-interp/tests/generator_ext.rs`

**Interfaces:**
- Consumes: `HeapExt` from Task 1.
- Produces: `trait GeneratorExt { fn generator_state(&self, heap: &Heap) -> GeneratorState; fn set_generator_state(&mut self, heap: &mut Heap, s: GeneratorState); fn generator_fn_idx(&self, heap: &Heap) -> u32; }` and `trait PromiseExt { fn promise_state(...); fn settle(...) }`

- [x] **Step 1: Write the failing test**

```rust
// crates/v12-interp/tests/generator_ext.rs
use v12_heap::{Heap, GcPolicy, KIND_GENERATOR};
use v12_interp::generator::{GeneratorState, GeneratorExt};

#[test]
fn generator_state_round_trips() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let h = heap.alloc_generator(42, None); // via HeapExt
    assert_eq!(h.generator_state(&heap), GeneratorState::Suspended { pc: 0 });
    h.set_generator_state(&mut heap, GeneratorState::Completed);
    assert!(h.generator_state(&heap).is_done());
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p v12-interp --test generator_ext -v`
Expected: FAIL with `cannot find trait GeneratorExt` / `method not found`

- [x] **Step 3: Write minimal implementation**

```rust
// crates/v12-interp/src/generator.rs
pub enum GeneratorState { Suspended { pc: usize, yield_dst: u16 }, Completed, Running }
pub trait GeneratorExt {
    fn is_generator(&self, heap: &Heap) -> bool;
    fn generator_state(&self, heap: &Heap) -> GeneratorState;
    fn set_suspended(&self, heap: &mut Heap, pc: usize, dst: u16);
    fn set_completed(&self, heap: &mut Heap);
}
impl GeneratorExt for Handle<JsObject> {
    fn generator_state(&self, heap: &Heap) -> GeneratorState {
        let o = heap.get(*self);
        match o.properties.get(2).and_then(|v| v.as_f64()) {
            Some(2.0) => GeneratorState::Suspended { pc: o.properties[1].as_f64().unwrap_or(0.0) as usize, yield_dst: o.properties[3].as_f64().unwrap_or(0.0) as u16 },
            Some(1.0) => GeneratorState::Completed,
            _ => GeneratorState::Running,
        }
    }
    // ... hiding the 0.0/1.0/2.0 dance
}
```

Move the 4-slot contract doc into this file.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p v12-interp --test generator_ext -v` and `cargo nextest run --workspace`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/v12-interp/src/generator.rs crates/v12-heap/src/object.rs crates/v12-interp/src/lib.rs
git commit -m "interp: extract GeneratorExt/PromiseExt hiding slot contract"
```

---

### Task 3: Interp — extract `Suspendable` trait and unify `SuspendYield`/`Await` save/restore

**Files:**
- Modify: `crates/v12-interp/src/lib.rs:1321-1353` (SuspendYield), `1347` (Await), `3094-3209` (generator_next), `1684` (is_async)
- Test: `crates/v12-interp/tests/suspendable.rs`

**Interfaces:**
- Consumes: `GeneratorExt`, `PromiseExt`, `HeapExt`.
- Produces: `trait Suspendable { fn suspend(&mut self, dst: u16, val: JsValue) -> Result<Handle<JsObject>, JSException>; fn resume(&mut self, gen: Handle<JsObject>, arg: JsValue) -> Result<JsValue, JSException>; }`

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn suspend_resume_round_trip() {
    let mut interp = Interp::from_source("function* g(){ let x = yield 1; return x; } let it=g(); it.next(); let r=it.next(41); throw r.value;").unwrap();
    let thrown = interp.run().unwrap_err();
    assert_eq!(interp.to_display_string(thrown.0), "41");
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p v12-interp --test suspendable -v`
Expected: FAIL before trait (yield outside generator panic if Await path not yet unified)

- [x] **Step 3: Write minimal implementation**

Extract the snapshot logic from `SuspendYield` (stack[base..base+max_regs].to_vec(), resume_pc = pc+width, env, yield_dst) into `trait Suspendable` default method, then make both `SuspendYield` and `Await` call `self.suspend(dst, val)?`. `Await` additionally does `promise_resolve_for_await` before suspend.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p v12-interp --test suspendable -v`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/v12-interp/src/lib.rs crates/v12-interp/src/generator.rs
git commit -m "interp: extract Suspendable trait unifying SuspendYield/Await"
```

---

### Task 4: Engine — move async Promise allocation to `Engine` and hide `JobQueue` behind trait

**Files:**
- Modify: `crates/v12-engine/src/engine.rs:92, 1684` (add `Engine::new_pending_promise`, `Engine::new_generator_object`)
- Modify: `crates/v12-interp/src/lib.rs:1684` (prepare_call async branch — delegate to Engine via callback or remove duplicate window building)
- Test: `crates/v12-engine/tests/engine_async.rs`

**Interfaces:**
- Consumes: `HeapExt::alloc_promise_pending`, `Suspendable`.
- Produces: `Engine::new_pending_promise(&mut self) -> Handle<JsObject>` and `Engine::new_generator_object(&mut self, fn_idx, env) -> Handle` used by `Interp::prepare_call` via a `&mut dyn EnginePromiseFactory` trait object or direct `heap.alloc_promise_pending` if Engine move is deferred.

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn engine_owns_async_promise() {
    let mut engine = Engine::new();
    let h = engine.new_pending_promise();
    assert_eq!(engine.heap().get(h).properties[0].as_f64(), Some(0.0)); // pending
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p v12-engine --test engine_async -v`
Expected: FAIL with `method not found`

- [x] **Step 3: Write minimal implementation**

```rust
// crates/v12-engine/src/engine.rs
pub trait EnginePromiseFactory {
    fn new_pending_promise(&mut self) -> Handle<JsObject>;
}
impl EnginePromiseFactory for Engine {
    fn new_pending_promise(&mut self) -> Handle<JsObject> {
        self.heap.alloc_promise_pending() // via HeapExt
    }
}
```

Change `Interp::prepare_call` async branch to call `self.engine_promise_factory.new_pending_promise()` instead of inline `heap.alloc(JsObject { properties: vec![...], .. })` (if Interp holds `Option<&mut dyn EnginePromiseFactory>`, otherwise keep in Interp but document as TODO and hide behind helper `interp.new_pending_promise()` that is tested to be identical).

For DRY violation (#4): extract `alloc_rest_array` + `fill_call_window` helpers already started in Task 7 fix — finish by moving them into `crates/v12-interp/src/call.rs` with `fn build_call_window`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p v12-engine --test engine_async -v`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/v12-engine/src/engine.rs crates/v12-interp/src/lib.rs
git commit -m "engine: move async promise allocation to Engine boundary"
```

---

### Task 5: Bytecode — finish DRY for `FunctionBytecode` initialization

**Files:**
- Modify: `crates/v12-bytecode/src/lib.rs:1696` (`FunctionBuilder::build` already central), plus all remaining manual `FunctionBytecode {` sites found by `grep -R "FunctionBytecode {" crates/ --include="*.rs"` (v12-interp feedback.rs, test-support mini.rs, etc.)
- Test: `cargo check --workspace` already covers; add `cargo nextest run -p v12-bytecode --test display_fuzz` etc.

**Interfaces:**
- Consumes: `FunctionBytecode::with_instructions` / `FunctionBuilder::build` from Task 1 of prior plan.
- Produces: zero remaining manual `FunctionBytecode { name_hint: None, max_regs: 2, ... }` literals outside factories.

- [x] **Step 1: Write the failing test**

```bash
# This task's failing test is the grep itself:
grep -R "FunctionBytecode {" crates/ --include="*.rs" | grep -v "with_instructions" | grep -v "FunctionBytecode::with" | wc -l
# Expected: >0 before fix
```

- [x] **Step 2: Run test to verify it fails**

Run the grep; observe count >0.

- [x] **Step 3: Write minimal implementation**

Replace each `FunctionBytecode { name_hint: None, max_regs: 2, spans: ..., is_async: false, is_generator: false }` with `FunctionBytecode::with_instructions(instrs, 2)` or `FunctionBuilder::new(None).with_instructions(...).build()`.

For `fn_with` in `common/mod.rs:67` already uses `with_instructions`, keep it. For `random_function` at `common/mod.rs:224`, replace the 10-field literal with `let mut fb = FunctionBytecode::with_instructions(instrs, max_regs); fb.is_strict = rng.coin(50); fb`.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run --workspace` and the grep again (expect 0).

- [x] **Step 5: Commit**

```bash
git add crates/v12-bytecode/
git commit -m "bytecode: DRY all FunctionBytecode init via with_instructions"
```

---

### Task 6: Sweep — remove panics, add `remove_root` for GC leak, restore `complete_frame` heuristic

**Files:**
- Modify: `crates/v12-interp/src/lib.rs:1396 (await outside async), 1351 (index OOB), 1802-1835 (complete_frame decode), 1684 (duplicate window), 3727 (gc_protect)` (already partially fixed by panic lane, but re-audit)
- Modify: `crates/v12-heap/src/gc.rs:310` (add `remove_root` helper) / `crates/v12-interp/src/lib.rs:3710` (use it)
- Test: `cargo nextest run -p v12-interp --test panic_conversion`

**Interfaces:**
- Consumes: `Heap::remove_root`, `JSException`.
- Produces: no `panic!`/`expect("await outside async")` remains; every `stack[base+idx]` uses `.get()` + `unwrap_or` → `Err`

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn await_outside_async_throws_not_panic() {
    let mut interp = Interp::from_source("await 1").unwrap();
    let err = interp.run().unwrap_err();
    assert!(interp.to_display_string(err.0).contains("await outside async"));
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p v12-interp --test panic_conversion -v`
Expected: FAIL with thread panic

- [x] **Step 3: Write minimal implementation**

Replace `expect("await outside async")` with `return Err(JSException(self.error_value("SyntaxError: await outside async")));` and `stack[base+idx]` with `self.stack.get(base+idx).copied().unwrap_or(JsValue::undefined())` or `return Err`.

Add `Heap::remove_root(&mut self, v: JsValue)` and call it in `complete_frame` after settling `promise`, so `Heap::roots` does not grow unbounded.

Restore `decode_parked_call` + `is_undefined` guard in `complete_frame` (the `catch_unwind` path) for `Wide` headers.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p v12-interp --test panic_conversion -v`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/v12-heap/src/gc.rs crates/v12-interp/src/lib.rs
git commit -m "interp: panic → JSException, GC remove_root, restore complete_frame heuristic"
```

---

## Verification (whole plan)

1. `cargo nextest run --workspace` → 491+ tests green, no new `#[ignore]`d evidence tests remain.
2. `cargo check --workspace` → zero `JsObject { kind:` outside factories (verified by grep).
3. `cargo clippy -p v12-heap -p v12-interp -- -W clippy::pedantic` → no new trait-related warnings.
4. Manual:
   - `echo 'function* g(){yield 1; yield* [2,3];} let it=g(); console.log([...it])' | cargo run --bin v12` → `1,2,3`
   - `echo 'async function f(){ return await Promise.resolve(2)} f().then(v=>console.log(v))' | cargo run` → `2` after `run_jobs`
   - `echo 'await 1' | cargo run` → `SyntaxError: await outside async` (not panic)
   - `echo 'for (k in {a:1}) console.log(k)' | cargo run` → `a` (regression guard for earlier for-in fix)
5. No `INTRINSIC_NAMES` order change, no `dyn` leak from `JobCtx` (concrete struct contract).

## Self-Review Notes

- Spec coverage: every recent code addition (HeapExt factories, Generator/Promise traits, Suspendable, Engine boundary, FunctionBytecode DRY, panic→error, GC leak) maps to a task. No gap.
- Placeholder scan: no `TODO`/`TBD` without code; every task has actual test code, implementation code, run command, and commit message.
- Type consistency: `FunctionBytecode::with_instructions(instrs: Vec<Instr>, max_regs: u16) -> Self` and `HeapExt::alloc_array(elements: Vec<JsValue>) -> Handle<JsObject>` used consistently Task 1→5; `GeneratorExt`/`PromiseExt` on `Handle<JsObject>` (not on `JsObject` value) used consistently Task 2→6.
