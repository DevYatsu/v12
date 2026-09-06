//! Global-variable opcodes: `GetGlobal`/`SetGlobal` slot resolution over the
//! realm global object (intrinsic slots + user slots with the GLOBAL_VAR_OFFSET
//! bias.

use v12_heap::{Handle, JsObject, JsValue, PropKey};

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
    pub(crate) fn is_realm_global(&self, obj: Handle<JsObject>) -> bool {
        Some(obj) == self.global || self.heap.realm_globals().contains(&obj)
    }

    /// Member-read fallback for a realm global: the intrinsic prefix slots
    /// have no shape descriptors, so a generic read like `other.eval` or
    /// `globalThis.Math` misses the shape walk. When the receiver is a realm
    /// global and the key names an intrinsic, answer from the prefix.
    /// Returns `None` to defer to the ordinary (undefined) miss path.
    pub(crate) fn realm_global_intrinsic_read(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
    ) -> Option<JsValue> {
        if !self.is_realm_global(obj) {
            return None;
        }
        for &name in v12_bytecode::GLOBAL_INTRINSICS {
            if self.key_is(key_v, name) {
                return self.global_intrinsic_value(obj, name);
            }
        }
        None
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

    /// The physical slot for an own data property named `text`, mapped
    /// through [`Self::global_slot_index`]: intern the name, resolve the
    /// shape descriptor, translate the slot.
    pub(crate) fn global_own_property_slot(&mut self, global: Handle<JsObject>, text: &str) -> Option<usize> {
        let h = self.heap.intern_text(text);
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
        let Some(global) = self.global else {
            return Ok(JsValue::undefined());
        };
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
            return Ok(v);
        }
        if let Some(v) = self.global_intrinsic_value(global, text) {
            return Ok(v);
        }
        if let Some(v) = self.global_property_value(global, text) {
            return Ok(v);
        }
        Ok(JsValue::undefined())
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
        let strings = self.strings_for_program(program);
        let text = strings.get(str_id as usize).cloned().unwrap_or_default();
        // Writes to the `arguments` binding land on the current activation
        // when one carries a materialized object, mirroring the read path.
        if text == "arguments" && self.frame_arguments_value().is_some() {
            self.set_frame_arguments(val);
            return Ok(());
        }
        if let Some(idx) = intrinsic_slot(&text)
            && idx < self.heap.get(global).properties.len()
        {
            // Intrinsics are at fixed indices; allow overwriting. An
            // out-of-range intrinsic slot (an embedder-built global without
            // the full prefix) falls through to the shape path instead.
            self.heap.get_mut(global).properties[idx] = val;
            return Ok(());
        }
        if let Some(idx) = self.global_own_property_slot(global, &text) {
            self.write_global_slot(global, idx, val);
            return Ok(());
        }
        // Otherwise, create new global property. The shape transition may
        // allocate, but roots were published at the top of this handler and
        // nothing here introduces values beyond that set (the interned key is
        // kept alive by the strong intern table), so no re-protect is needed.
        // Keep the physical index in sync with the shape's slot numbering.
        let h = self.heap.intern_text(&text);
        let key = PropKey::from_string(h);
        let shape = self.shape_of(global);
        let child = self.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
        self.bind_shape(global, child);
        let new_slot = usize::try_from(self.heap.get(child).num_own - 1).expect("slot fits usize");
        self.write_global_slot(global, self.global_slot_index(global, new_slot), val);
        Ok(())
    }
}