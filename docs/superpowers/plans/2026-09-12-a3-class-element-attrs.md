# A3 — Class element attributes, delete semantics, Function.name — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make class method/accessor installs carry spec attributes (writable+configurable, non-enumerable), make `delete` remove a property from own-property queries, make `Object.defineProperty` honor descriptor flags, initialize instance fields on `this`, and stamp `Function.name` — collapsing the 560× `m descriptor should not be enumerable; m descriptor should be configurable` failure in `language/expressions/class/elements`.

**Architecture:** Add one narrow ISA opcode `DefineMethod = 72` (same three register slots as `SetProperty`, attrs fixed to `Attrs::BUILTIN`) so property installation carries attributes without touching the hottest opcode. Reuse `Attrs::BUILTIN` (WRITABLE|CONFIGURABLE) as the method attr set, add `Attrs::FUNCTION_PROTOTYPE` for the one remaining state. Add a heap `update_data_attrs` transition helper and an interp `define_own_data_attrs` helper. Make `delete`-holed data descriptors invisible to own-property queries via a shared `descriptor_is_live` predicate. Move public instance-field initialization from the class-expression frame into the constructor unit body on `REG_THIS`. Thread `function_name` from the collector to `alloc_closure`.

**Tech Stack:** Rust workspace (`v12-bytecode`, `v12-interp`, `v12-heap`, `v12-bccompiler`, `v12-engine`, `v12-native`, `test-support`), `oxc_ast` 0.147, `cargo nextest`, test262 harness.

**Spec:** `docs/superpowers/specs/2026-09-12-known-failures-design.md` (§A3).

## Global Constraints

- Never run parallel write lanes on shared files (2026-09-02 incident). All tasks run sequentially in one lane.
- No lane may run `git stash`, `git reset`, `git checkout`, or a workspace-wide `cargo fmt`.
- Run the workspace gate with `cargo nextest run --workspace` (canonical; **576 passed / 0 skipped** at plan time). Do not assume the count; verify.
- Large test262 runs use the default human format. Never `--format json` or `--format tap` on big filters. `./conformance/run.sh --filter <f> --jobs 8`.
- Opcode discriminants are part of the serialized format and must never be renumbered. `DefineMethod = 72` is next after `IteratorClose = 71`.
- `Attrs::BUILTIN` (WRITABLE|CONFIGURABLE) IS the spec method attribute set. Reuse it verbatim; do not add a third method state.
- Commit after each task. One commit per task.

---

### Task 1: Heap attributes — `FUNCTION_PROTOTYPE` and `update_data_attrs`

**Files:**
- Modify: `crates/v12-heap/src/shape.rs` (add const after `Attrs::BUILTIN`, ~line 78)
- Modify: `crates/v12-heap/src/gc.rs` (add `update_data_attrs` after `add_property`, ~line 547)
- Test: `crates/v12-bytecode/tests/decode_sweep.rs` is unaffected here; the heap check is a new unit test in `crates/v12-heap/src/gc.rs`

**Interfaces:**
- Consumes: `Attrs::new(w,e,c)`, `Descriptor::Data`, `Transitions::insert`, `Shape` fields (`parent`, `transitions`, `descriptors`, `proto_cell`, `num_own`).
- Produces:
  - `Attrs::FUNCTION_PROTOTYPE: Attrs` (writable, non-enumerable, non-configurable).
  - `pub fn Heap::update_data_attrs(&mut self, parent: ShapeHandle, key: PropKey, attrs: Attrs) -> ShapeHandle` — returns the child shape bound to the `(key, attrs)` edge, with the key's existing descriptor attrs replaced in a cloned descriptor list (never appended). Data-only; an accessor descriptor is replaced by a data descriptor at the next slot.

- [ ] **Step 1: Add the `FUNCTION_PROTOTYPE` constant**

In `crates/v12-heap/src/shape.rs`, directly after the `BUILTIN` constant:

```rust
    /// A function's own `prototype` property: writable, non-enumerable,
    /// non-configurable (ES 10.2.5 / 15.7.3.1). Not `BUILTIN`, which is
    /// writable+configurable.
    pub const FUNCTION_PROTOTYPE: Attrs = Attrs::new(true, false, false);
```

- [ ] **Step 2: Add the `update_data_attrs` helper**

In `crates/v12-heap/src/gc.rs`, directly after `add_property` (which ends at line 547):

```rust
    /// Reconfigures an existing own property's attributes, returning the child
    /// shape bound to `parent`'s `(key, attrs)` edge.
    ///
    /// Unlike [`Self::add_property`] this never appends a duplicate descriptor:
    /// the key's existing descriptor is replaced in the cloned list. Data and
    /// accessor descriptors both become a data descriptor. The descriptor's
    /// slot is preserved for data properties; a fresh slot is allocated only
    /// when the key is absent.
    pub fn update_data_attrs(&mut self, parent: ShapeHandle, key: PropKey, attrs: Attrs) -> ShapeHandle {
        if let Some(existing) = self.get(parent).transitions.get(key, attrs) {
            return existing;
        }
        let (proto_cell, num_own) = {
            let parent_shape = self.get(parent);
            (parent_shape.proto_cell, parent_shape.num_own)
        };
        let mut list = self.get(parent).descriptors.as_slice().to_vec();
        let mut replaced = false;
        let mut next_num_own = num_own;
        for d in list.iter_mut() {
            if d.key() == key {
                let slot = d.slot().unwrap_or(num_own);
                *d = Descriptor::Data { key, slot, attrs };
                replaced = true;
                break;
            }
        }
        if !replaced {
            list.push(Descriptor::Data {
                key,
                slot: num_own,
                attrs,
            });
            next_num_own = num_own + 1;
        }
        let mut descriptors = crate::shape::Descriptors::default();
        for d in list {
            descriptors.push(d);
        }
        let child_handle = self.alloc(Shape {
            parent: Some(parent),
            transitions: Transitions::default(),
            descriptors,
            proto_cell,
            num_own: next_num_own,
        });
        self.get_mut(parent).transitions.insert(key, attrs, child_handle);
        child_handle
    }
```

- [ ] **Step 3: Add a unit test for the reconfiguration**

At the bottom of `crates/v12-heap/src/gc.rs`, inside the existing `#[cfg(test)]` module (append this test):

