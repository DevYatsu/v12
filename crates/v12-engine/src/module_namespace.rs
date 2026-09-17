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
//! - `[[Extensible]]` is `false` — `FLAG_NOT_EXTENSIBLE`, correct.
//! - Export descriptors — installed with the spec attributes, correct.
//! - `[[OwnPropertyKeys]]` order — keys are defined in sorted order so the
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
//!
//! The loader registers the namespace before evaluating dependencies, so a
//! cyclic or self importer resolves to the same object; exports are populated
//! after the module body completes (a snapshot, not live bindings).

use v12_heap::{Handle, Heap, JsObject, JsValue, PropKey};

use crate::internal_methods::{FullDescriptor, apply_property_descriptor};

/// Allocates an empty namespace object: `[[Prototype]]` null, initially
/// extensible (the loader makes it non-extensible once populated). The
/// object is rooted so dependency evaluation cannot collect it.
pub(crate) fn alloc_namespace(heap: &mut Heap) -> Handle<JsObject> {
    let obj = heap.alloc(JsObject {
        prototype: None,
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

/// Populates `ns` from the compiler epilogue's `exports` object and marks the
/// namespace non-extensible.
///
/// Export keys are defined in ascending string order so the shape descriptor
/// order — hence `[[OwnPropertyKeys]]` — is sorted per ES 10.4.6 step 1.
/// Existing keys are updated in place; `exports` keys that collide with a
/// previously installed key keep the namespace's fixed attributes.
pub(crate) fn populate_namespace(
    heap: &mut Heap,
    ns: Handle<JsObject>,
    exports: Handle<JsObject>,
) {
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
    for (_, key, value) in entries {
        let desc = FullDescriptor {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(false),
            ..FullDescriptor::default()
        };
        let _ = apply_property_descriptor(heap, ns, key, desc);
    }
    // ES 10.4.6: the namespace is born non-extensible. Set after population so
    // the ordinary define path (which rejects new keys on a non-extensible
    // object) can install the exports.
    heap.get_mut(ns).flags |= JsObject::FLAG_NOT_EXTENSIBLE;
}
