//! Destructuring and object-composition opcodes: `CopyArrayRest`/
//! `CopyObjectRest`/`MergeObject`, iterator protocol ops, and array append.

use v12_heap::{Attrs, Handle, JsObject, JsValue, Kind, PropKey};

use super::{accessor_target, child_slot, Interp, JSException};
use crate::ops;

impl Interp<'_> {
    pub(crate) fn op_copy_array_rest(&mut self, src_v: JsValue, start: u16) -> Result<JsValue, JSException> {
        let Some(src_obj) = src_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: cannot destructure non-iterable"),
            ));
        };
        if self.heap.get(src_obj).kind != Kind::Array {
            // For destructuring, non-array iterable is still an error in our subset (only arrays).
            return Err(JSException(
                self.error_value("TypeError: spread/rest source is not an array"),
            ));
        }
        let start_usize = start as usize;
        let src_len = self.heap.get(src_obj).elements_array.len();
        let slice: Vec<JsValue> = if start_usize >= src_len {
            Vec::new()
        } else {
            self.heap
                .get(src_obj)
                .elements_array
                .iter()
                .skip(start_usize)
                .collect()
        };
        self.gc_protect();
        let shape = self.array_shape();
        let h = self.heap.alloc(JsObject::array(slice));
        self.bind_shape(h, shape);
        // Elements may contain holes; they are preserved as hole values.
        Ok(JsValue::object(h))
    }

    pub(crate) fn op_copy_object_rest(
        &mut self,
        src_v: JsValue,
        excl_vals: &[JsValue],
    ) -> Result<JsValue, JSException> {
        if src_v.is_null() || src_v.is_undefined() {
            return Err(JSException(self.error_value(
                "TypeError: cannot destructure 'undefined' or 'null'",
            )));
        }
        let Some(src_obj) = src_v.as_object() else {
            // Primitives in object rest: spec coerces to object, but our subset treats as empty.
            self.gc_protect();
            let h = self.heap.alloc(JsObject::default());
            let shape = self.heap.root_shape();
            self.bind_shape(h, shape);
            return Ok(JsValue::object(h));
        };
        // Collect excluded keys as PropKeys for fast compare.
        // Build a set of handler strings for comparison (using heap string handles if possible).
        // For simplicity, compare via textual equality using strings_equal for string values.
        // Excluded values may be strings, numbers, symbols. Convert via property_key.
        let mut excl_keys: Vec<PropKey> = Vec::with_capacity(excl_vals.len());
        for &v in excl_vals {
            // Numbers and booleans coerce via ToPropertyKey: use property_key which may allocate.
            // For performance, handle string fast path.
            if let Some(h) = v.as_string() {
                // Use the string handle already interned.
                excl_keys.push(PropKey::from_string(h));
                continue;
            }
            if let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) {
                // Numeric key → decimal string.
                let text = ops::number_to_string(n);
                let h = self.heap.intern_text(&text);
                excl_keys.push(PropKey::from_string(h));
                continue;
            }
            if let Some(b) = v.as_bool() {
                let text = if b { "true" } else { "false" };
                let h = self.heap.intern_text(text);
                excl_keys.push(PropKey::from_string(h));
                continue;
            }
            if v.is_symbol()
                && let Some(y) = v.as_symbol()
            {
                excl_keys.push(PropKey::from_symbol(y));
                continue;
            }
            // Fallback: ToString then intern.
            let h = ops::to_js_string(self.heap, v)?;
            excl_keys.push(PropKey::from_string(h));
        }

        let shape = self.shape_of(src_obj);
        // Snapshot descriptors + properties before the loop below: each
        // iteration calls `add_property`, which allocates a shape slot and
        // may trigger a collection; a borrowed `&sh.descriptors` would dangle
        // across that mutation (and cannot borrow-check against `&mut
        // self.heap`). Descriptors are handles, so a stale-by-one-GC snapshot
        // is fine as long as `src_obj` keeps its shape alive as a root.
        let descs: Vec<v12_heap::Descriptor> = {
            let sh = self.heap.get(shape);
            sh.descriptors.as_slice().to_vec()
        };
        let src_props: Vec<JsValue> = self.heap.get(src_obj).properties.as_slice().to_vec();
        self.gc_protect();
        let dst_h = self.heap.alloc(JsObject::default());
        let mut cur_shape = self.heap.root_shape();
        self.bind_shape(dst_h, cur_shape);
        for desc in descs {
            let key = desc.key();
            // Check if excluded.
            if excl_keys.contains(&key) {
                continue;
            }
            // Only data descriptors with slots are copied; accessors are skipped (hole).
            let Some(slot) = desc.slot() else {
                continue;
            };
            let slot_usize = slot as usize;
            let phys = self.global_slot_index(src_obj, slot_usize);
            if phys >= src_props.len() {
                continue;
            }
            let val = src_props[phys];
            if val.is_hole() {
                continue;
            }
            // Skip non-enumerable? All default are enumerable.
            // Add to dst.
            let child = self
                .heap
                .add_property(cur_shape, key, v12_heap::Attrs::DEFAULT);
            self.bind_shape(dst_h, child);
            self.heap.get_mut(dst_h).properties.push(val);
            cur_shape = child;
        }
        Ok(JsValue::object(dst_h))
    }

    /// `MergeObject`: copies every enumerable own property of `src` onto the
    /// existing object `dst` (object spread). `null`/`undefined` sources are
    /// no-ops per spec; later writes win.
    pub(crate) fn op_merge_object(&mut self, dst_v: JsValue, src_v: JsValue) -> Result<(), JSException> {
        if src_v.is_null() || src_v.is_undefined() {
            return Ok(());
        }
        let Some(src_obj) = src_v.as_object() else {
            // Primitives in spread: spec coerces to object; our subset treats
            // as empty.
            return Ok(());
        };
        let Some(dst_obj) = dst_v.as_object() else {
            return Ok(());
        };
        // Snapshot descriptors + properties before the copy loop (each
        // iteration may allocate).
        let shape = self.shape_of(src_obj);
        let descs: Vec<v12_heap::Descriptor> = {
            let sh = self.heap.get(shape);
            sh.descriptors.as_slice().to_vec()
        };
        let src_props: Vec<JsValue> = self.heap.get(src_obj).properties.as_slice().to_vec();
        let mut cur_shape = self.shape_of(dst_obj);
        self.gc_protect();
        for desc in descs {
            let key = desc.key();
            let Some(slot) = desc.slot() else {
                continue; // accessors skipped
            };
            let phys = self.global_slot_index(src_obj, slot as usize);
            if phys >= src_props.len() {
                continue;
            }
            let val = src_props[phys];
            if val.is_hole() {
                continue;
            }
            let child = self
                .heap
                .add_property(cur_shape, key, v12_heap::Attrs::DEFAULT);
            self.bind_shape(dst_obj, child);
            self.heap.get_mut(dst_obj).properties.push(val);
            cur_shape = child;
        }
        Ok(())
    }

    /// `DefineAccessor`: defines an accessor property on `obj` at `key` with
    /// the given getter/setter function objects (or `undefined` for absent).
    pub(crate) fn op_define_accessor(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        getter_v: JsValue,
        setter_v: JsValue,
    ) -> Result<(), JSException> {
        let Some(obj) = obj_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: cannot define accessor on non-object"),
            ));
        };
        let key = self.property_key(key_v)?;
        let getter = accessor_target(self.heap, getter_v);
        let setter = accessor_target(self.heap, setter_v);
        self.gc_protect();
        let shape = self.shape_of(obj);
        let child = self
            .heap
            .define_accessor(shape, key, getter, setter, Attrs::DEFAULT);
        self.bind_shape(obj, child);
        // Accessor slots hold a hole in `properties`; ensure one exists.
        let slot = child_slot(self.heap, child);
        let props = &mut self.heap.get_mut(obj).properties;
        if props.len() <= slot {
            props.resize(slot + 1, JsValue::hole());
        }
        Ok(())
    }

    /// `SetPrototype`: sets `obj`'s `[[Prototype]]` to `proto` (the class
    /// `extends` wiring). `proto` may be an object or `null`; primitive
    /// prototypes are rejected per ES `OrdinarySetPrototypeOf`.
    pub(crate) fn op_set_prototype(&mut self, obj_v: JsValue, proto_v: JsValue) -> Result<(), JSException> {
        let Some(obj) = obj_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: cannot set prototype of a primitive"),
            ));
        };
        match proto_v {
            v if v.is_null() => {
                if self.heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
                    return Ok(());
                }
                self.heap.get_mut(obj).prototype = None;
                Ok(())
            }
            v => {
                let Some(proto) = v.as_object() else {
                    return Err(JSException(
                        self.error_value("TypeError: prototype must be an object or null"),
                    ));
                };
                if self.heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
                    return Ok(());
                }
                // ES: setting a prototype that creates a cycle is rejected.
                let mut cur = Some(proto);
                while let Some(c) = cur {
                    if c == obj {
                        return Err(JSException(
                            self.error_value("TypeError: cyclic [[Prototype]] value"),
                        ));
                    }
                    cur = self.heap.get(c).prototype;
                }
                self.heap.get_mut(obj).prototype = Some(proto);
                Ok(())
            }
        }
    }

    /// ES GetIterator (7.4.1): `iter = iterable[Symbol.iterator]()`, then
    /// require the result to be an object.
    ///
    /// The realm's `Symbol.iterator` well-known symbol lives on the global
    /// object at the fixed `Symbol` intrinsic's `iterator` property; reading
    /// it goes through the ordinary `get_property` path so user code can
    /// observe and override it.
    pub(crate) fn op_get_iterator(&mut self, src_v: JsValue) -> Result<JsValue, JSException> {
        // 1. `method = GetV(iterable, @@iterator)` — resolve the well-known
        //    symbol first so the lookup uses the real symbol key.
        let method = self.iterator_symbol_method(src_v)?;
        if method.as_object().is_none() {
            return Err(JSException(self.error_value(
                "TypeError: value is not iterable (its Symbol.iterator property is not a function)",
            )));
        }
        // 2. `iterator = Call(method, iterable)` — reuse the call machinery
        //    (handles bytecode natives and engine natives uniformly). The
        //    method object is freshly synthesized by `get_property`; park it
        //    on the stack so the safepoint inside `call_inline` keeps it
        //    alive (only `add_root`-ed otherwise, which the next protect
        //    discards).
        let method_obj = method.as_object().expect("checked above");
        self.stack.push(JsValue::object(method_obj));
        self.gc_protect();
        let result = self.call_inline(method_obj, src_v, &[]);
        self.stack.pop();
        match result {
            Ok(v) if v.as_object().is_some() => Ok(v),
            Ok(_) => Err(JSException(self.error_value(
                "TypeError: result of Symbol.iterator call is not an object",
            ))),
            Err(e) => Err(e),
        }
    }

    /// Resolves the `@@iterator` method value off `obj` (a symbol-keyed
    /// `get_property`), without treating a missing method as an error — the
    /// caller decides the failure mode.
    pub(crate) fn iterator_symbol_method(&mut self, obj_v: JsValue) -> Result<JsValue, JSException> {
        let sym = self.symbol_iterator_key();
        let sym_v = JsValue::symbol(sym);
        self.gc_protect();
        self.get_property(0, 0, obj_v, sym_v)
    }

    /// The realm's `Symbol.iterator` well-known symbol handle, allocated once.
    pub(crate) fn symbol_iterator_key(&mut self) -> Handle<v12_heap::V12Symbol> {
        if let Some(h) = self.symbol_iterator {
            return h;
        }
        // Symbol("Symbol.iterator") — the well-known symbol's [[Description]].
        // The heap's V12Symbol is currently a unit struct; the description is
        // recorded on the Symbol intrinsic's `description` property instead
        // (deferred until the Symbol built-in lands).
        self.gc_protect();
        let h = self.heap.alloc(v12_heap::V12Symbol);
        self.heap.add_root(JsValue::symbol(h));
        self.symbol_iterator = Some(h);
        h
    }

    /// ES IteratorNext (7.4.2): `result = iterator.next()`.
    pub(crate) fn op_iterator_next(&mut self, iter_v: JsValue) -> Result<JsValue, JSException> {
        let Some(_iter_obj) = iter_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: IteratorNext called on non-object"),
            ));
        };
        let next_key = self.new_temp_key("next");
        let next_v = self.get_property(0, 0, iter_v, next_key)?;
        if next_v.as_object().is_none() {
            return Err(JSException(
                self.error_value("TypeError: iterator.next is not a function"),
            ));
        }
        let next_obj = next_v.as_object().expect("checked above");
        // Park the resolved function on the value stack: `gc_protect`
        // republishes the stack as roots, so the function object survives the
        // safepoint inside `call_inline` (it is freshly synthesized by
        // `get_property` and only `add_root`-ed, which the next protect
        // discards).
        self.stack.push(JsValue::object(next_obj));
        self.gc_protect();
        let result = self.call_inline(next_obj, iter_v, &[]);
        self.stack.pop();
        result
    }

    /// ES IteratorClose (7.4.6): call `iterator.return()` when present.
    /// Non-object `return` methods are ignored (spec: return is not a
    /// function → continue unwinding). The close is best-effort — the
    /// original completion always wins.
    pub(crate) fn op_iterator_close(&mut self, iter_v: JsValue) -> Result<(), JSException> {
        let Some(_iter_obj) = iter_v.as_object() else {
            return Ok(());
        };
        let return_key = self.new_temp_key("return");
        let return_v = self.get_property(0, 0, iter_v, return_key)?;
        let Some(return_obj) = return_v.as_object() else {
            return Ok(());
        };
        self.stack.push(JsValue::object(return_obj));
        self.gc_protect();
        let result = self.call_inline(return_obj, iter_v, &[]);
        self.stack.pop();
        let _ = result;
        Ok(())
    }

    /// Interns `text` into a fresh register (compiler-style temp) for use as
    /// a property key operand. Mirrors the compiler's `load_str_key`; kept
    /// here so iterator runtime paths don't hand-build key values.
    pub(crate) fn new_temp_key(&mut self, text: &str) -> JsValue {
        JsValue::string(self.heap.intern_text(text))
    }

    pub(crate) fn op_check_is_array(&mut self, v: JsValue) -> Result<(), JSException> {
        let Some(obj) = v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        };
        if self.heap.get(obj).kind != Kind::Array {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        }
        Ok(())
    }

    pub(crate) fn op_array_append(&mut self, dst_v: JsValue, src_v: JsValue) -> Result<(), JSException> {
        let Some(dst_obj) = dst_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: destination is not an object"),
            ));
        };
        if self.heap.get(dst_obj).kind != Kind::Array {
            return Err(JSException(
                self.error_value("TypeError: destination is not an array"),
            ));
        }
        let Some(src_obj) = src_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        };
        if self.heap.get(src_obj).kind != Kind::Array {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        }
        let src_elements: Vec<JsValue> = self.heap.get(src_obj).elements_snapshot();
        if src_elements.is_empty() {
            return Ok(());
        }
        // Extend dst elements and update length.
        let dst_len_before = self.heap.get(dst_obj).elements_array.len();
        let new_len = dst_len_before + src_elements.len();
        self.gc_protect();
        // Update shape length if needed (array length property is slot 0).
        let shape = self.shape_of(dst_obj);
        let len_key = self.length_key();
        let slot = self
            .heap
            .lookup_property(shape, len_key)
            .and_then(|d| d.slot())
            .map(|s| s as usize);
        if let Some(slot) = slot {
            self.heap.get_mut(dst_obj).properties[slot] =
                ops::box_number(f64::from(new_len as u32));
        }
        let dst = &mut self.heap.get_mut(dst_obj).elements_array;
        for v in src_elements {
            dst.push(v);
        }
        Ok(())
    }

    /// Physical index for a shape-derived property `slot` on object `obj`.
    ///
    /// The global object's `properties` vector is prefixed by
    /// `GLOBAL_VAR_OFFSET` intrinsic slots that the shape graph does not
    /// track (the realm installs them by pushing directly), so every
    /// descriptor slot on the global maps to `GLOBAL_VAR_OFFSET + slot`;
    /// ordinary objects use the slot as-is.