```rust
    #[test]
    fn update_data_attrs_replaces_without_appending() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let key = heap.intern_text("x");
        let root = heap.root_shape();
        let with_x = heap.add_property(root, key, Attrs::DEFAULT);
        assert_eq!(heap.get(with_x).num_own, 1);
        let reconfigured = heap.update_data_attrs(with_x, key, Attrs::BUILTIN);
        assert_eq!(heap.get(reconfigured).num_own, 1, "no duplicate descriptor");
        let desc = heap.get(reconfigured).descriptors.find(key).expect("present");
        assert_eq!(desc.attrs(), Attrs::BUILTIN);
        // A second call with the same attrs returns the same cached child.
        let again = heap.update_data_attrs(with_x, key, Attrs::BUILTIN);
        assert_eq!(again, reconfigured);
    }
```

If `Heap::new`, `root_shape`, or `intern_text` differ in this module, use the same setup the existing tests in `gc.rs` use; do not invent names.

- [ ] **Step 4: Run the heap tests**

Run: `cargo nextest run -p v12-heap`
Expected: PASS (new test plus existing).

- [ ] **Step 5: Commit**

```bash
git add crates/v12-heap/src/shape.rs crates/v12-heap/src/gc.rs
git commit -m "feat(heap): Attrs::FUNCTION_PROTOTYPE + update_data_attrs transition helper"
```

---

### Task 2: ISA — `DefineMethod = 72`, interp handler, dispatch, mini arm

**Files:**
- Modify: `crates/v12-bytecode/src/opcode.rs` (enum variant, `TryFrom`, `ALL_OPS`)
- Modify: `crates/v12-bytecode/src/lib.rs` (`mnemonic`, `fmt_operands`)
- Modify: `crates/v12-bytecode/tests/common/mod.rs` (`KNOWN_DISCRIMINANTS`, `EXPECTED_OPCODE_COUNT`)
- Modify: `crates/v12-interp/src/object_ops.rs` (`op_define_method`)
- Modify: `crates/v12-interp/src/execute.rs` (dispatch arm)
- Modify: `crates/test-support/src/mini.rs` (exhaustive match arm)

**Interfaces:**
- Consumes: `Instr::new(op,a,b,c)`, `Heap::add_property`, `Heap::update_data_attrs` (Task 1), `Interp::property_key`, `Interp::shape_of`, `Interp::bind_shape`, `Interp::gc_protect`, `Interp::global_slot_index`, `child_slot`.
- Produces:
  - `Opcode::DefineMethod = 72`; operands `a` = destination object register, `b` = property key register, `c` = value register; no immediate.
  - `Interp::op_define_method(&mut self, obj_v: JsValue, key_v: JsValue, value_v: JsValue) -> Result<(), JSException>` — defines an own property with `Attrs::BUILTIN`; never walks the prototype chain.

- [ ] **Step 1: Add the opcode variant**

In `crates/v12-bytecode/src/opcode.rs`, after `IteratorClose = 71,` (line 131):

```rust
    /// Defines an own method property: `r{a}[r{b}] = r{c}` with spec method
    /// attributes (writable + configurable, non-enumerable). Unlike
    /// `SetProperty` this never walks the prototype chain and never invokes a
    /// setter.
    DefineMethod = 72,
```

In the `TryFrom<u8> for Opcode` impl, after the `71 => Ok(Self::IteratorClose),` arm (line 205):

```rust
            72 => Ok(Self::DefineMethod),
```

In `#[cfg(test)] mod encoding_tests`, append to `ALL_OPS` after `Opcode::IteratorClose,`:

```rust
        Opcode::DefineMethod,
```

- [ ] **Step 2: Add mnemonic and operand formatting**

In `crates/v12-bytecode/src/lib.rs`, in `mnemonic` after the `IteratorClose => "iterator_close",` arm:

```rust
        Opcode::DefineMethod => "define_method",
```

In `fmt_operands`, add next to the `SetProperty` arm:

```rust
        Opcode::DefineMethod => write!(f, " r{a}, r{b}, r{c}"),
```

- [ ] **Step 3: Update the discriminant sweep table**

In `crates/v12-bytecode/tests/common/mod.rs`:

```rust
pub const KNOWN_DISCRIMINANTS: &[u8] = &[
    1, 2, 3, 4, // Move, LoadConst, LoadInt, Wide
    10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, // Add .. BitNot
    24, 25, 26, 27, 28, 29, 30, 31, 32, 33, // Eq .. TypeOf
    34, 35, 36, 37, // Jump, JumpIfFalse, JumpIfTrue, LoopHeader
    38, 39, 40, // Call, Return, Throw
    41, 42, 43, 44, 45, 46, 47, 48, 49, // GetProperty .. SetEnvSlot
    50, 51, 52, // CreateGenerator, SuspendYield, Await
    53, 54, // In, InstanceOf
    55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68,
    // CopyArrayRest .. Construct, GetNewTarget, ToNumber, MergeObject, DefineAccessor, JumpIfNullish, SetPrototype
    69, 70, 71, // GetIterator, IteratorNext, IteratorClose
    72, // DefineMethod
];

pub const EXPECTED_OPCODE_COUNT: usize = 67;
```

- [ ] **Step 4: Write the interp handler**

In `crates/v12-interp/src/object_ops.rs`, after `op_define_accessor` (ends line 222):

```rust
    /// `DefineMethod`: defines an own data property on `obj` at `key` with
    /// spec method attributes. Never walks the prototype chain, never invokes
    /// a setter. If the key already exists as own data, the value is
    /// overwritten and attrs are re-stamped to `BUILTIN` (later duplicate wins).
    pub(crate) fn op_define_method(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        value_v: JsValue,
    ) -> Result<(), JSException> {
        let Some(obj) = obj_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: cannot define method on non-object"),
            ));
        };
        let key = self.property_key(key_v)?;
        self.gc_protect();
        let shape = self.shape_of(obj);
        match self.heap.get(shape).descriptors.find(key).copied() {
            Some(existing @ v12_heap::Descriptor::Data { slot, .. }) => {
                let idx = self.global_slot_index(obj, slot as usize);
                let props = &mut self.heap.get_mut(obj).properties;
                if props.len() <= idx {
                    props.resize(idx + 1, JsValue::hole());
                }
                props[idx] = value_v;
                if existing.attrs() != Attrs::BUILTIN {
                    let child = self.heap.update_data_attrs(shape, key, Attrs::BUILTIN);
                    self.bind_shape(obj, child);
                }
            }
            Some(v12_heap::Descriptor::Accessor { .. }) => {
                // A duplicate accessor/method name in one class body is a
                // SyntaxError, so this is unreachable from compiled classes.
                return Err(JSException(
                    self.error_value("TypeError: cannot redefine accessor as method"),
                ));
            }
            None => {
                let child = self.heap.add_property(shape, key, Attrs::BUILTIN);
                self.bind_shape(obj, child);
                let settings = &mut self.heap.get_mut(obj);
                settings.properties.push(value_v);
                settings.property_keys.push(Some(key));
            }
        }
        Ok(())
    }
```

