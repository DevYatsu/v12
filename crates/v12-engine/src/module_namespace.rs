//! ES 10.4.6 Module Namespace Exotic Object.
//!
//! v1 represents a module namespace as a non-extensible ordinary object with
//! a `null` `[[Prototype]]` and export properties carrying the spec descriptor
//! `{ [[Writable]]: true, [[Enumerable]]: true, [[Configurable]]: false }`.
//!
//! ## Representation and its honest limits
//!
//! The spec makes the namespace a *distinct exotic* object whose `[[Set]]`
//! returns `false` for every key, `[[Get]]` answers only export keys, and
//! `[[OwnPropertyKeys]]` is sorted. The engine's property dispatch has no
//! per-object exotic hook reachable from `v12-engine` (the interpreter owns
//! `get_property`/`set_property` and cannot be edited by this lane), so the
//! representation uses ordinary machinery and therefore does **not** capture
//! every exotic behaviour:
//!
//! - `[[Prototype]]` is `null` — represented directly, observable and correct.
//! - `[[Extensible]]` is `false` — `FLAG_NOT_EXTENSIBLE`, set at allocation so
//!   it holds even while exports are still being initialized, correct.
//! - Export descriptors — installed with the spec attributes, correct.
//! - `[[OwnPropertyKeys]]` order — keys are installed in sorted order so the
//!   shape walk (which `Object.getOwnPropertyNames` reads) reports sorted
//!   export string keys, correct.
//! - `[[Delete]]` — `configurable: false` makes deletes of export keys fail
//!   and deletes of absent keys succeed, matching the exotic.
//! - `[[Set]]` — **gap**: exports are `[[Writable]]: true` per the descriptor
//!   quirk, so an ordinary write to an export key succeeds instead of being
//!   rejected. A correct `[[Set]]` needs an exotic dispatch seam.
//! - `@@toStringTag` — **gap**: the engine has no reachable well-known
//!   `Symbol.toStringTag` singleton to key a stored property against, so the
//!   property is not installed.
//! - Live bindings — **gap**: exports are a post-evaluation snapshot; a module
//!   that reads its own namespace mid-body observes an empty object.
//!
//! The loader registers the namespace before evaluating dependencies, so a
//! cyclic or self importer resolves to the same object; exports are populated
//! after the module body completes.

use v12_heap::{Attrs, Handle, Heap, JsObject, JsValue, PropKey};

/// Allocates an empty namespace object: `[[Prototype]]` null and
/// `[[Extensible]]` false from birth. Rooted so dependency evaluation cannot
/// collect it.
pub(crate) fn alloc_namespace(heap: &mut Heap) -> Handle<JsObject> {
    let obj = heap.alloc(JsObject {
        prototype: None,
        flags: JsObject::FLAG_NOT_EXTENSIBLE,
        ..JsObject::default()
    });
    heap.add_root(JsValue::object(obj));
    obj
}

/// Flattened text of a string `PropKey` (symbol keys return empty).
fn key_text(heap: &mut Heap, key: PropKey) -> String {
    let Some(h) = key.string() else {
        return String::new();
    };
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
        _ => String::new(),
    }
}

/// Installs the module's export keys (sorted, deduped) with `undefined`
/// values, before the body runs.
///
/// ES 10.4.6 defines `[[OwnPropertyKeys]]` as the module's export list, which
/// the linker knows statically. Seeding the keys makes `hasOwnProperty`,
/// `in`, and `getOwnPropertyNames` observe the exports while the body is
/// still executing; values are filled later by [`populate_namespace`]. This
/// does NOT close the live-binding gap: a read during the body sees
/// `undefined`, not the current binding value.
pub(crate) fn seed_export_keys(heap: &mut Heap, ns: Handle<JsObject>, names: &[String]) {
    let mut sorted: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| !n.is_empty() && *n != "*")
        .collect();
    sorted.sort_unstable();
    sorted.dedup();
    let attrs = Attrs::new(true, true, false);
    for name in sorted {
        let key = PropKey::from_string(heap.intern_text(name));
        let shape = heap.shape_of(ns);
        if heap.get(shape).descriptors.find(key).is_some() {
            continue;
        }
        let child = heap.add_property(shape, key, attrs);
        heap.bind_shape(ns, child);
        heap.get_mut(ns).properties.push(JsValue::undefined());
        heap.get_mut(ns).property_keys.push(Some(key));
    }
}

/// Populates `ns` from the compiler epilogue's `exports` object.
///
/// Export keys are installed in ascending string order so the shape descriptor
/// order — hence `[[OwnPropertyKeys]]` — is sorted per ES 10.4.6 step 1.
/// Installation is a direct shape transition (not the ordinary define path)
/// because the namespace is already non-extensible. An existing key's value is
/// overwritten in place; the spec attributes are fixed.
pub(crate) fn populate_namespace(heap: &mut Heap, ns: Handle<JsObject>, exports: Handle<JsObject>) {
    let shape = heap.shape_of(exports);
    // Copy the descriptor list out before any mutable borrow (key_text
    // flattens strings); descriptors are `Copy`.
    let descriptors: Vec<(PropKey, Option<u32>)> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|d| (d.key(), d.slot()))
        .collect();
    let mut entries: Vec<(String, PropKey, JsValue)> = Vec::new();
    for (key, slot) in descriptors {
        if key.is_symbol() {
            continue;
        }
        let Some(slot) = slot else {
            continue;
        };
        let value = heap
            .get(exports)
            .properties
            .get(slot as usize)
            .copied()
            .unwrap_or(JsValue::undefined());
        let text = key_text(heap, key);
        entries.push((text, key, value));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    // `{ [[Writable]]: true, [[Enumerable]]: true, [[Configurable]]: false }`
    // is the spec's report for an export data property (ES 10.4.6 step 5).
    let attrs = Attrs::new(true, true, false);
    for (_, key, value) in entries {
        let shape = heap.shape_of(ns);
        if let Some(desc) = heap.get(shape).descriptors.find(key).copied() {
            // Update in place; attributes stay fixed.
            if let Some(slot) = desc.slot() {
                let o = heap.get_mut(ns);
                if o.properties.len() <= slot as usize {
                    o.properties.resize(slot as usize + 1, JsValue::hole());
                }
                o.properties[slot as usize] = value;
            }
            continue;
        }
        let child = heap.add_property(shape, key, attrs);
        heap.bind_shape(ns, child);
        heap.get_mut(ns).properties.push(value);
        heap.get_mut(ns).property_keys.push(Some(key));
    }
}
