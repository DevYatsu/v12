//! Destructuring and object-composition opcodes: `CopyArrayRest`/
//! `CopyObjectRest`/`MergeObject`, iterator protocol ops, and array append.

use v12_heap::{Attrs, Handle, JsObject, JsValue, Kind, PropKey};

use super::{Interp, JSException, accessor_target, child_slot};
use crate::ops;

impl Interp<'_> {
    pub(crate) fn op_copy_array_rest(
        &mut self,
        src_v: JsValue,
        start: u16,
    ) -> Result<JsValue, JSException> {
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
        self.link_array_proto(h);
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
    pub(crate) fn op_merge_object(
        &mut self,
        dst_v: JsValue,
        src_v: JsValue,
    ) -> Result<(), JSException> {
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
            .define_accessor(shape, key, getter, setter, Attrs::BUILTIN);
        self.bind_shape(obj, child);
        // Accessor slots hold a hole in `properties`; ensure one exists.
        let slot = child_slot(self.heap, child);
        let props = &mut self.heap.get_mut(obj).properties;
        if props.len() <= slot {
            props.resize(slot + 1, JsValue::hole());
        }
        Ok(())
    }

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

    /// `SetPrototype`: sets `obj`'s `[[Prototype]]` to `proto` (the class
    /// `extends` wiring). `proto` may be an object or `null`; primitive
    /// prototypes are rejected per ES `OrdinarySetPrototypeOf`.
    pub(crate) fn op_set_prototype(
        &mut self,
        obj_v: JsValue,
        proto_v: JsValue,
    ) -> Result<(), JSException> {
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
        self.get_iterator(src_v, false)
    }

    /// ES GetIterator (7.4.1) with the `async` hint. When `is_async`, the
    /// `@@asyncIterator` method is preferred; if absent, the sync
    /// `@@iterator` method is used and its results are awaited by the
    /// for-await lowering (`AsyncFromSyncIterator` semantics approximated by
    /// the `Await` around `IteratorNext` and the bound value). When
    /// `is_async` is false the `@@iterator` method is used.
    pub(crate) fn get_iterator(
        &mut self,
        src_v: JsValue,
        is_async: bool,
    ) -> Result<JsValue, JSException> {
        // 1. `method = GetV(iterable, @@iterator)` — resolve the well-known
        //    symbol first so the lookup uses the real symbol key.
        let method = if is_async {
            let async_method = self.async_iterator_symbol_method(src_v)?;
            if async_method.is_null() || async_method.is_undefined() {
                self.iterator_symbol_method(src_v)?
            } else {
                async_method
            }
        } else {
            self.iterator_symbol_method(src_v)?
        };
        // 2. `iterator = Call(method, iterable)` — reuse the call machinery
        //    (handles bytecode natives and engine natives uniformly). The
        //    method object is freshly synthesized by `get_property`; park it
        //    on the stack so the safepoint inside `call_inline` keeps it
        //    alive (only `add_root`-ed otherwise, which the next protect
        //    discards).
        //    GetMethod gate (spec 7.4.6): a present non-callable `@@iterator`
        //    is a TypeError. Gate on `Kind::Function` (the same callability
        //    gate as `op_iterator_close`), not mere object-ness: a plain
        //    object in the slot must throw here rather than fall through to
        //    `call_inline` and read a placeholder callable.
        let method_obj = method
            .as_object()
            .filter(|h| self.heap.get(*h).kind == Kind::Function)
            .ok_or_else(|| {
                JSException(self.error_value(
                    "TypeError: value is not iterable (its Symbol.iterator property is not a function)",
                ))
            })?;
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

    /// Resolves the `@@asyncIterator` method value off `obj` (a symbol-keyed
    /// `get_property`); a missing method yields `undefined` so the caller can
    /// fall back to the sync iterator.
    pub(crate) fn async_iterator_symbol_method(
        &mut self,
        obj_v: JsValue,
    ) -> Result<JsValue, JSException> {
        let sym = self.symbol_async_iterator_key();
        let sym_v = JsValue::symbol(sym);
        self.gc_protect();
        self.get_property(0, 0, obj_v, sym_v)
    }

    /// The realm's `Symbol.asyncIterator` well-known symbol handle, allocated
    /// once (distinct from `Symbol.iterator`).
    pub(crate) fn symbol_async_iterator_key(&mut self) -> Handle<v12_heap::V12Symbol> {
        if let Some(h) = self.symbol_async_iterator {
            return h;
        }
        self.gc_protect();
        let h = self.heap.alloc(v12_heap::V12Symbol);
        self.heap.add_root(JsValue::symbol(h));
        self.symbol_async_iterator = Some(h);
        h
    }

    /// Resolves the `@@iterator` method value off `obj` (a symbol-keyed
    /// `get_property`), without treating a missing method as an error — the
    /// caller decides the failure mode.
    pub(crate) fn iterator_symbol_method(
        &mut self,
        obj_v: JsValue,
    ) -> Result<JsValue, JSException> {
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
    /// GetMethod gate + result validation: a present non-callable `next`
    /// is a TypeError (same `Kind::Function` gate as `op_iterator_close`),
    /// and a non-Object `next()` result is a TypeError (spec 7.4.2 step 3).
    /// Without the result check the primitive flows into the `done`/`value`
    /// reads and the abrupt completion is silently dropped.
    pub(crate) fn op_iterator_next(&mut self, iter_v: JsValue) -> Result<JsValue, JSException> {
        let Some(_iter_obj) = iter_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: IteratorNext called on non-object"),
            ));
        };
        let next_key = self.new_temp_key("next");
        let next_v = self.get_property(0, 0, iter_v, next_key)?;
        let Some(next_obj) = next_v
            .as_object()
            .filter(|h| self.heap.get(*h).kind == Kind::Function)
        else {
            return Err(JSException(
                self.error_value("TypeError: iterator.next is not a function"),
            ));
        };
        // Park the resolved function on the value stack: `gc_protect`
        // republishes the stack as roots, so the function object survives the
        // safepoint inside `call_inline` (it is freshly synthesized by
        // `get_property` and only `add_root`-ed, which the next protect
        // discards).
        self.stack.push(JsValue::object(next_obj));
        self.gc_protect();
        let result = self.call_inline(next_obj, iter_v, &[]);
        self.stack.pop();
        match result {
            Ok(v) if v.as_object().is_some() => Ok(v),
            Ok(_) => Err(JSException(
                self.error_value("TypeError: iterator.next() returned a non-object"),
            )),
            Err(e) => Err(e),
        }
    }

    /// ES IteratorClose (7.4.6): call `iterator.return()` when present.
    /// GetMethod semantics: null/undefined means "no close"; any other
    /// non-callable value is a TypeError. Callability is `Kind::Function`
    /// (the same gate as `prepare_call`), not mere object-ness: a plain
    /// object in the `return` slot must throw here rather than fall through
    /// to `call_inline` and read a placeholder callable.
    /// A throw from `return()` propagates: on the inline break/return path
    /// the close error is the completion, and on the exception-handler path
    /// the compiler rethrows the original error after a successful close.
    /// `check_result` is the completion-aware close contract: spec 7.4.6
    /// validates the `return()` result as an Object only when the incoming
    /// completion is NOT throw ("if completion is throw, return
    /// completion"). The opcode carries `rb` as the flag — `0` on the
    /// handler (throw) path, `1` on the normal break/return path. Validating
    /// unconditionally regresses the throw path (a `return(){}` yielding
    /// `undefined` would mask the original error with a TypeError); skipping
    /// unconditionally swallows the normal-path abrupt (a non-Object result
    /// on `break`/`return` must throw).
    pub(crate) fn op_iterator_close(
        &mut self,
        iter_v: JsValue,
        throw_path: bool,
    ) -> Result<(), JSException> {
        // Spec 7.4.6 step "if completion is throw, return completion": on
        // the handler path the original abrupt always wins — a GetMethod
        // abrupt, a non-callable `return`, a `return()` throw, or a
        // non-Object result must NOT mask it. Swallow every close error and
        // let the caller rethrow the original via `Throw exc`.
        let result = self.fallible_iterator_close(iter_v, !throw_path);
        if throw_path { Ok(()) } else { result }
    }

    /// The fallible half of [`Self::op_iterator_close`]: GetMethod gate,
    /// `return()` call, and — when `check_result` — the Object-result
    /// validation. Errors propagate to the caller.
    fn fallible_iterator_close(
        &mut self,
        iter_v: JsValue,
        check_result: bool,
    ) -> Result<(), JSException> {
        let Some(_iter_obj) = iter_v.as_object() else {
            return Ok(());
        };
        let return_key = self.new_temp_key("return");
        let return_v = self.get_property(0, 0, iter_v, return_key)?;
        if return_v.is_null() || return_v.is_undefined() {
            return Ok(());
        }
        let Some(return_obj) = return_v
            .as_object()
            .filter(|h| self.heap.get(*h).kind == Kind::Function)
        else {
            return Err(JSException(
                self.error_value("TypeError: iterator.return is not a function"),
            ));
        };
        self.stack.push(JsValue::object(return_obj));
        self.gc_protect();
        let inner = self.call_inline(return_obj, iter_v, &[]);
        self.stack.pop();
        match inner {
            Err(e) => Err(e),
            Ok(v) if check_result && v.as_object().is_none() => Err(JSException(
                self.error_value("TypeError: iterator.return() returned a non-object"),
            )),
            Ok(_) => Ok(()),
        }
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

    pub(crate) fn op_array_append(
        &mut self,
        dst_v: JsValue,
        src_v: JsValue,
    ) -> Result<(), JSException> {
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
        // Spec: appended items land at `length`-based indices. For a sparse
        // array (`new Array(len)`) the element store is shorter than the
        // `length` property, so the store end is not the write index. The
        // length is an f64 (ToLength): array-like receivers legally carry
        // lengths up to 2^53-1, and indices beyond the element-store range
        // skip the store write (the flat guard refuses huge-gap resizes).
        let key = self.length_key();
        let shape = self.shape_of(obj);
        let mut len = self
            .heap
            .lookup_property(shape, key)
            .and_then(|d| d.slot().map(|s| s as usize))
            .filter(|&idx| idx < self.heap.get(obj).properties.len())
            .and_then(|idx| {
                let v = self.heap.get(obj).properties[idx];
                v.as_smi()
                    .map(|s| f64::from(s))
                    .or_else(|| v.as_f64())
                    .map(|n| {
                        if n.is_nan() {
                            0.0
                        } else {
                            n.trunc().clamp(0.0, 9007199254740991.0)
                        }
                    })
            })
            .unwrap_or_else(|| self.heap.get(obj).element_len() as f64);
        for &item in args {
            if len <= 4294967294.0 {
                self.heap.get_mut(obj).set_element(len as u32, item);
            }
            len += 1.0;
        }
        let new_len = len;
        // Sync length if shape exists
        if let Some(desc) = self
            .heap
            .lookup_property(shape, key)
            .and_then(|d| d.slot().map(|s| s as usize))
            && desc < self.heap.get(obj).properties.len()
        {
            self.heap.get_mut(obj).properties[desc] = ops::box_number(new_len);
        }
        Ok(ops::box_number(new_len))
    }
}