- [ ] **Step 5: Add the dispatch arm**

In `crates/v12-interp/src/execute.rs`, immediately after the `Opcode::SetProperty` arm (ends line 631):

```rust
                Opcode::DefineMethod => {
                    let obj_v = self.stack[base + usize::from(ra)];
                    let key_v = self.stack[base + usize::from(rb)];
                    let value = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    attempt!(self.op_define_method(obj_v, key_v, value));
                    self.set_pc(pc + op_width);
                }
```

- [ ] **Step 6: Add the mini-interpreter arm**

In `crates/test-support/src/mini.rs`, in the panic match group add `DefineMethod`:

```rust
            Opcode::CreateGenerator
            | Opcode::SuspendYield
            | Opcode::Await
            | Opcode::CopyArrayRest
            | Opcode::CheckIsArray
            | Opcode::CallApply
            | Opcode::CopyObjectRest
            | Opcode::ArrayAppend
            | Opcode::MergeObject
            | Opcode::DefineAccessor
            | Opcode::DefineMethod
            | Opcode::GetIterator
            | Opcode::IteratorNext
            | Opcode::IteratorClose
            | Opcode::GetNewTarget => {
                panic!("generator/async/copy/define opcodes not expected in tier-1 mini programs")
            }
```

- [ ] **Step 7: Run the bytecode and interp gates**

Run: `cargo nextest run -p v12-bytecode -p v12-interp -p test-support`
Expected: PASS. The exhaustive sweeps in `decode_sweep.rs` now cover discriminant 72; `known_discriminants_exactly_match_opcode_enum` passes with count 67.

- [ ] **Step 8: Commit**

```bash
git add crates/v12-bytecode/src/opcode.rs crates/v12-bytecode/src/lib.rs crates/v12-bytecode/tests/common/mod.rs crates/v12-interp/src/object_ops.rs crates/v12-interp/src/execute.rs crates/test-support/src/mini.rs
git commit -m "feat(bytecode,interp): DefineMethod opcode 72 + BUILTIN-attr own-property handler"
```

---

### Task 3: Interp attribute fixes — accessor attrs, function length/prototype/constructor

**Files:**
- Modify: `crates/v12-interp/src/object_ops.rs` (`op_define_accessor` attrs, line 213)
- Modify: `crates/v12-interp/src/property.rs` (add `define_own_data_attrs`)
- Modify: `crates/v12-interp/src/lib.rs` (`install_function_length`, `materialize_function_prototype`)

**Interfaces:**
- Consumes: `Attrs::BUILTIN`, `Attrs::FUNCTION_PROTOTYPE` (Task 1), `Heap::add_property`, `child_slot`.
- Produces: `Interp::define_own_data_attrs(&mut self, obj_v: JsValue, key_v: JsValue, value: JsValue, attrs: Attrs) -> Result<(), JSException>`.

- [ ] **Step 1: Change the accessor install attrs**

In `crates/v12-interp/src/object_ops.rs`, line 213, change:

```rust
            .define_accessor(shape, key, getter, setter, Attrs::DEFAULT);
```

to:

```rust
            .define_accessor(shape, key, getter, setter, Attrs::BUILTIN);
```

- [ ] **Step 2: Add the explicit-attrs own-data helper**

In `crates/v12-interp/src/property.rs`, after `set_property` (ends line 779):

```rust
    /// Defines an own data property with explicit attributes, bypassing the
    /// setter/prototype walk of [`Self::set_property`]. Used for fresh
    /// function-intrinsic properties (`length`, `prototype`, `constructor`)
    /// whose attributes the spec fixes.
    pub(crate) fn define_own_data_attrs(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        value: JsValue,
        attrs: Attrs,
    ) -> Result<(), JSException> {
        let Some(obj) = obj_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: cannot define property on non-object"),
            ));
        };
        let key = self.property_key(key_v)?;
        self.gc_protect();
        let shape = self.shape_of(obj);
        let child = self.heap.add_property(shape, key, attrs);
        self.bind_shape(obj, child);
        let slot = child_slot(self.heap, child) as usize;
        let settings = &mut self.heap.get_mut(obj);
        if settings.properties.len() <= slot {
            settings.properties.resize(slot + 1, JsValue::hole());
        }
        settings.properties[slot] = value;
        if settings.property_keys.len() <= slot {
            settings.property_keys.resize(slot + 1, None);
        }
        settings.property_keys[slot] = Some(key);
        Ok(())
    }
```

Confirm `Attrs` and `child_slot` are in scope in `property.rs` (they are used by `set_property`); if `Attrs` is not imported, add it to the existing `v12_heap::{...}` import.

- [ ] **Step 3: Fix `install_function_length`**

In `crates/v12-interp/src/lib.rs`, replace the body of `install_function_length` (lines 698-710) with:

```rust
    fn install_function_length(&mut self, h: Handle<JsObject>, expected: u16) {
        let len_v = JsValue::from_i32_smi(i32::from(expected)).expect("param count fits Smi");
        let frame = self.stack.len();
        self.stack.push(JsValue::object(h));
        self.gc_protect();
        let key = JsValue::string(self.heap.intern_text("length"));
        let installed = self.define_own_data_attrs(
            JsValue::object(h),
            key,
            len_v,
            Attrs::new(false, false, true),
        );
        debug_assert!(installed.is_ok(), "length install cannot fail");
        self.stack.truncate(frame);
    }
```

- [ ] **Step 4: Fix `materialize_function_prototype`**

In `crates/v12-interp/src/lib.rs`, replace the two `set_property` calls at lines 686-688 with explicit-attr defines:

```rust
        let ctor_key = JsValue::string(self.heap.intern_text("constructor"));
        let proto_key_v = JsValue::string(self.heap.intern_text("prototype"));
        let result = self
            .define_own_data_attrs(proto_v, ctor_key, f_v, Attrs::BUILTIN)
            .and_then(|()| {
                self.define_own_data_attrs(
                    f_v,
                    proto_key_v,
                    proto_v,
                    Attrs::FUNCTION_PROTOTYPE,
                )
            });
```

