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

    /// Reads a property `key` walking the prototype chain (own shape first,
    /// then each `[[Prototype]]`): the first hit whose value is a string.
    /// Shape lookup is own-shape only, so inherited links like
    /// `instance.constructor` (living on the class prototype) need the walk.
    fn chain_str_prop(
        &mut self,
        obj: v12_heap::Handle<v12_heap::JsObject>,
        key: &str,
    ) -> Option<v12_heap::Handle<V12Str>> {
        let h = self.heap.intern_text(key);
        let pk = v12_heap::PropKey::from_string(h);
        let mut cur = Some(obj);
        while let Some(o) = cur {
            let shape = self.heap.shape_of(o);
            if let Some(desc) = self.heap.lookup_property(shape, pk)
                && let Some(slot) = desc.slot()
            {
                let props = &self.heap.get(o).properties;
                let hit = props
                    .get(slot as usize)
                    .and_then(|v| v.as_string())
                    .or_else(|| {
                        let idx = crate::realm::INTRINSIC_COUNT + slot as usize;
                        props.get(idx).and_then(|v| v.as_string())
                    });
                if hit.is_some() {
                    return hit;
                }
            }
            cur = self.heap.get(o).prototype;
        }
        None
    }

    /// Reads an object-valued property `key` walking the prototype chain:
    /// the first hit whose value is an object.
    fn chain_obj_prop(
        &mut self,
        obj: v12_heap::Handle<v12_heap::JsObject>,
        key: &str,
    ) -> Option<v12_heap::Handle<v12_heap::JsObject>> {
        let h = self.heap.intern_text(key);
        let pk = v12_heap::PropKey::from_string(h);
        let mut cur = Some(obj);
        while let Some(o) = cur {
            let shape = self.heap.shape_of(o);
            if let Some(desc) = self.heap.lookup_property(shape, pk)
                && let Some(slot) = desc.slot()
            {
                let props = &self.heap.get(o).properties;
                let hit = props
                    .get(slot as usize)
                    .and_then(|v| v.as_object())
                    .or_else(|| {
                        let idx = crate::realm::INTRINSIC_COUNT + slot as usize;
                        props.get(idx).and_then(|v| v.as_object())
                    });
                if hit.is_some() {
                    return hit;
                }
            }
            cur = self.heap.get(o).prototype;
        }
        None
    }

    /// Returns a display string for a value, using the engine heap.
    pub fn to_display_string(&mut self, value: JsValue) -> String {
        self.display_value(value, 0)
    }

    /// Depth-threaded display worker: `to_display_string` starts at 0 and
    /// composite kinds recurse with `depth + 1` so cyclic structures
    /// (a Map holding itself, an array holding its own Map, …) terminate
    /// instead of overflowing the stack.
    fn display_value(&mut self, value: JsValue, depth: usize) -> String {
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
        // BigInts render decimal with the `n` suffix (`10n`, `-3n`).
        if let Some(handle) = value.as_bigint() {
            let big = self.heap.get(handle).clone();
            return Self::bigint_text(&big);
        }
        // Symbols are opaque in v1 (no description slot); render like the
        // engine's own `Symbol.prototype.toString`.
        if value.as_symbol().is_some() {
            return "Symbol()".to_string();
        }
        if value.is_object() {
            // Real arrays render as their comma-joined elements (so `map`
            // results don't display as `[object Object]`); other objects
            // keep the error-message lookup below.
            if let Some(obj) = value.as_object()
                && self.heap.get(obj).kind == v12_heap::Kind::Array
            {
                return self.array_join_text(obj, depth);
            }
            // Non-recursive composite kinds carry enough info for a useful
            // rendering without descending into their payload, so they render
            // at any depth (no cap needed). Functions use the Node convention
            // `[Function: name]`; the name is the closure's own `name` data
            // property, installed at closure creation.
            if let Some(obj) = value.as_object() {
                match self.heap.get(obj).kind {
                    v12_heap::Kind::Function => return self.function_text(obj),
                    v12_heap::Kind::Generator => return "[Generator]".to_string(),
                    v12_heap::Kind::Iterator => return self.iterator_text(obj),
                    _ => {}
                }
            }
            // Collection / promise / regexp objects carry their payload in
            // elements or internal slots: render it instead of opaque
            // `[object Object]`. Past the depth cap, bail out so cyclic
            // structures terminate.
            if depth <= 8
                && let Some(obj) = value.as_object()
            {
                let kind = self.heap.get(obj).kind;
                match kind {
                    v12_heap::Kind::Map => return self.map_text(obj, depth),
                    v12_heap::Kind::Set => return self.set_text(obj, depth),
                    v12_heap::Kind::Promise => return self.promise_text(obj, depth),
                    v12_heap::Kind::RegExp => {
                        if let Some(text) = self.regexp_text(obj) {
                            return text;
                        }
                    }
                    _ => {}
                }
            }
            // Plain-object errors (e.g. Test262Error) are not Kind::Error but
            // carry a `message` property. Render them usefully instead of
            // opaque "[object Object]" so the runner bucket becomes actionable.
            if let Some(obj) = value.as_object() {
                let shape = self.heap.shape_of(obj);
                let lookup_str_prop =
                    |heap: &mut v12_heap::Heap,
                     key: &str|
                     -> Option<v12_heap::Handle<v12_heap::V12Str>> {
                        let h = heap.intern_text(key);
                        let pk = v12_heap::PropKey::from_string(h);
                        let desc = heap.lookup_property(shape, pk)?;
                        let slot = desc.slot()?;
                        // Ordinary objects store properties at `slot`; the global object biases by INTRINSIC_COUNT.
                        let props = &heap.get(obj).properties;
                        let idx_plain = slot as usize;
                        if let Some(v) = props.get(idx_plain).and_then(|v| v.as_string()) {
                            return Some(v);
                        }
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
                        if msg.is_empty() {
                            return name;
                        }
                        return format!("{name}: {msg}");
                    }
                    // No name — return the message directly (covers Test262Error which
                    // stores only `message`; prefixing with generic "Error" would be noisy).
                    if !msg.is_empty() {
                        return msg;
                    }
                }
                // Empty/missing message (e.g. `new Test262Error()` with no
                // message argument): fall back to `constructor.name` so the
                // runner can classify the throw (it requires "Test262Error").
                if let Some(co) = self.chain_obj_prop(obj, "constructor")
                    && let Some(nh) = self.chain_str_prop(co, "name")
                {
                    let name = self.heap_string_text(nh);
                    if !name.is_empty() {
                        return name;
                    }
                }
            }
            return "[object Object]".to_string();
        }
        "<unprintable>".to_string()
    }

    /// Comma-joined element text of a real array (`undefined`/`null`/holes
    /// render empty, matching `Array.prototype.join`). Nested arrays
    /// recurse; `depth` caps the recursion so cyclic arrays terminate.
    fn array_join_text(
        &mut self,
        obj: v12_heap::Handle<v12_heap::JsObject>,
        depth: usize,
    ) -> String {
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
                parts.push(self.display_value(v, depth + 1));
            }
        }
        parts.join(",")
    }

    /// Decimal text of a BigInt magnitude (little-endian base 256) with the
    /// `n` suffix. Pure computation on a snapshot: no heap access, so no
    /// borrow or GC hazard. Repeated divide-by-10 is O(digits²) but BigInt
    /// display is cold-path diagnostics, not arithmetic.
    fn bigint_text(big: &v12_heap::V12BigInt) -> String {
        let mut cur: Vec<u8> = big.magnitude_le.clone();
        while cur.last() == Some(&0) {
            cur.pop();
        }
        if cur.is_empty() {
            return "0n".to_string();
        }
        let mut digits: Vec<u8> = Vec::new();
        while !cur.is_empty() {
            let mut rem: u16 = 0;
            let mut next: Vec<u8> = Vec::with_capacity(cur.len());
            for &b in cur.iter().rev() {
                let acc = rem * 256 + u16::from(b);
                let q = acc / 10;
                rem = acc % 10;
                if !next.is_empty() || q != 0 {
                    next.push(q as u8);
                }
            }
            next.reverse();
            digits.push(rem as u8);
            cur = next;
        }
        let mut s = String::with_capacity(digits.len() + 2);
        if big.sign {
            s.push('-');
        }
        for d in digits.iter().rev() {
            s.push((b'0' + d) as char);
        }
        s.push('n');
        s
    }

    /// `Map(n) { k => v, … }`: entries live in `elements` as `[k1, v1, …]`
    /// pairs (see `builtins/map.rs`). Keys and values render through the
    /// depth-threaded worker so nested BigInts/collections stay meaningful.
    fn map_text(&mut self, obj: v12_heap::Handle<v12_heap::JsObject>, depth: usize) -> String {
        let entries: Vec<JsValue> = self.heap.get(obj).elements_snapshot();
        let mut parts = Vec::with_capacity(entries.len() / 2);
        for pair in entries.chunks_exact(2) {
            let k = self.display_value(pair[0], depth + 1);
            let v = self.display_value(pair[1], depth + 1);
            parts.push(format!("{k} => {v}"));
        }
        format!("Map({}) {{{}}}", parts.len(), parts.join(", "))
    }

    /// `Set(n) { v, … }`: values live in `elements` (see `builtins/map.rs`).
    fn set_text(&mut self, obj: v12_heap::Handle<v12_heap::JsObject>, depth: usize) -> String {
        let values: Vec<JsValue> = self.heap.get(obj).elements_snapshot();
        let mut parts = Vec::with_capacity(values.len());
        for v in values {
            parts.push(self.display_value(v, depth + 1));
        }
        format!("Set({}) {{{}}}", parts.len(), parts.join(", "))
    }

    /// Promise state rendering: settled payloads render through the worker,
    /// pending promises show no payload (none exists yet). Internal slots
    /// are `properties[0..3] = [state, value, reactions]` (see
    /// `builtins/promise.rs`); a malformed shape falls back to opaque.
    fn promise_text(&mut self, obj: v12_heap::Handle<v12_heap::JsObject>, depth: usize) -> String {
        let slots = self.heap.get(obj).properties.clone();
        let (Some(state), Some(payload)) = (
            slots.first().and_then(|v| v.as_smi()),
            slots.get(1).copied(),
        ) else {
            return "[object Object]".to_string();
        };
        use crate::builtins::promise::{STATE_FULFILLED, STATE_REJECTED};
        if state == STATE_FULFILLED {
            format!("Promise {{ {} }}", self.display_value(payload, depth + 1))
        } else if state == STATE_REJECTED {
            format!(
                "Promise {{ <rejected> {} }}",
                self.display_value(payload, depth + 1)
            )
        } else {
            "Promise { <pending> }".to_string()
        }
    }

    /// `/source/flags` from the RegExp internal slots (`properties =
    /// [source, flags, lastIndex]`, both strings; see `builtins/regexp.rs`).
    /// `None` when the shape is unexpected, letting the caller fall through
    /// to the generic object path.
    fn regexp_text(&mut self, obj: v12_heap::Handle<v12_heap::JsObject>) -> Option<String> {
        let source_h = self.heap.get(obj).properties.first()?.as_string()?;
        let flags_h = self.heap.get(obj).properties.get(1)?.as_string()?;
        let source = self.heap_string_text(source_h);
        let flags = self.heap_string_text(flags_h);
        Some(format!("/{source}/{flags}"))
    }

    /// `[Function: name]` (Node inspect convention) from the closure's own
    /// `name` data property, installed at closure creation (see
    /// `Interp::alloc_closure`). Anonymous closures and intrinsics without an
    /// own name render `[Function (anonymous)]`. Own-shape lookup only: the
    /// prototype chain reaches `Function.prototype`, whose `name` describes
    /// the built-in, not this closure.
    fn function_text(&mut self, obj: v12_heap::Handle<v12_heap::JsObject>) -> String {
        let Some(name_h) = self.own_str_prop(obj, "name") else {
            return "[Function (anonymous)]".to_string();
        };
        let name = self.heap_string_text(name_h);
        if name.is_empty() {
            return "[Function (anonymous)]".to_string();
        }
        format!("[Function: {name}]")
    }

    /// `[Array Iterator]` / `[Map Iterator]` / `[Set Iterator]` from the
    /// iterator's kind slot (`elements[0]`; see `builtins/iterator.rs`). The
    /// state (source, index) stays opaque: it would recurse into the
    /// collection and can form cycles (`arr[Symbol.iterator]` held by `arr`).
    fn iterator_text(&mut self, obj: v12_heap::Handle<v12_heap::JsObject>) -> String {
        use crate::builtins::iterator::{
            ITER_KIND_ARRAY_ENTRIES, ITER_KIND_ARRAY_KEYS, ITER_KIND_ARRAY_VALUES,
            ITER_KIND_MAP_ENTRIES, ITER_KIND_MAP_KEYS, ITER_KIND_MAP_VALUES, ITER_KIND_SET_VALUES,
        };
        let label = match self.heap.get(obj).elements.first().and_then(|v| v.as_smi()) {
            Some(k)
                if k == ITER_KIND_ARRAY_VALUES
                    || k == ITER_KIND_ARRAY_ENTRIES
                    || k == ITER_KIND_ARRAY_KEYS =>
            {
                "Array Iterator"
            }
            Some(k)
                if k == ITER_KIND_MAP_ENTRIES
                    || k == ITER_KIND_MAP_KEYS
                    || k == ITER_KIND_MAP_VALUES =>
            {
                "Map Iterator"
            }
            Some(k) if k == ITER_KIND_SET_VALUES => "Set Iterator",
            _ => "Iterator",
        };
        format!("[{label}]")
    }

    /// Reads own (!) property `key` as a heap string, or `None` when the
    /// object has no such own slot or the slot is not a string. Mirrors the
    /// `properties[slot]` / global-biased `INTRINSIC_COUNT + slot` lookup used
    /// by the message/name fallback above.
    fn own_str_prop(
        &mut self,
        obj: v12_heap::Handle<v12_heap::JsObject>,
        key: &str,
    ) -> Option<v12_heap::Handle<V12Str>> {
        let h = self.heap.intern_text(key);
        let pk = v12_heap::PropKey::from_string(h);
        let shape = self.heap.shape_of(obj);
        let desc = self.heap.lookup_property(shape, pk)?;
        let slot = desc.slot()?;
        let props = &self.heap.get(obj).properties;
        props
            .get(slot as usize)
            .and_then(|v| v.as_string())
            .or_else(|| {
                let idx = crate::realm::INTRINSIC_COUNT + slot as usize;
                props.get(idx).and_then(|v| v.as_string())
            })
    }
}
