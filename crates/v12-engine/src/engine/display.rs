//! Value display: `to_display_string` rendering (strings, objects with
//! name/message, functions, arrays) used by errors and host output.


use v12_heap::{JsValue, V12Str};

use super::Engine;

impl Engine {
    pub(crate) fn heap_string_text(&mut self, handle: v12_heap::Handle<V12Str>) -> String {
        self.heap.flatten(handle);
        match &self.heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
            _ => String::new(),
        }
    }

    /// Returns a display string for a value, using the engine heap.
    pub fn to_display_string(&mut self, value: JsValue) -> String {
        // For engine-heap values, intern and flatten via heap string ops.
        if let Some(handle) = value.as_string() {
            self.heap.flatten(handle);
            match &self.heap.get(handle).storage {
                v12_heap::StrStorage::Latin1(bytes) => {
                    return String::from_utf8_lossy(bytes).into_owned();
                }
                v12_heap::StrStorage::Utf16(units) => return String::from_utf16_lossy(units),
                _ => return String::new(),
            }
        }
        // Real error objects render as "Name: message".
        if value.is_object()
            && let Some(obj) = value.as_object()
            && self.heap.get(obj).kind == v12_heap::Kind::Error
        {
            // Snapshot the handles first so the text decode (which needs
            // `&mut self`) doesn't fight the borrow.
            let name_h = self
                .heap
                .get(obj)
                .properties
                .first()
                .and_then(|v| v.as_string());
            let msg_h = self
                .heap
                .get(obj)
                .properties
                .get(1)
                .and_then(|v| v.as_string());
            let name = name_h
                .map(|h| self.heap_string_text(h))
                .unwrap_or_else(|| "Error".to_string());
            let msg = msg_h.map(|h| self.heap_string_text(h)).unwrap_or_default();
            if msg.is_empty() {
                return name;
            }
            return format!("{name}: {msg}");
        }
        if let Some(n) = value.as_smi().map(f64::from).or(value.as_f64()) {
            if n.is_nan() {
                return "NaN".to_string();
            }
            if n == f64::INFINITY {
                return "Infinity".to_string();
            }
            if n == f64::NEG_INFINITY {
                return "-Infinity".to_string();
            }
            return format!("{n}");
        }
        if value.is_true() {
            return "true".to_string();
        }
        if value.is_false() {
            return "false".to_string();
        }
        if value.is_undefined() {
            return "undefined".to_string();
        }
        if value.is_null() {
            return "null".to_string();
        }
        if value.is_object() {
            // Real arrays render as their comma-joined elements (so `map`
            // results don't display as `[object Object]`); other objects
            // keep the error-message lookup below.
            if let Some(obj) = value.as_object()
                && self.heap.get(obj).kind == v12_heap::Kind::Array
            {
                return self.array_join_text(obj, 0);
            }
            // Plain-object errors (e.g. Test262Error) are not Kind::Error but
            // carry a `message` property. Render them usefully instead of
            // opaque "[object Object]" so the runner bucket becomes actionable.
            if let Some(obj) = value.as_object() {
                let shape = self.heap.shape_of(obj);
                let lookup_str_prop = |heap: &mut v12_heap::Heap, key: &str| -> Option<v12_heap::Handle<v12_heap::V12Str>> {
                    let h = heap.intern_text(key);
                    let pk = v12_heap::PropKey::from_string(h);
                    let desc = heap.lookup_property(shape, pk)?;
                    let slot = desc.slot()?;
                    // Ordinary objects store properties at `slot`; the global object biases by INTRINSIC_COUNT.
                    let props = &heap.get(obj).properties;
                    let idx_plain = slot as usize;
                    if let Some(v) = props.get(idx_plain).and_then(|v| v.as_string()) { return Some(v); }
                    let idx_global = crate::realm::INTRINSIC_COUNT + slot as usize;
                    props.get(idx_global).and_then(|v| v.as_string())
                };
                // Snapshot handles before borrowing self mutably for text decode.
                let msg_h = lookup_str_prop(&mut self.heap, "message");
                if let Some(mh) = msg_h {
                    let name_h = lookup_str_prop(&mut self.heap, "name");
                    let msg = self.heap_string_text(mh);
                    if let Some(nh) = name_h {
                        let name = self.heap_string_text(nh);
                        if msg.is_empty() { return name; }
                        return format!("{name}: {msg}");
                    }
                    // No name — return the message directly (covers Test262Error which
                    // stores only `message`; prefixing with generic "Error" would be noisy).
                    if !msg.is_empty() { return msg; }
                }
            }
            return "[object Object]".to_string();
        }
        "<unprintable>".to_string()
    }

    /// Comma-joined element text of a real array (`undefined`/`null`/holes
    /// render empty, matching `Array.prototype.join`). Nested arrays
    /// recurse; `depth` caps the recursion so cyclic arrays terminate.
    fn array_join_text(&mut self, obj: v12_heap::Handle<v12_heap::JsObject>, depth: usize) -> String {
        if depth > 8 {
            return String::new();
        }
        // Snapshot before formatting: rendering an element may allocate
        // (and thus collect), invalidating a live borrow of the store.
        let elements: Vec<JsValue> = self.heap.get(obj).elements_snapshot();
        let mut parts = Vec::with_capacity(elements.len());
        for v in elements {
            if v.is_undefined() || v.is_null() || v.is_hole() {
                parts.push(String::new());
            } else if let Some(nested) = v
                .as_object()
                .filter(|h| self.heap.get(*h).kind == v12_heap::Kind::Array)
            {
                parts.push(self.array_join_text(nested, depth + 1));
            } else {
                parts.push(self.to_display_string(v));
            }
        }
        parts.join(",")
    }
}