Keep the existing `gc_protect`, the proto/f stack push and truncate, and the idempotent early return unchanged.

- [ ] **Step 5: Verify object-literal accessors and function attrs**

Write `/tmp/a3_accessor.js`:

```js
var o = { get x() { return 1; }, set x(v) {} };
var d = Object.getOwnPropertyDescriptor(o, "x");
console.log(d.enumerable, d.configurable, typeof d.get, typeof d.set);
function f(a, b) {}
var fl = Object.getOwnPropertyDescriptor(f, "length");
var fp = Object.getOwnPropertyDescriptor(f, "prototype");
var fc = Object.getOwnPropertyDescriptor(f.prototype, "constructor");
console.log(fl.writable, fl.enumerable, fl.configurable, fl.value);
console.log(fp.writable, fp.enumerable, fp.configurable, typeof fp.value);
console.log(fc.writable, fc.enumerable, fc.configurable);
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a3_accessor.js`
Expected: `false true function function` / `false false true 2` / `true false false object` / `true false true`.

- [ ] **Step 6: Run the gate**

Run: `cargo nextest run --workspace`
Expected: PASS (576+). Object-literal accessor tests may assert enumerable=true; if any fail, they encode the old wrong behavior — update them to the spec value and note it in the commit message.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-interp/src/object_ops.rs crates/v12-interp/src/property.rs crates/v12-interp/src/lib.rs
git commit -m "fix(interp): spec attrs for accessors, function length/prototype/constructor"
```

---

### Task 4: Class installs use `DefineMethod`

**Files:**
- Modify: `crates/v12-bccompiler/src/class.rs` (lines 73, 75, 159)

**Interfaces:**
- Consumes: `Opcode::DefineMethod` (Task 2).
- Produces: class `prototype`/`constructor` links and instance/static methods installed with `BUILTIN` attrs.

- [ ] **Step 1: Change the ctor/proto links**

In `crates/v12-bccompiler/src/class.rs`, lines 73 and 75, replace both `SetProperty` emits:

```rust
    cx.emit_reg3(Opcode::DefineMethod, ctor, proto_key, proto, span);
    cx.emit_reg3(Opcode::DefineMethod, proto, ctor_key, ctor, span);
```

- [ ] **Step 2: Change the method install**

In `define_elements`, the non-ctor method arm (line 159), replace:

```rust
            cx.emit_reg3(Opcode::SetProperty, target, key_reg, fn_reg, m.span);
```

with:

```rust
            cx.emit_reg3(Opcode::DefineMethod, target, key_reg, fn_reg, m.span);
```

- [ ] **Step 3: Verify class descriptor attrs**

Write `/tmp/a3_class.js`:

```js
class C { m() {} static s() {} get g() { return 1; } set g(v) {} }
var md = Object.getOwnPropertyDescriptor(C.prototype, "m");
var sd = Object.getOwnPropertyDescriptor(C, "s");
var gd = Object.getOwnPropertyDescriptor(C.prototype, "g");
var cd = Object.getOwnPropertyDescriptor(C.prototype, "constructor");
console.log(md.enumerable, md.writable, md.configurable);
console.log(sd.enumerable, sd.writable, sd.configurable);
console.log(gd.enumerable, gd.configurable, typeof gd.get, typeof gd.set);
console.log(cd.enumerable, cd.writable, cd.configurable);
console.log(C.prototype.constructor === C);
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a3_class.js`
Expected: `false true true` / `false true true` / `false true function function` / `false true true` / `true`.

- [ ] **Step 4: Run the gate**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/v12-bccompiler/src/class.rs
git commit -m "fix(bccompiler): class methods/prototype/constructor installed via DefineMethod"
```

---

### Task 5: `Object.prototype.propertyIsEnumerable`

**Files:**
- Modify: `crates/v12-native/src/id.rs` (add variant `= 1024`)
- Modify: `crates/v12-engine/src/builtins/mod.rs` (`builtin_length` arm + `ObjectProto` table entry)
- Modify: `crates/v12-engine/src/builtins/object.rs` (handler)
- Modify: `crates/v12-engine/src/builtins/boolean.rs` (make `to_boolean` reachable)

**Interfaces:**
- Consumes: `NativeId` (strum `FromRepr`), `Ctx::type_error`, `property_key(ctx, v)`, `Heap::lookup_property`, `Descriptor::is_data()/slot()/attrs()`.
- Produces: `NativeId::ObjectProtoPropertyIsEnumerable = 1024`; `object::object_proto_property_is_enumerable`.

- [ ] **Step 1: Add the native id**

In `crates/v12-native/src/id.rs`, after `ObjectGetOwnPropertySymbols = 1023,`:

```rust
    /// `Object.prototype.propertyIsEnumerable(key)`.
    ObjectProtoPropertyIsEnumerable = 1024,
```

- [ ] **Step 2: Make the truthiness helper reachable**

In `crates/v12-engine/src/builtins/boolean.rs`, line 23, change `fn to_boolean(ctx: &Ctx, v: JsValue) -> bool` to `pub(crate) fn to_boolean(ctx: &Ctx, v: JsValue) -> bool`. Its signature is unchanged; it already handles the full ToBoolean table.

- [ ] **Step 3: Write the handler**

In `crates/v12-engine/src/builtins/object.rs`, after `object_has_own_property`:

```rust
/// `Object.prototype.propertyIsEnumerable(key)` – whether `key` is an own
/// enumerable data property of `this`.
pub fn object_proto_property_is_enumerable(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = this.as_object().ok_or_else(|| {
        ctx.type_error("TypeError: Object.prototype.propertyIsEnumerable called on non-object")
    })?;
    let key_v = args.first().copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v)?;
    let shape = ctx.heap.shape_of(obj);
    let enumerable = match ctx.heap.lookup_property(shape, pk) {
        Some(desc) if desc.is_data() => {
            let populated = ctx
                .heap
                .get(obj)
                .properties
                .get(desc.slot() as usize)
                .is_some_and(|v| !v.is_hole());
            populated && desc.attrs().enumerable()
        }
        Some(desc) => desc.attrs().enumerable(),
        None => false,
    };
    Ok(JsValue::from_bool(enumerable))
}
```

- [ ] **Step 4: Add the builtin length**

In `crates/v12-engine/src/builtins/mod.rs`, in `builtin_length`, add:

