//! Iterator built-ins: `%ArrayIteratorPrototype%` and the Array/Map/Set
//! iterator objects behind `Symbol.iterator` (`for-of`, spread, `yield*`).
//!
//! An iterator object is an ordinary object (prototype-linked to
//! `%IteratorPrototype%` where available) carrying its state in the internal
//! `elements` vector: `[kind, source, index]`.
//!
//! - `elements[0]`: iterator kind discriminant (Smi).
//! - `elements[1]`: the source object being iterated (array, map, or set).
//! - `elements[2]`: next index (Smi).
//!
//! `next` is a native function routed through the registry
//! ([`NATIVE_ARRAY_ITERATOR_NEXT`]); each `next` call reads the state, walks
//! the source's elements, and produces an ES iterator result object
//! (`{value, done}`).

use v12_heap::{Handle, Heap, JsObject, JsValue};
use v12_native::Throw;

use super::helpers;

/// Iterator kind: array values (`for (const v of arr)`).
pub const ITER_KIND_ARRAY_VALUES: i32 = 0;
/// Iterator kind: array entries (`for (const [k, v] of arr.entries())`).
pub const ITER_KIND_ARRAY_ENTRIES: i32 = 1;
/// Iterator kind: array keys (`for (const k of arr.keys())`).
pub const ITER_KIND_ARRAY_KEYS: i32 = 2;
/// Iterator kind: Map entries (`for (const [k, v] of map)`).
pub const ITER_KIND_MAP_ENTRIES: i32 = 3;
/// Iterator kind: Map keys.
pub const ITER_KIND_MAP_KEYS: i32 = 4;
/// Iterator kind: Map values.
pub const ITER_KIND_MAP_VALUES: i32 = 5;
/// Iterator kind: Set values (`for (const v of set)`).
pub const ITER_KIND_SET_VALUES: i32 = 6;

/// Index of the state slots inside an iterator's `elements` vector.
const SLOT_KIND: usize = 0;
const SLOT_SOURCE: usize = 1;
const SLOT_INDEX: usize = 2;

/// Builds an iterator object over `source` with the given kind.
fn create_iterator(heap: &mut Heap, source: Handle<JsObject>, kind: i32) -> Handle<JsObject> {
    helpers::alloc_obj(
        heap,
        JsObject {
            kind: v12_heap::Kind::Iterator,
            elements: vec![
                helpers::smi_or_f64(i64::from(kind)),
                JsValue::object(source),
                helpers::smi_or_f64(0),
            ],
            ..JsObject::default()
        },
    )
}

/// Allocates an ES iterator result `{value, done}`.
fn iterator_result(heap: &mut Heap, value: JsValue, done: bool) -> JsValue {
    let h = helpers::alloc_obj(heap, JsObject::default());
    // Shape-driven set: value then done, matching `make_iterator_result`.
    let value_key = heap.intern_string(v12_heap::V12Str::latin1(b"value".to_vec()));
    let done_key = heap.intern_string(v12_heap::V12Str::latin1(b"done".to_vec()));
    let pk_value = v12_heap::PropKey::from_string(value_key);
    let pk_done = v12_heap::PropKey::from_string(done_key);
    let shape0 = heap.root_shape();
    let shape1 = heap.add_property(shape0, pk_value, v12_heap::Attrs::DEFAULT);
    let shape2 = heap.add_property(shape1, pk_done, v12_heap::Attrs::DEFAULT);
    // Bind the shape so the interpreter's `GetProperty` finds `value`/`done`
    // through its shape walk (unbound objects stay on the root shape, which
    // has no descriptors).
    heap.bind_shape(h, shape2);
    let done_val = if done {
        JsValue::true_()
    } else {
        JsValue::false_()
    };
    heap.get_mut(h).properties = smallvec::smallvec![value, done_val];
    heap.get_mut(h).property_keys = smallvec::smallvec![Some(pk_value), Some(pk_done)];
    JsValue::object(h)
}

/// The iterator's source object and next index.
fn state(heap: &Heap, iter: Handle<JsObject>) -> Option<(Handle<JsObject>, usize)> {
    let o = heap.get(iter);
    if o.kind != v12_heap::Kind::Iterator || o.elements.len() < 3 {
        return None;
    }
    let source = o.elements[SLOT_SOURCE].as_object()?;
    let index = o.elements[SLOT_INDEX]
        .as_smi()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    Some((source, index))
}

/// Reads the source's element at `index` (arrays: the elements lattice;
/// maps/sets: the `elements` pair/value vector). Holes read as `undefined`.
fn source_elem(heap: &Heap, source: Handle<JsObject>, index: usize) -> JsValue {
    let o = heap.get(source);
    match o.kind {
        v12_heap::Kind::Array => o
            .elements_array
            .get(index as u32)
            .map(|v| if v.is_hole() { JsValue::undefined() } else { v })
            .unwrap_or(JsValue::undefined()),
        v12_heap::Kind::Map | v12_heap::Kind::Set | v12_heap::Kind::Arguments => o
            .elements
            .get(index)
            .copied()
            .unwrap_or(JsValue::undefined()),
        _ => JsValue::undefined(),
    }
}