pub(crate) fn array_join_fallback(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let Some(arr) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: Array.prototype.join requires an array"),
            ));
        };
        let sep = if let Some(&v) = args.first() {
            if v.is_undefined() {
                ",".to_string()
            } else {
                self.to_display_string(v)
            }
        } else {
            ",".to_string()
        };
        let elements: Vec<JsValue> = self.heap.get(arr).elements_snapshot();
        let mut parts = Vec::with_capacity(elements.len());
        for &v in &elements {
            if v.is_undefined() || v.is_null() || v.is_hole() {
                parts.push(String::new());
            } else {
                parts.push(self.to_display_string(v));
            }
        }
        self.gc_protect();
        Ok(JsValue::string(self.heap.intern_text(&parts.join(&sep))))
    }

    pub(crate) fn array_push_fallback(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let Some(obj) = this_v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: Array.prototype.push called on non-object",
            )));
        };
        for &item in args {
            self.heap.get_mut(obj).push_element(item);
        }
        let new_len = self.heap.get(obj).element_len() as u32;
        // Sync length if shape exists
        let key = self.length_key();
        let shape = self.shape_of(obj);
        if let Some(desc) = self
            .heap
            .lookup_property(shape, key)
            .and_then(|d| d.slot().map(|s| s as usize))
            && desc < self.heap.get(obj).properties.len()
        {
            self.heap.get_mut(obj).properties[desc] = ops::box_number(f64::from(new_len));
        }
        Ok(ops::box_number(f64::from(new_len)))
    }
}