```rust
        NativeId::ObjectProtoPropertyIsEnumerable => Some(1),
```

- [ ] **Step 5: Register in the ObjectProto table**

In `crates/v12-engine/src/builtins/mod.rs`, in the `ObjectProto { ... }` block (line 662-666), add:

```rust
            "propertyIsEnumerable" (1) => ObjectProtoPropertyIsEnumerable => |heap, this, args| call_ctx(object::object_proto_property_is_enumerable, heap, this, args),
```

- [ ] **Step 6: Verify**

Write `/tmp/a3_pie.js`:

```js
console.log(Object.prototype.propertyIsEnumerable.call({ a: 1 }, "a"));
console.log(Object.prototype.propertyIsEnumerable.call({ a: 1 }, "b"));
console.log(Object.prototype.propertyIsEnumerable.call([1], "0"));
class C { m() {} }
console.log(Object.prototype.propertyIsEnumerable.call(C.prototype, "m"));
console.log(Object.prototype.propertyIsEnumerable.call(C.prototype, "constructor"));
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a3_pie.js`
Expected: `true` / `false` / `true` / `false` / `false`.

- [ ] **Step 7: Run the gate**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/v12-native/src/id.rs crates/v12-engine/src/builtins/mod.rs crates/v12-engine/src/builtins/object.rs crates/v12-engine/src/builtins/boolean.rs
git commit -m "feat(engine): Object.prototype.propertyIsEnumerable"
```

---

### Task 6: `delete` semantics — holed data descriptors are absent

**Files:**
- Modify: `crates/v12-engine/src/builtins/object.rs` (liveness predicate; `object_has_own_property`, `object_has_own`, `object_get_own_property_descriptor`)
- Modify: `crates/v12-interp/src/property.rs` (`delete_property` comment/assertion only — behavior already correct)

**Interfaces:**
- Consumes: `Descriptor::Data { slot }`, `JsValue::is_hole()`.
- Produces: `fn descriptor_is_live(heap: &Heap, obj: Handle<JsObject>, desc: &v12_heap::Descriptor) -> bool`.

- [ ] **Step 1: Add the shared liveness predicate**

In `crates/v12-engine/src/builtins/object.rs`, before `object_has_own_property`:

```rust
/// Whether `desc` is a live own property of `obj`. `delete` stores `hole` in a
/// data property's slot while leaving the shared shape descriptor in place, so
/// a holed data descriptor is not observable. Accessors have no slot and are
/// always live.
fn descriptor_is_live(heap: &Heap, obj: Handle<JsObject>, desc: &v12_heap::Descriptor) -> bool {
    match desc {
        v12_heap::Descriptor::Data { slot, .. } => heap
            .get(obj)
            .properties
            .get(*slot as usize)
            .is_some_and(|v| !v.is_hole()),
        v12_heap::Descriptor::Accessor { .. } => true,
    }
}
```

Confirm `Heap` is imported in `object.rs`; if only `Ctx` wraps it, import `v12_heap::Heap`.

- [ ] **Step 2: Filter `hasOwnProperty`**

In `object_has_own_property`, replace the `found` computation:

```rust
    let shape = ctx.heap.shape_of(this_obj);
    let found = ctx
        .heap
        .lookup_property(shape, pk)
        .is_some_and(|d| descriptor_is_live(ctx.heap, this_obj, d));
```

- [ ] **Step 3: Filter `Object.hasOwn`**

In `object_has_own`, replace the final expression:

```rust
    let shape = ctx.heap.shape_of(obj);
    Ok(JsValue::from_bool(
        ctx.heap
            .lookup_property(shape, pk)
            .is_some_and(|d| descriptor_is_live(ctx.heap, obj, d)),
    ))
```

- [ ] **Step 4: Filter `getOwnPropertyDescriptor`**

In `object_get_own_property_descriptor`, change the descriptor lookup so a holed data descriptor yields `undefined`:

```rust
    let Some(desc) = ctx
        .heap
        .lookup_property(shape, pk)
        .filter(|d| descriptor_is_live(ctx.heap, obj, d))
    else {
        return Ok(JsValue::undefined());
    };
```

Adapt to the existing binding shape (the function currently binds `desc` then matches). If it currently does `let desc = ... else { return undefined }`, add `.filter(...)` to that expression and keep the rest unchanged.

- [ ] **Step 5: Verify**

Write `/tmp/a3_delete.js`:

```js
class C { m() {} }
delete C.prototype.m;
console.log(C.prototype.hasOwnProperty("m"));
console.log(Object.prototype.propertyIsEnumerable.call(C.prototype, "m"));
console.log(Object.getOwnPropertyDescriptor(C.prototype, "m") === undefined);
var o = { a: 1 };
delete o.a;
console.log(o.hasOwnProperty("a"), Object.hasOwn(o, "a"), Object.getOwnPropertyDescriptor(o, "a") === undefined);
class D { n() {} }
console.log(D.prototype.hasOwnProperty("n"));
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a3_delete.js`
Expected: `false` / `false` / `true` / `false false true` / `true`.

- [ ] **Step 6: Run the gate**

Run: `cargo nextest run --workspace`
Expected: PASS. Then run the class/elements filter to measure the collapse:

Run: `./conformance/run.sh --filter language/expressions/class/elements --jobs 8`
Expected: pass count rises well above the 413 baseline (the 560× descriptor message collapses). Record the number for Task 9.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/object.rs crates/v12-interp/src/property.rs
git commit -m "fix(engine): deleted (holed) data properties are absent from own-property queries"
```

---

### Task 7: `Object.defineProperty` descriptor flags

**Files:**
- Modify: `crates/v12-engine/src/builtins/object.rs` (`object_define_property`, new `parse_data_descriptor`)
- Modify: `crates/v12-engine/src/internal_methods.rs` (`ordinary_define_own_property`)

**Interfaces:**
- Consumes: `PropertyDescriptor`, `Heap::update_data_attrs` (Task 1), `Ctx::to_boolean` (Task 5), `property_key`.
- Produces: `fn parse_data_descriptor(ctx: &mut Ctx, v: JsValue) -> Result<PropertyDescriptor, Throw>`.

- [ ] **Step 1: Write the descriptor parser**

In `crates/v12-engine/src/builtins/object.rs`, before `object_define_property`:

