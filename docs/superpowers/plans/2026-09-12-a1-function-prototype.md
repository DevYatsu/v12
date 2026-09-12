# A1: Function.prototype + real bind — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Function.prototype` a real callable function object and implement `Function.prototype.bind` well enough that `conformance/test262/harness/propertyHelper.js` loads and `language/expressions/class/elements` stops failing with `TypeError: callee is not a function`.

**Architecture:** Two root causes. (1) `function_proto` is allocated as `Kind::Ordinary` and never linked to the `Function` constructor, so `function_method_surface` (which early-returns unless `kind == Kind::Function`) never fires and `Function.prototype.call` reads `undefined`. (2) `NativeId::FunctionBind` is a pass-through stub that returns its receiver, so the curried-bind idiom `Function.prototype.call.bind(...)` cannot work. Fix (1) by making `function_proto` a `Kind::Function` object and adding `Function` to the `install_ctor` link loop. Fix (2) by adding a traced `FunctionTarget::Bound(Handle<JsObject>)` variant backed by a small state object, and dispatching it through `call_object`.

**Tech Stack:** Rust, `cargo nextest`, `v12-cli`, `test262-runner`.

**Spec:** `docs/superpowers/specs/2026-09-12-known-failures-design.md` §1

## Global Constraints

- Never run `git stash`, `git reset`, `git checkout`, or workspace-wide `cargo fmt` (2026-09-02 incident).
- Run tests with `cargo nextest run --workspace` (not `cargo test`).
- v12 CLI runs: `cargo run -q -p v12-cli --bin v12 -- /tmp/t.js`.
- test262 runs: `cargo run -q -p test262-runner --bin test262-runner -- --filter <f> --jobs 8 [--verbose]`.
- Large test262 runs must use the default human format; never `--format json`/`tap` on large slices.
- Commit each verified task; the tree may not be left dirty between tasks.

---

### Task 1: `Function.prototype` is a callable function linked to the `Function` ctor

**Files:**
- Modify: `crates/v12-engine/src/realm.rs` (prototype alloc at ~:153-159; `install_ctor` loop at ~:168-179)
- Modify: `crates/v12-heap/src/object.rs` (only if a constructor helper is needed)
- Test: `/tmp/t1.js` (scratch, not committed)

**Interfaces:**
- Consumes: `alloc_root(heap)` (`realm.rs:245`), `JsObject::function(target, env)` (`object.rs:232`), `crate::builtins::install_ctor(heap, ctor, proto)` (`mod.rs:263`).
- Produces: a `function_proto` heap object with `kind == Kind::Function` that is linked to the global `Function` constructor (`Function.prototype === function_proto`, `function_proto.constructor === Function`).

- [ ] **Step 1: Write the failing test**

Create `/tmp/t1.js`:

```js
var r = [];
r.push(typeof Function.prototype);
r.push(Function.prototype.constructor === Function);
r.push(typeof Function.prototype.call);
r.push(typeof Function.prototype.bind);
r.push(Function.prototype.call === Function.prototype.call);
print(r.join("|"));
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/t1.js`
Expected (current): `undefined|false|undefined|undefined|false`

- [ ] **Step 3: Make `function_proto` a `Kind::Function` object**

In `crates/v12-engine/src/realm.rs`, replace the `function_proto` allocation (currently `let function_proto = alloc_root(heap);` near :157) with a callable allocation that has no bytecode target. Use the existing helper pattern:

```rust
// Function.prototype is itself callable and returns undefined; no bytecode target.
let function_proto = crate::builtins::helpers::alloc_obj(
    heap,
    JsObject::function(FunctionTarget::Bytecode(u32::MAX), None),
);
```

`u32::MAX` is the native-seam placeholder already used for the intrinsic placeholders (`realm.rs:47-73`). Ensure `FunctionTarget` and `JsObject` are in scope (they already are; `JsObject::function` is used elsewhere in the file).

- [ ] **Step 4: Link the `Function` ctor to `function_proto`**

`Ctx::define_method` currently returns `()`, so no caller can get the allocated function handle. Make it return `Option<Handle<JsObject>>` (the allocated `func`, `None` when target is `None`); this is backward compatible because existing callers may ignore a return value. Thread the return through `install_native_with_length` (`mod.rs:229`) and `install_native` (`mod.rs:249`) the same way.

In `crates/v12-engine/src/builtins/ctx.rs:374`, change the signature to `-> Option<v12_heap::Handle<v12_heap::JsObject>>`, return `None` on the early `let Some(target) = obj else { return None };`, and return `Some(func)` after `self.define_data_prop(target, name, JsValue::object(func))`.

Then in `crates/v12-engine/src/realm.rs:211`, capture the handle and link:

```rust
let function_ctor =
    crate::builtins::install_native(heap, Some(global), "Function", NativeId::Function);
if let Some(ctor) = function_ctor {
    crate::builtins::install_ctor(heap, ctor, function_proto);
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/t1.js`
Expected: `function|true|function|function|true`

- [ ] **Step 6: Run the workspace gate**

