//! Global-variable opcodes: `GetGlobal`/`SetGlobal` slot resolution over the
//! realm global object (intrinsic slots + user slots with the GLOBAL_VAR_OFFSET
//! bias.

use v12_heap::{Handle, JsObject, JsValue, PropKey, V12Str};

use super::{intrinsic_slot, Interp, JSException, GLOBAL_VAR_OFFSET};

impl Interp<'_> {
    pub(crate) fn global_slot_index(&self, obj: Handle<JsObject>, slot: usize) -> usize {
        if self.is_realm_global(obj) {
            GLOBAL_VAR_OFFSET + slot
        } else {
            slot
        }
    }

    /// True when `obj` is a realm global: the interpreter's own global or a
    /// secondary realm's global created on the same heap (via
    /// `$262.createRealm`). All realm globals carry the descriptor-less
    /// intrinsic prefix, so their var slots share the `GLOBAL_VAR_OFFSET`
    /// bias and their intrinsic reads fall back to the fixed prefix slots.
    ///
    /// O(1): one handle compare for the interpreter's own global, one
    /// header-bit read (`Heap::REALM_GLOBAL_FLAG`, stamped by
    /// `Heap::register_realm_global`) for secondary-realm globals. The
    /// `realm_globals` vec is never scanned here — it serves only as the
    /// GC-root registry.
    pub(crate) fn is_realm_global(&self, obj: Handle<JsObject>) -> bool {
        Some(obj) == self.global || self.heap.is_realm_global(obj)
    }

    /// Member-read fallback for a realm global: the intrinsic prefix slots
    /// have no shape descriptors, so a generic read like `other.eval` or
    /// `globalThis.Math` misses the shape walk. When the receiver is a realm
    /// global and the key names an intrinsic, answer from the prefix.
    /// Returns `None` to defer to the ordinary (undefined) miss path.
    ///
    /// O(1): the key flattens once, then a single `intrinsic_slot` match
    /// (compiler jump table) indexes the fixed prefix array directly. No
    /// `GLOBAL_INTRINSICS` loop, no per-name `key_is` reflatten+memcmp.
    pub(crate) fn realm_global_intrinsic_read(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
    ) -> Option<JsValue> {
        if !self.is_realm_global(obj) {
            return None;
        }
        let handle = key_v.as_string()?;
        self.heap.flatten(handle);
        // Intrinsic names are ASCII; a UTF-16 key with non-ASCII units
        // cannot name one (same gate as `method_native`).
        let idx = match &self.heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => {
                intrinsic_slot(std::str::from_utf8(bytes).ok()?)?
            }
            v12_heap::StrStorage::Utf16(units) => {
                if !units.iter().all(|&u| u < 128) {
                    return None;
                }
                let bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
                intrinsic_slot(std::str::from_utf8(&bytes).ok()?)?
            }
            _ => return None,
        };
        self.global_slot_value(obj, idx)
    }

    /// The value of a populated global slot: `None` for out-of-range and
    /// hole slots (a hole means "never initialized").
    pub(crate) fn global_slot_value(&self, global: Handle<JsObject>, idx: usize) -> Option<JsValue> {
        let v = *self.heap.get(global).properties.get(idx)?;
        if v.is_hole() { None } else { Some(v) }
    }

    /// The intrinsic-slot value for a global name (the fixed prefix slots),
    /// or `None` when the name is not an intrinsic or the slot is unpopulated.
    pub(crate) fn global_intrinsic_value(&self, global: Handle<JsObject>, text: &str) -> Option<JsValue> {
        let idx = intrinsic_slot(text)?;
        self.global_slot_value(global, idx)
    }

    /// The own-property value for `text` via the shape graph, mapped through
    /// [`Self::global_slot_index`]. `None` when the name is not an own data
    /// property or the slot is unpopulated.
    pub(crate) fn global_property_value(&mut self, global: Handle<JsObject>, text: &str) -> Option<JsValue> {
        let h = self.heap.intern_text(text);
        let key = PropKey::from_string(h);
        let shape = self.shape_of(global);
        let desc = self.heap.lookup_property(shape, key)?;
        let idx = self.global_slot_index(global, desc.slot()? as usize);
        self.global_slot_value(global, idx)
    }

    /// The physical slot for an own data property named by the already
    /// interned handle `h`, mapped through [`Self::global_slot_index`].
    /// Takes the handle (not text) so callers intern once per access and
    /// share it between lookup and creation.
    pub(crate) fn global_own_property_slot(
        &mut self,
        global: Handle<JsObject>,
        h: Handle<V12Str>,
    ) -> Option<usize> {
        let key = PropKey::from_string(h);
        let shape = self.shape_of(global);
        let desc = self.heap.lookup_property(shape, key)?;
        let slot = desc.slot()?;
        Some(self.global_slot_index(global, slot as usize))
    }

    /// Writes a global slot, growing the properties vector when an
    /// embedder-assembled global lacks the full slot range.
    pub(crate) fn write_global_slot(&mut self, global: Handle<JsObject>, idx: usize, val: JsValue) {
        let len = self.heap.get(global).properties.len();
        if len <= idx {
            self.heap
                .get_mut(global)
                .properties
                .resize(idx + 1, JsValue::undefined());
        }
        self.heap.get_mut(global).properties[idx] = val;
    }

    pub(crate) fn op_get_global(&mut self, str_id: u32, program: u32) -> Result<JsValue, JSException> {
        let Some(v) = self.resolve_global(str_id, program) else {
            // Missing binding: the compiler only emits `GetGlobal` for
            // declared variables, hoisted names, and intrinsics, so an
            // unresolved read here is a genuine undeclared reference.
            let text = self.global_name_text(str_id, program);
            return Err(JSException(
                self.error_value(&format!("ReferenceError: {text} is not defined")),
            ));
        };
        Ok(v)
    }

    /// `GetGlobalLenient`: same resolution as `GetGlobal`, but a missing
    /// binding yields `undefined` (spec: `typeof undeclared` never throws).
    pub(crate) fn op_get_global_lenient(&mut self, str_id: u32, program: u32) -> Result<JsValue, JSException> {
        Ok(self.resolve_global(str_id, program).unwrap_or_else(JsValue::undefined))
    }

    fn global_name_text(&mut self, str_id: u32, program: u32) -> String {
        self.strings_for_program(program)
            .get(str_id as usize)
            .cloned()
            .unwrap_or_default()
    }

    /// Common resolution path for `GetGlobal`/`GetGlobalLenient`: `None` when
    /// no binding of the name exists.
    ///
    /// Interns at most once per access: the intrinsic fast path
    /// (`intrinsic_slot` match) interns zero times, and the own-property
    /// fallback interns exactly once inside `global_property_value`.
    fn resolve_global(&mut self, str_id: u32, program: u32) -> Option<JsValue> {
        let global = self.global?;
        // The fast path allocates only when interning an unseen key, but any
        // `Heap::alloc` can collect — publish roots first so values written
        // since the last opcode-level protect stay reachable.
        self.gc_protect();
        // Borrow the compiler's string table entry (program-scoped: an eval
        // frame's `Str32` ids index the eval program's string table):
        // comparing against the intrinsics list and interning both take
        // &str, so no String clone is needed on this fast path.
        let strings = self.strings_for_program(program);
        let text: &str = strings
            .get(str_id as usize)
            .map(String::as_str)
            .unwrap_or("");
        // The `arguments` binding resolves to the current activation's
        // materialized object, not the global. Falls through when no frame
        // on the stack carries one (top-level code, functions that never
        // reference it), preserving ordinary global reads.
        if text == "arguments"
            && let Some(v) = self.frame_arguments_value()
        {
            return Some(v);
        }
        if let Some(v) = self.global_intrinsic_value(global, text) {
            return Some(v);
        }
        if let Some(v) = self.global_property_value(global, text) {
            return Some(v);
        }
        None
    }

    pub(crate) fn op_set_global(
        &mut self,
        str_id: u32,
        val: JsValue,
        program: u32,
    ) -> Result<(), JSException> {
        let Some(global) = self.global else {
            return Ok(());
        };
        // Interning a new key and the shape transition below can each
        // allocate; publish roots first so `val` survives any collection.
        self.gc_protect();
        // Borrow the string-table entry (no `String` clone): the borrow
        // lives on the locally owned `strings`, never across `&mut self`.
        let strings = self.strings_for_program(program);
        let text: &str = strings
            .get(str_id as usize)
            .map(String::as_str)
            .unwrap_or("");
        // Writes to the `arguments` binding land on the current activation
        // when one carries a materialized object, mirroring the read path.
        if text == "arguments" && self.frame_arguments_value().is_some() {
            self.set_frame_arguments(val);
            return Ok(());
        }
        if let Some(idx) = intrinsic_slot(text)
            && idx < self.heap.get(global).properties.len()
        {
            // Intrinsics are at fixed indices; allow overwriting. An
            // out-of-range intrinsic slot (an embedder-built global without
            // the full prefix) falls through to the shape path instead.
            self.heap.get_mut(global).properties[idx] = val;
            return Ok(());
        }
        // Intern once: this handle serves the own-slot lookup below and
        // the creation path after it — no second hash/alloc per access.
        let h = self.heap.intern_text(text);
        if let Some(idx) = self.global_own_property_slot(global, h) {
            self.write_global_slot(global, idx, val);
            return Ok(());
        }
        // Otherwise, create new global property. The shape transition may
        // allocate, but roots were published at the top of this handler and
        // nothing here introduces values beyond that set (the interned key is
        // kept alive by the strong intern table), so no re-protect is needed.
        // Keep the physical index in sync with the shape's slot numbering.
        let key = PropKey::from_string(h);
        let shape = self.shape_of(global);
        let child = self.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
        self.bind_shape(global, child);
        let new_slot = usize::try_from(self.heap.get(child).num_own - 1).expect("slot fits usize");
        self.write_global_slot(global, self.global_slot_index(global, new_slot), val);
        Ok(())
    }
}