```rust
/// Minimal `ToPropertyDescriptor` for data descriptors: reads `value`,
/// `writable`, `enumerable`, `configurable` with spec-default `false` for
/// absent flags. `PropertyDescriptor::default()` is all-`true`, so it is
/// deliberately not used here.
fn parse_data_descriptor(ctx: &mut Ctx, v: JsValue) -> Result<PropertyDescriptor, Throw> {
    let mut desc = PropertyDescriptor {
        value: None,
        writable: false,
        enumerable: false,
        configurable: false,
    };
    let Some(obj) = v.as_object() else {
        return Err(ctx.type_error("TypeError: Property description must be an object"));
    };
    for name in ["value", "writable", "enumerable", "configurable"] {
        let key = ctx.heap.intern_text(name);
        let present = {
            let shape = ctx.heap.shape_of(obj);
            ctx.heap
                .lookup_property(shape, PropKey::String(key))
                .is_some()
        };
        if !present {
            continue;
        }
        let got = ctx.get_prop(JsValue::object(obj), name)?;
        match name {
            "value" => desc.value = Some(got),
            "writable" => desc.writable = super::boolean::to_boolean(ctx, got),
            "enumerable" => desc.enumerable = super::boolean::to_boolean(ctx, got),
            "configurable" => desc.configurable = super::boolean::to_boolean(ctx, got),
            _ => unreachable!(),
        }
    }
    Ok(desc)
}
```

`PropKey::String` takes a `Spur`; confirm the variant name in `crates/v12-heap/src/prop_key.rs` and use the interned handle. If `Ctx` has no `get_prop`, use the same property-read path `object.rs` uses elsewhere (e.g. `ctx.heap` + shape lookup) or add a two-line helper; do not invent a method that does not exist.

- [ ] **Step 2: Use it in `object_define_property`**

Replace the `value` extraction and the `ordinary_define_own_property` call (lines 65-74):

```rust
    let descriptor = if args.len() >= 3 {
        parse_data_descriptor(ctx, args[2])?
    } else {
        PropertyDescriptor {
            value: Some(JsValue::undefined()),
            writable: false,
            enumerable: false,
            configurable: false,
        }
    };
    crate::internal_methods::ordinary_define_own_property(&mut *ctx.heap, obj, key, descriptor)?;
    Ok(JsValue::object(obj))
```

- [ ] **Step 3: Honor flags in `ordinary_define_own_property`**

In `crates/v12-engine/src/internal_methods.rs`, in the existing-descriptor `Descriptor::Data` branch, replace the current body with:

```rust
            Descriptor::Data { slot, attrs, .. } => {
                let slot = *slot as usize;
                if let Some(v) = descriptor.value {
                    if !attrs.writable() {
                        return Ok(false);
                    }
                    let obj_mut = heap.get_mut(obj);
                    if obj_mut.properties.len() <= slot {
                        obj_mut.properties.resize(slot + 1, JsValue::hole());
                    }
                    obj_mut.properties[slot] = v;
                }
                let new_attrs = Attrs::new(
                    descriptor.writable,
                    descriptor.enumerable,
                    descriptor.configurable,
                );
                if new_attrs != *attrs {
                    let next_shape = heap.update_data_attrs(shape, key, new_attrs);
                    heap.bind_shape(obj, next_shape);
                }
                return Ok(true);
            }
```

Confirm `heap.bind_shape` exists (interp has it; the heap has `bind_shape` per the recon). If the engine's `Heap` exposes shape binding under a different name, use that name.

- [ ] **Step 4: Honor flags on new-property creation**

In the new-property arm (line 210), replace:

```rust
        let next_shape = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
```

with:

```rust
        let next_shape = heap.add_property(
            shape,
            key,
            Attrs::new(
                descriptor.writable,
                descriptor.enumerable,
                descriptor.configurable,
            ),
        );
```

- [ ] **Step 5: Verify**

Write `/tmp/a3_defprop.js`:

```js
var o = {};
Object.defineProperty(o, "x", { value: 1, enumerable: false });
var d = Object.getOwnPropertyDescriptor(o, "x");
console.log(d.value, d.writable, d.enumerable, d.configurable);
var p = {};
Object.defineProperty(p, "y", { value: 2, writable: true, enumerable: true, configurable: true });
var e = Object.getOwnPropertyDescriptor(p, "y");
console.log(e.value, e.writable, e.enumerable, e.configurable);
Object.defineProperty(o, "x", { value: 9 });
console.log(o.x, Object.getOwnPropertyDescriptor(o, "x").writable);
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a3_defprop.js`
Expected: `1 false false false` / `2 true true true` / `9 false`.

- [ ] **Step 6: Run the gate**