/// Source length for iteration bounds.
fn source_len(heap: &Heap, source: Handle<JsObject>) -> usize {
    let o = heap.get(source);
    match o.kind {
        v12_heap::Kind::Array => o.elements_array.len(),
        v12_heap::Kind::Map => o.elements.len() / 2,
        v12_heap::Kind::Set | v12_heap::Kind::Arguments => o.elements.len(),
        _ => 0,
    }
}

/// `Array.prototype.values` / `Map.prototype[Symbol.iterator]` /
/// `Set.prototype[Symbol.iterator]` shared implementation: returns a fresh
/// iterator over `this`.
pub fn iterator_for(heap: &mut Heap, this: JsValue, kind: i32) -> Result<JsValue, Throw> {
    let obj = this
        .as_object()
        .ok_or_else(|| Throw::type_error(heap, "TypeError: value is not iterable"))?;
    Ok(JsValue::object(create_iterator(heap, obj, kind)))
}

/// `iterator.next()` — shared by all four iterator kinds. Advances the
/// internal index and produces `{value, done}`.
pub fn iterator_next(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let iter = helpers::as_object(heap, this, "iterator.next", Some(v12_heap::Kind::Iterator))?;
    let Some((source, index)) = state(heap, iter) else {
        return Err(Throw::type_error(
            heap,
            "iterator.next called on non-iterator",
        ));
    };
    let kind = heap.get(iter).elements[SLOT_KIND]
        .as_smi()
        .unwrap_or(ITER_KIND_ARRAY_VALUES);
    let len = source_len(heap, source);
    if index >= len {
        // Done: mark the iterator exhausted and return {undefined, true}.
        heap.get_mut(iter).elements[SLOT_INDEX] =
            helpers::smi_or_f64(i64::from(v12_heap::JsValue::SMI_MAX));
        return Ok(iterator_result(heap, JsValue::undefined(), true));
    }
    // Advance the index before producing the value: iterator state updates
    // happen on each `next`, even when the value is a pair.
    heap.get_mut(iter).elements[SLOT_INDEX] = helpers::smi_or_f64(index as i64 + 1);
    let value = match kind {
        ITER_KIND_ARRAY_VALUES => source_elem(heap, source, index),
        ITER_KIND_ARRAY_KEYS => helpers::smi_or_f64(index as i64),
        ITER_KIND_ARRAY_ENTRIES | ITER_KIND_MAP_ENTRIES => {
            let key = if kind == ITER_KIND_ARRAY_ENTRIES {
                helpers::smi_or_f64(index as i64)
            } else {
                source_elem(heap, source, 2 * index)
            };
            let val = if kind == ITER_KIND_ARRAY_ENTRIES {
                source_elem(heap, source, index)
            } else {
                source_elem(heap, source, 2 * index + 1)
            };
            // `[key, value]` pair.
            let pair = helpers::alloc_obj(heap, JsObject::array(vec![key, val]));
            JsValue::object(pair)
        }
        ITER_KIND_MAP_KEYS => source_elem(heap, source, 2 * index),
        ITER_KIND_MAP_VALUES => source_elem(heap, source, 2 * index + 1),
        ITER_KIND_SET_VALUES => source_elem(heap, source, index),
        _ => JsValue::undefined(),
    };
    Ok(iterator_result(heap, value, false))
}

/// `%IteratorPrototype%` shared `[Symbol.iterator]()`: returns `this`.
/// The interpreter's `GetIterator` looks up `Symbol.iterator` on the
/// iterator object itself; without this, a `for-of` over an iterator
/// (rather than an iterable) would fail. Satisfies the spec identity
/// `iterator[Symbol.iterator]() === iterator`.
pub fn iterator_self(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let _ = heap;
    Ok(this)
}

/// `Array.prototype[Symbol.iterator]` — values iterator over `this`.
pub fn array_iterator(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    iterator_for(heap, this, ITER_KIND_ARRAY_VALUES)
}

/// `Map.prototype[Symbol.iterator]` — entries iterator over `this`.
pub fn map_iterator(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    iterator_for(heap, this, ITER_KIND_MAP_ENTRIES)
}

/// `Set.prototype[Symbol.iterator]` — values iterator over `this`.
pub fn set_iterator(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    iterator_for(heap, this, ITER_KIND_SET_VALUES)
}

/// `Array.prototype.entries` — entries iterator over `this`.
pub fn array_iterator_entries(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    iterator_for(heap, this, ITER_KIND_ARRAY_ENTRIES)
}

/// `Array.prototype.keys` — keys iterator over `this`.
pub fn array_iterator_keys(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    iterator_for(heap, this, ITER_KIND_ARRAY_KEYS)
}
