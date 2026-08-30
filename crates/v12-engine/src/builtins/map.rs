//! Map and Set built-ins.
//!
//! Minimal but correct v1: entries live in the object's `elements` vec as
//! `[k1, v1, k2, v2, …]` pairs (Map) or `[v1, v2, …]` values (Set), with
//! linear-scan membership (SameValueZero on `JsValue` bits — the engine's
//! identity semantics for keys). O(n) lookups are acceptable at this stage;
//! a hash-backed table replaces this when the value model gains a proper
//! key hash.

use v12_heap::{Heap, JsObject, JsValue};
use v12_native::Throw;

use super::helpers;

/// A Map object: `kind == Kind::Map`, entries in `elements` as key/value pairs.
fn map_entries<'a>(heap: &'a Heap, obj: v12_heap::Handle<JsObject>) -> Option<&'a [JsValue]> {
    if heap.get(obj).kind != v12_heap::Kind::Map {
        return None;
    }
    Some(&heap.get(obj).elements)
}

/// A Set object: `kind == Kind::Set`, entries in `elements` as values.
fn set_entries<'a>(heap: &'a Heap, obj: v12_heap::Handle<JsObject>) -> Option<&'a [JsValue]> {
    if heap.get(obj).kind != v12_heap::Kind::Set {
        return None;
    }
    Some(&heap.get(obj).elements)
}

/// SameValueZero on engine values: bit-identical, with `-0.0` equal to `+0.0`.
fn same_value_zero(a: JsValue, b: JsValue) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y || (x == 0.0 && y == 0.0), // +0 == -0
        _ => a == b,
    }
}

/// `Map(iterable?)` — creates a Map. The iterable is ignored for v1
/// (constructor from entries is a later conformance item).
pub fn map_construct(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::alloc_obj(heap, JsObject {
        kind: v12_heap::Kind::Map,
        ..JsObject::default()
    });
    Ok(JsValue::object(obj))
}

/// `Set(iterable?)` — creates a Set.
pub fn set_construct(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::alloc_obj(heap, JsObject {
        kind: v12_heap::Kind::Set,
        ..JsObject::default()
    });
    Ok(JsValue::object(obj))
}

/// `Map.prototype.get(key)` — the value for `key`, or `undefined`.
pub fn map_get(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Map.prototype.get", Some(v12_heap::Kind::Map))?;
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    for pair in map_entries(heap, obj).unwrap_or_default().chunks_exact(2) {
        if same_value_zero(pair[0], key) {
            return Ok(pair[1]);
        }
    }
    Ok(JsValue::undefined())
}

/// `Map.prototype.set(key, value)` — sets and returns the Map.
pub fn map_set(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Map.prototype.set", Some(v12_heap::Kind::Map))?;
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    let value = args.get(1).copied().unwrap_or_else(JsValue::undefined);
    let entries = &mut heap.get_mut(obj).elements;
    for pair in entries.chunks_exact_mut(2) {
        if same_value_zero(pair[0], key) {
            pair[1] = value;
            return Ok(this);
        }
    }
    entries.push(key);
    entries.push(value);
    Ok(this)
}

/// `Map.prototype.has(key)` — membership.
pub fn map_has(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Map.prototype.has", Some(v12_heap::Kind::Map))?;
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    let found = map_entries(heap, obj)
        .unwrap_or_default()
        .chunks_exact(2)
        .any(|pair| same_value_zero(pair[0], key));
    Ok(if found { JsValue::true_() } else { JsValue::false_() })
}

/// `Map.prototype.delete(key)` — removes and returns `true` if present.
pub fn map_delete(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Map.prototype.delete", Some(v12_heap::Kind::Map))?;
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    let entries = &mut heap.get_mut(obj).elements;
    if let Some(pos) = (0..entries.len() / 2).find(|&i| same_value_zero(entries[2 * i], key)) {
        entries.remove(2 * pos);
        entries.remove(2 * pos);
        return Ok(JsValue::true_());
    }
    Ok(JsValue::false_())
}

/// `Map.prototype.size` getter — number of entries. Wired as a native.
pub fn map_size(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Map.prototype.size", Some(v12_heap::Kind::Map))?;
    let n = (map_entries(heap, obj).unwrap_or_default().len() / 2) as i64;
    Ok(helpers::smi_or_f64(n))
}

/// `Set.prototype.add(value)` — adds and returns the Set.
pub fn set_add(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Set.prototype.add", Some(v12_heap::Kind::Set))?;
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    let entries = &mut heap.get_mut(obj).elements;
    if !entries.iter().any(|&v| same_value_zero(v, value)) {
        entries.push(value);
    }
    Ok(this)
}

/// `Set.prototype.has(value)` — membership.
pub fn set_has(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Set.prototype.has", Some(v12_heap::Kind::Set))?;
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    let found = set_entries(heap, obj)
        .unwrap_or_default()
        .iter()
        .any(|&v| same_value_zero(v, value));
    Ok(if found { JsValue::true_() } else { JsValue::false_() })
}

/// `Set.prototype.delete(value)` — removes and returns `true` if present.
pub fn set_delete(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Set.prototype.delete", Some(v12_heap::Kind::Set))?;
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    let entries = &mut heap.get_mut(obj).elements;
    if let Some(pos) = entries.iter().position(|&v| same_value_zero(v, value)) {
        entries.remove(pos);
        return Ok(JsValue::true_());
    }
    Ok(JsValue::false_())
}

/// `Set.prototype.size` getter — number of values.
pub fn set_size(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Set.prototype.size", Some(v12_heap::Kind::Set))?;
    let n = set_entries(heap, obj).unwrap_or_default().len() as i64;
    Ok(helpers::smi_or_f64(n))
}