Run: `cargo nextest run --workspace`
Expected: all tests pass, same count as before (569).

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/realm.rs crates/v12-heap/src/object.rs
git commit -m "fix(realm): Function.prototype is a callable function linked to the Function ctor"
```

---

### Task 2: `FunctionTarget::Bound` + real `Function.prototype.bind`

**Files:**
- Modify: `crates/v12-heap/src/function.rs` (enum at :128-166, Debug :140-151, Trace :156-163)
- Modify: `crates/v12-interp/src/call_setup.rs` (match sites :70, :288, :388, :798, :963; `dispatch_native` `FunctionBind` arm :1215-1229)
- Modify: `crates/v12-interp/src/internal_methods.rs:297`
- Modify: `crates/v12-engine/src/realm.rs:142,281` and `crates/v12-heap/src/function.rs:150,160` if exhaustive matches appear
- Test: `/tmp/t2.js`

**Interfaces:**
- Consumes: `FunctionTarget` (`function.rs:128`), `call_object` (`v12-interp/src/lib.rs:1007`), `JsObject.elements: Vec<JsValue>` (`object.rs`).
- Produces: `FunctionTarget::Bound(Handle<JsObject>)` where the handle points to a **state object** whose `elements` are `[target_fn: JsValue, this_arg: JsValue, bound_args..: JsValue]`; `Function.prototype.bind(thisArg, ...args)` returns a `Kind::Function` object whose `callable == Bound(state)`.

- [ ] **Step 1: Write the failing test**

Create `/tmp/t2.js`:

```js
var join = Function.prototype.call.bind(Array.prototype.join);
print(join([1, 2], "-"));

var has = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
print(has({ x: 1 }, "x"));

function add(a, b, c) { return a + b + c; }
var addOne = add.bind(null, 1);
print(addOne(2, 3));
print(addOne.length);
print(typeof addOne);
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/t2.js`
Expected (current): `Uncaught TypeError: callee is not a function` on line 1.

- [ ] **Step 3: Add the `Bound` variant and keep all matches exhaustive**

In `crates/v12-heap/src/function.rs`, add to `FunctionTarget` (:128-166):

```rust
/// A bound function: the handle points at a state object whose `elements`
/// are `[target_fn, this_arg, bound_args..]`. Traced via `Trace` below.
Bound(Handle<JsObject>),
```

Update `Debug` (:140-151) with a `Bound(_) => write!(f, "FunctionTarget::Bound")` arm.

Update `Trace for FunctionTarget` (:156-163) so `Bound(state)` traces `JsValue::object(*state)` (or the equivalent used by `RealmEval`). This is mandatory: the state object is otherwise invisible to the collector. Read the existing `RealmEval` trace arm and mirror it.

- [ ] **Step 4: Implement `FunctionBind` to build the bound function**

In `crates/v12-interp/src/call_setup.rs`, replace the stub body at :1215-1229. If the task has a heap-access helper (`self.heap`), allocate the state object then the bound function:

```rust
// args[0] is thisArg; args[1..] are the bound prefix.
let this_arg = args.first().copied().unwrap_or(JsValue::undefined());
let bound_args: Vec<JsValue> = if args.len() > 1 { args[1..].to_vec() } else { Vec::new() };

// State object: elements = [target_fn, this_arg, bound_args..].
// GC discipline: build the Vec of children BEFORE allocating the state object,
// so no collection can run while the values live only in locals.
self.gc_protect();
let mut state = JsObject::default();
state.elements.push(JsValue::object(target));
state.elements.push(this_arg);
state.elements.extend_from_slice(&bound_args);
let state_h = self.heap.alloc(state); // Heap::alloc -> Handle<T>, infallible