Run: `cargo nextest run --workspace`
Expected: PASS. The `built-ins/Object/getOwnPropertyDescriptor` filter should rise from 136/328.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-engine/src/builtins/object.rs crates/v12-engine/src/internal_methods.rs
git commit -m "feat(engine): Object.defineProperty honors descriptor flags"
```

---

### Task 8: Instance fields initialize on `this` in the constructor

**Files:**
- Modify: `crates/v12-bccompiler/src/unit.rs` (`UnitNode::Class` arm)
- Modify: `crates/v12-bccompiler/src/class.rs` (make `property_key_reg` `pub(crate)`; drop the instance-field install branch)
- Test: `crates/v12-bccompiler/src/tests.rs`

**Interfaces:**
- Consumes: `crate::model::REG_THIS`, `FnCtx::expr`, `Opcode::SetProperty`, `class::property_key_reg`.
- Produces: public instance fields emitted as `this.<key> = <value>` at the top of the constructor body for base classes.

- [ ] **Step 1: Expose the key helper**

In `crates/v12-bccompiler/src/class.rs`, change:

```rust
fn property_key_reg(
```

to:

```rust
pub(crate) fn property_key_reg(
```

- [ ] **Step 2: Drop the prototype-target instance-field install**

In `define_elements`, the `PropertyDefinition` non-private branch, replace the target selection and emit with a static-only install:

```rust
            let target = if p.r#static { ctor } else { continue };
            let key_reg = property_key_reg(cx, &p.key, p.computed, p.span)?;
            let value_reg = if let Some(v) = &p.value {
                cx.expr(v)?
            } else {
                let d = cx.new_temp();
                cx.load_undefined(d, p.span);
                d
            };
            cx.emit_reg3(Opcode::SetProperty, target, key_reg, value_reg, p.span);
```

(Private fields keep their existing `DefinePrivateW` path above; only the public non-static branch changes.)

- [ ] **Step 3: Emit instance fields in the constructor unit**

In `crates/v12-bccompiler/src/unit.rs`, replace the `UnitNode::Class(c)` arm with:

```rust
        UnitNode::Class(c) => {
            // Base-class instance fields initialize on `this` at the top of the
            // constructor, before the body. Derived classes must wait until
            // after `super()`; that ordering is not modeled yet, so derived
            // fields are skipped rather than initialized too early.
            if c.heritage.is_none() {
                for el in &c.body.body {
                    let oxc_ast::ast::ClassElement::PropertyDefinition(p) = el else {
                        continue;
                    };
                    if p.r#static {
                        continue;
                    }
                    let Some(value) = &p.value else {
                        continue;
                    };
                    let value_reg = cx.expr(value)?;
                    let key_reg =
                        crate::class::property_key_reg(&mut cx, &p.key, p.computed, p.span)?;
                    cx.emit_reg3(
                        Opcode::SetProperty,
                        crate::model::REG_THIS,
                        key_reg,
                        value_reg,
                        p.span,
                    );
                }
            }
            let ctor = c.body.body.iter().find_map(|el| match el {
                oxc_ast::ast::ClassElement::MethodDefinition(m)
                    if m.kind == MethodDefinitionKind::Constructor =>
                {
                    Some(m)
                }
                _ => None,
            });
            if let Some(m) = ctor {
                let Some(body) = m.value.body.as_deref() else {
                    return Err(cx.err(m.span, "constructor without a body is not supported"));
                };
                cx.stmt_list(&body.statements)?;
            }
            // Default constructor: field initializers above, then `return undefined`.
        }
```

If `cx.expr` needs `&mut *cx` to avoid a double-borrow with `property_key_reg`, bind the value first (as written) — `cx.expr(value)?` completes before `property_key_reg` re-borrows.

- [ ] **Step 4: Add a compiler test**

In `crates/v12-bccompiler/src/tests.rs`, add:

```rust
#[test]
fn base_class_instance_field_initializes_on_this() {
    let (prog, _strings) =
        compile_source_with_strings("class C { a = 1; b; } var c = new C();").expect("compiles");
    for f in &prog.functions {
        f.validate().expect("valid bytecode");
    }
}
```

- [ ] **Step 5: Verify**

Write `/tmp/a3_fields.js`:

```js
class C { a = 1; b = this.a + 1; m() { return this.a; } }
var c = new C();
console.log(C.prototype.hasOwnProperty("a"));
console.log(c.hasOwnProperty("a"), c.a, c.b, c.m());
var o = { n: 5 };
class D { x = o.n * 2; }
console.log(new D().x);
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a3_fields.js`
Expected: `false` / `true 1 2 1` / `10`.

- [ ] **Step 6: Run the gate**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/v12-bccompiler/src/unit.rs crates/v12-bccompiler/src/class.rs crates/v12-bccompiler/src/tests.rs
git commit -m "fix(bccompiler): instance fields initialize on this in the constructor"
```

---

### Task 9: `Function.name` for methods and classes

**Files:**
- Modify: `crates/v12-bytecode/src/lib.rs` (`FunctionBytecode.function_name`)
- Modify: `crates/v12-bccompiler/src/model.rs` (`UnitPlan.function_name`)
- Modify: `crates/v12-bccompiler/src/collect.rs` (populate for methods and ctor)
- Modify: `crates/v12-bccompiler/src/unit.rs` (`fb.function_name`)
- Modify: `crates/v12-interp/src/lib.rs` (`alloc_closure` installs name)

**Interfaces:**
- Consumes: `static_key_text`, `UnitPlan::new`, `Interp::define_own_data_attrs` (Task 3), `Attrs::new(false,false,true)`.
- Produces: `FunctionBytecode.function_name: Option<String>`; `UnitPlan.function_name: Option<String>`.

- [ ] **Step 1: Add the bytecode field**

In `crates/v12-bytecode/src/lib.rs`, in `FunctionBytecode`, after `pub name_hint: Option<String>,`:

```rust
    /// The spec `SetFunctionName` value for this closure, when statically
    /// known (methods, class constructors). `None` for anonymous/arrow fns.
    pub function_name: Option<String>,
```

Find the `FunctionBytecode` constructor(s)/`Default` impl and initialize `function_name: None` everywhere `name_hint` is initialized.

- [ ] **Step 2: Add the plan field**

In `crates/v12-bccompiler/src/model.rs`, in `UnitPlan`, after `name_hint: String,`:

```rust
    /// Statically-known `SetFunctionName` value for the unit's closure.
    pub function_name: Option<String>,
```

Initialize `function_name: None` in `UnitPlan::new`.

- [ ] **Step 3: Populate for methods**

In `crates/v12-bccompiler/src/collect.rs`, at the method-loop `UnitPlan::new` call (~line 466), set the name after construction:

```rust
            let mut mplan = UnitPlan::new(
                Some(parent),
                false,
                format!("<method>{}", Self::static_key_or_default(&m.key)),
            );
            let prefix = match m.kind {
                oxc_ast::ast::MethodDefinitionKind::Get => "get ",
                oxc_ast::ast::MethodDefinitionKind::Set => "set ",
                _ => "",
            };
            mplan.function_name = crate::expr::static_key_text(&m.key)
                .map(|k| format!("{prefix}{k}"));
```

- [ ] **Step 4: Populate for the constructor**

In the ctor-collection block (~line 427-442), after acquiring the ctor plan (the `UnitPlan` for `idx`), set its name from the class id. Add immediately after `register_formals`:

```rust
        if let Some(id) = &c.id {
            self.plans.units[idx].function_name = Some(id.name.to_string());
        }
```

- [ ] **Step 5: Thread it into the finish step**

In `crates/v12-bccompiler/src/unit.rs`, after `fb.name_hint = Some(comp.plans.units[idx].name_hint.clone());`:

```rust
    fb.function_name = comp.plans.units[idx].function_name.clone();
```

- [ ] **Step 6: Install in `alloc_closure`**

In `crates/v12-interp/src/lib.rs`, in `alloc_closure`, after `install_function_length`, read and install the name. The `funcs.get(fn_idx)` tuple currently destructures `(is_arrow, expected_args)`; extend the read to also obtain `function_name` (borrow it, clone the `Option<String>`), then:

```rust
        if let Some(name) = function_name {
            let frame = self.stack.len();
            self.stack.push(JsValue::object(h));
            self.gc_protect();
            let key = JsValue::string(self.heap.intern_text("name"));
            let installed =
                self.define_own_data_attrs(JsValue::object(h), key, JsValue::string(name_handle), Attrs::new(false, false, true));
            debug_assert!(installed.is_ok(), "name install cannot fail");
            self.stack.truncate(frame);
        }
```

Intern the name to a handle with `self.heap.intern_text(&name)` (deposit into a local before pushing if the borrow checker requires it). Keep the existing `install_function_length` and prototype materialization.

- [ ] **Step 7: Verify**

Write `/tmp/a3_name.js`:

```js
class C { m() {} static s() {} get g() { return 1; } }
console.log(C.prototype.m.name);
console.log(C.s.name);
console.log(Object.getOwnPropertyDescriptor(C.prototype, "g").get.name);
console.log(C.name);
var d = Object.getOwnPropertyDescriptor(C.prototype.m, "name");
console.log(d.writable, d.enumerable, d.configurable);
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a3_name.js`
Expected: `m` / `s` / `get g` / `C` / `false false true`.

- [ ] **Step 8: Run the gate**

Run: `cargo nextest run --workspace`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/v12-bytecode/src/lib.rs crates/v12-bccompiler/src/model.rs crates/v12-bccompiler/src/collect.rs crates/v12-bccompiler/src/unit.rs crates/v12-interp/src/lib.rs
git commit -m "feat(bccompiler,interp): SetFunctionName for class methods and constructors"
```

---

### Task 10: Re-score and record

**Files:**
- Modify: `conformance/fix-log.md` (new entry at the top of §Entries)

**Interfaces:**
- Consumes: all prior tasks.
- Produces: a fix-log entry with before/after numbers.

- [ ] **Step 1: Run the full workspace gate**

Run: `cargo nextest run --workspace`
Expected: PASS, count ≥ 576. Record the exact count.

- [ ] **Step 2: Re-score the target suites**

Run each and record the pass/total:

```
./conformance/run.sh --filter language/expressions/class/elements --jobs 8
./conformance/run.sh --filter language/expressions/object/method-definition --jobs 8
./conformance/run.sh --filter built-ins/Object/getOwnPropertyDescriptor --jobs 8
./conformance/run.sh --filter language/expressions --jobs 8
```

- [ ] **Step 3: Write the fix-log entry**

At the top of the §Entries section in `conformance/fix-log.md`, add (fill in the measured numbers):

```md
### 2026-09-12 — Step A3: class element attrs, delete semantics, Function.name

- **Filter:** `language/expressions/class/elements`, `language/expressions/object/method-definition`, `built-ins/Object/getOwnPropertyDescriptor`, then `language/expressions` (11 190 files, 8 jobs)
- **Before:** class/elements 413/1428, object/method-definition 155/303, getOwnPropertyDescriptor 136/328, `language/expressions` 5 128/11 190 (45.8 %)
- **After:** class/elements <N>/1428, object/method-definition <N>/303, getOwnPropertyDescriptor <N>/328, `language/expressions` <N>/11 190 (<P> %)
- **Delta:** <fill in>
- **Root cause:** class method/accessor installs lowered to attributeless `SetProperty`/`DefineAccessor` stamping `Attrs::DEFAULT`; `delete` holed the value but left the shared shape descriptor readable; `Object.defineProperty` ignored descriptor flags; instance fields installed on the prototype; no `SetFunctionName`.
- **Fix:** new `DefineMethod = 72` opcode installing own data properties with `Attrs::BUILTIN`; `op_define_accessor` → `BUILTIN`; explicit attrs for function `length`/`prototype`/`constructor`; `descriptor_is_live` filters holed data descriptors from own-property queries; `Object.defineProperty` parses descriptor flags; instance fields initialize on `this` in the constructor; `function_name` threaded to `alloc_closure`.
- **Accepted gaps:** derived-class field ordering after `super()`; static blocks; accessor `defineProperty` (`get`/`set`); computed/symbol method `name`; full holed-descriptor reader sweep.
- **Engine change:** <commit hashes>
- **Files:** `crates/v12-heap/src/{shape,gc}.rs`, `crates/v12-bytecode/src/{opcode,lib}.rs`, `crates/v12-interp/src/{object_ops,execute,property,lib}.rs`, `crates/v12-bccompiler/src/{class,unit,collect,model}.rs`, `crates/v12-engine/src/{internal_methods,builtins/object,builtins/mod,builtins/boolean}.rs`, `crates/v12-native/src/id.rs`, `crates/test-support/src/mini.rs`
- **Bucket:** `known-failures.md` §A3 — closed
- **Runner:** `./conformance/run.sh --filter <f> --jobs 8`
```

- [ ] **Step 4: Commit**

```bash
git add conformance/fix-log.md
git commit -m "docs(conformance): record A3 class element attrs results"
```

---

## Self-Review

**Spec coverage.** D1 is Tasks 2–4 (opcode, handler, attrs, class call sites). D6 is Task 5 (`propertyIsEnumerable`). D2 is Task 6. D4 is Task 7. D3 is Task 8. D5 is Task 9. Sequencing D1+D6 → D2 → D4 → D3 → D5 is preserved: Task 6 lands after Task 5 completes, so the right disjunct of `verifyProperty` sees a live `propertyIsEnumerable`. Deferred items (static blocks, accessor `defineProperty`, computed/symbol name, derived-class field ordering, full D2 reader sweep) are explicitly excluded from every task and listed in the fix-log entry.

**Placeholder scan.** Every code step carries full code. Task 10's fix-log entry intentionally leaves measured numbers as `<N>`/`<P>` because they cannot be known until execution; every other step is concrete. Two engine-reconter steps (Task 7 Step 1's `Ctx::get_prop`/`PropKey::String` names, Task 2 Step 3's heap-test setup) name the exact fallback to use if the assumed helper name differs — they are not "TBD".

**Type consistency.** `update_data_attrs(parent, key, attrs) -> ShapeHandle` is used identically in Tasks 2 and 7. `define_own_data_attrs(obj_v, key_v, value, attrs) -> Result<(), JSException>` is used identically in Tasks 3 and 9. `descriptor_is_live(heap, obj, desc) -> bool` is used identically across the three Task 6 call sites. `Attrs::BUILTIN` and `Attrs::FUNCTION_PROTOTYPE` are defined once in Task 1 and reused. `Opcode::DefineMethod` operands `(a=obj, b=key, c=value)` are consistent between the encoder (Task 4), the handler (Task 2), and the dispatch arm (Task 2).