let bound = self.heap.alloc(JsObject::function(
    v12_heap::FunctionTarget::Bound(state_h),
    None,
));
Ok(JsValue::object(bound))
```

`Heap::alloc` is infallible (`crates/v12-heap/src/gc.rs:410` returns `Handle<T>`, not a `Result`). Do not wrap it in `?` or `.expect()`. The `FunctionTarget::Trace` impl added in Step 3 keeps `state_h` reachable; `self.gc_protect()` matches the `map_set_method` pattern (`lib.rs:1329`).

- [ ] **Step 5: Dispatch `Bound` in the call seams**

Add a `Bound(state_h)` arm to every exhaustive `FunctionTarget` match. Exact sites and expected shapes (verified by read):

1. `crates/v12-interp/src/call_setup.rs:48-81` (`prepare_call`): returns `Result<CallOutcome, JSException>`. The `Native`/`Host` arms `return`; `Bytecode` falls through. Add:

```rust
v12_heap::FunctionTarget::Bound(state_h) => {
    let (target_fn, this_arg, prefix) = {
        let st = self.heap.get(state_h);
        let target_fn = st.elements[0].as_object().expect("bound target is an object");
        let this_arg = st.elements[1];
        let prefix: Vec<JsValue> = st.elements[2..].to_vec();
        (target_fn, this_arg, prefix)
    };
    let args_start = callee_slot + 2;
    let args_end = args_start + usize::from(argc);
    let mut call_args = prefix;
    call_args.extend_from_slice(&self.stack[args_start..args_end]);
    return self.call_object(target_fn, this_arg, &call_args).map(CallOutcome::Value);
}
```

2. `crates/v12-interp/src/call_setup.rs:279-293` (accessor dispatch): returns `Result<JsValue, JSException>`. Add:

```rust
v12_heap::FunctionTarget::Bound(state_h) => {
    let (target_fn, this_arg, prefix) = {
        let st = self.heap.get(state_h);
        (
            st.elements[0].as_object().expect("bound target is an object"),
            st.elements[1],
            st.elements[2..].to_vec(),
        )
    };
    let mut call_args = prefix;
    call_args.extend_from_slice(args);
    self.call_object(target_fn, this_arg, &call_args)
}
```

3. `crates/v12-interp/src/call_setup.rs:388` (`prepare_call_apply`): mirror site 1, but the forwarded args come from the apply args-array that the site already validated/collected (`fwd`), not from `self.stack`; append `fwd` after the prefix.

4. `crates/v12-interp/src/call_setup.rs:798` (`prepare_construct`): a bound function is constructible only if its target is constructible; for A1 the minimal correct arm is `return Err(JSException(self.error_value("TypeError: value is not a constructor")))` unless the target is a constructible function, in which case clear the bound `this_arg` and construct the target with the appended prefix. If wiring construct-correctly is large, the TypeError arm is acceptable for A1 and must be flagged in the commit message.

5. `crates/v12-interp/src/call_setup.rs:963` (`dispatch_native`-adjacent match): apply the same shape as the site it feeds.

6. `crates/v12-interp/src/internal_methods.rs:297`: the grouped `FunctionTarget::Bytecode(_) | FunctionTarget::RealmEval(_)` arm must gain `| FunctionTarget::Bound(_)` with the same behavior its group already has.

7. `crates/v12-heap/src/function.rs:150` (Debug) and `:160` (Trace) are handled in Step 3.

Read each site before editing; do not assume the shapes above are exact — they were captured from the current file, but line numbers shift. Also update `crates/v12-engine/src/realm.rs:142` and `:281` only if the compiler reports a non-exhaustive match there (those are constructors, not matches).

For the `map_set_method`-style recursion guard: `Bound` dispatch calls `call_object`, which re-enters `execute` bounded by `stop_at_frames`; this is the same re-entrancy the `FunctionCall`/`FunctionApply` arms already use (`call_setup.rs:1177`, `:1213`), so no new frame discipline is needed.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/t2.js`
Expected:
```
1-2
true
6
2
function
```

- [ ] **Step 7: Verify propertyHelper loads**

Run:

```bash
printf '%s\n' 'var o = {};' 'verifyProperty(o, "x", { value: 1, writable: true, enumerable: true, configurable: true, value: undefined });' > /tmp/ph.js
cargo run -q -p v12-cli --bin v12 -- /tmp/ph.js
```

Better: run the harness file directly:

```bash
cargo run -q -p v12-cli --bin v12 -- conformance/test262/harness/propertyHelper.js
```

Expected: no `TypeError: callee is not a function`. (A standalone load may report other errors; the only requirement is that `.call.bind` at line 31 resolves.)

- [ ] **Step 8: Run the workspace gate**

Run: `cargo nextest run --workspace`
Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/v12-heap/src/function.rs crates/v12-interp/src/call_setup.rs crates/v12-interp/src/internal_methods.rs
git commit -m "feat(interp): FunctionTarget::Bound and real Function.prototype.bind"
```

---

### Task 3: Re-score `class/elements` and record the gate

**Files:**
- Modify: `conformance/fix-log.md` (append entry)
- Modify: `conformance/known-failures.md` (strike the A1 bullet when green)

**Interfaces:**
- Consumes: the fixes from Tasks 1-2.
- Produces: a recorded before/after score and a green A1 bullet.

- [ ] **Step 1: Re-score the A1 slice**

Run: `cargo run -q -p test262-runner --bin test262-runner -- --filter language/expressions/class/elements --jobs 8`
Expected: `callee is not a function` failure count near 0 (was 718); pass count up from 413.

- [ ] **Step 2: Confirm propertyHelper-driven messages dropped**

Run: `cargo run -q -p test262-runner --bin test262-runner -- --filter language/expressions/class/elements --jobs 8 --verbose 2>&1 | grep -oE 'threw: [^|]*' | sed 's/[0-9]\+/N/g' | sort | uniq -c | sort -rn | head`
Expected: `callee is not a function` no longer top cluster.

- [ ] **Step 3: Run the full `language` re-score (human format)**

Run: `./conformance/run.sh --filter language --jobs 8`
Expected: pass count ≥ prior baseline; record total/pass/fail/skip.

- [ ] **Step 4: Append to `conformance/fix-log.md`**

Add an entry with: date, commit range, the A1 slice numbers, the full `language` numbers, and `cargo nextest run --workspace` count.

- [ ] **Step 5: Commit**

```bash
git add conformance/fix-log.md conformance/known-failures.md
git commit -m "docs(conformance): record A1 Function.prototype/bind results"
```
