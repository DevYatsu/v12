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
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`. Callback-taking methods stay at the interpreter seam
//! (`Interp::run_callback_builtin`) and are untouched here.

use v12_heap::{Handle, Heap, JsObject, JsValue};
use v12_native::Throw;

use super::ctx::Ctx;
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
fn create_iterator(ctx: &mut Ctx, source: Handle<JsObject>, kind: i32) -> Handle<JsObject> {
    ctx.alloc_obj(JsObject {
        kind: v12_heap::Kind::Iterator,
        elements: vec![
            helpers::smi_or_f64(i64::from(kind)),
            JsValue::object(source),
            helpers::smi_or_f64(0),
        ],
        ..JsObject::default()
    })
}

/// Allocates an ES iterator result `{value, done}`.
fn iterator_result(ctx: &mut Ctx, value: JsValue, done: bool) -> JsValue {
    let heap = &mut *ctx.heap;
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
    let done_val = JsValue::from_bool(done);
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
pub fn iterator_for(ctx: &mut Ctx, this: JsValue, kind: i32) -> Result<JsValue, Throw> {
    let obj = this
        .as_object()
        .ok_or_else(|| ctx.type_error("TypeError: value is not iterable"))?;
    Ok(JsValue::object(create_iterator(ctx, obj, kind)))
}

/// `iterator.next()` — shared by all four iterator kinds. Advances the
/// internal index and produces `{value, done}`.
pub fn iterator_next(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let iter = ctx.this_object(this, "iterator.next", Some(v12_heap::Kind::Iterator))?;
    let Some((source, index)) = state(ctx.heap, iter) else {
        return Err(ctx.type_error("iterator.next called on non-iterator"));
    };
    let kind = ctx.heap.get(iter).elements[SLOT_KIND]
        .as_smi()
        .unwrap_or(ITER_KIND_ARRAY_VALUES);
    let len = source_len(ctx.heap, source);
    if index >= len {
        // Done: mark the iterator exhausted and return {undefined, true}.
        ctx.heap.get_mut(iter).elements[SLOT_INDEX] =
            helpers::smi_or_f64(i64::from(v12_heap::JsValue::SMI_MAX));
        return Ok(iterator_result(ctx, JsValue::undefined(), true));
    }
    // Advance the index before producing the value: iterator state updates
    // happen on each `next`, even when the value is a pair.
    ctx.heap.get_mut(iter).elements[SLOT_INDEX] = helpers::smi_or_f64(index as i64 + 1);
    let heap = &mut *ctx.heap;
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
    Ok(iterator_result(ctx, value, false))
}

/// `%IteratorPrototype%` shared `[Symbol.iterator]()`: returns `this`.
/// The interpreter's `GetIterator` looks up `Symbol.iterator` on the
/// iterator object itself; without this, a `for-of` over an iterator
/// (rather than an iterable) would fail. Satisfies the spec identity
/// `iterator[Symbol.iterator]() === iterator`.
pub fn iterator_self(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let _ = ctx;
    Ok(this)
}

/// `Array.prototype[Symbol.iterator]` — values iterator over `this`.
pub fn array_iterator(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    iterator_for(ctx, this, ITER_KIND_ARRAY_VALUES)
}

/// `Map.prototype[Symbol.iterator]` — entries iterator over `this`.
pub fn map_iterator(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    iterator_for(ctx, this, ITER_KIND_MAP_ENTRIES)
}

/// `Set.prototype[Symbol.iterator]` — values iterator over `this`.
pub fn set_iterator(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    iterator_for(ctx, this, ITER_KIND_SET_VALUES)
}

/// `Array.prototype.entries` — entries iterator over `this`.
pub fn array_iterator_entries(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    iterator_for(ctx, this, ITER_KIND_ARRAY_ENTRIES)
}

/// `Array.prototype.keys` — keys iterator over `this`.
pub fn array_iterator_keys(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    iterator_for(ctx, this, ITER_KIND_ARRAY_KEYS)
}

/// Collects all remaining `next()` values of `this` iterator into a Vec.
fn drain_iterator(ctx: &mut Ctx, this: JsValue) -> Result<Vec<JsValue>, Throw> {
    let mut out = Vec::new();
    loop {
        let r = iterator_next(ctx, this, &[])?;
        let Some(o) = r.as_object() else { break };
        let done = ctx
            .heap
            .get(o)
            .properties
            .get(1)
            .copied()
            .unwrap_or(JsValue::undefined());
        if done.is_true() {
            break;
        }
        let v = ctx
            .heap
            .get(o)
            .properties
            .first()
            .copied()
            .unwrap_or(JsValue::undefined());
        out.push(v);
        if out.len() > 1_000_000 {
            break;
        }
    }
    Ok(out)
}

/// Builds an array iterator (kind values) over `values`.
fn array_values_iterator(ctx: &mut Ctx, values: Vec<JsValue>) -> Result<JsValue, Throw> {
    let arr = ctx.alloc_obj(JsObject::array(values));
    let h = arr;
    Ok(JsValue::object(create_iterator(ctx, h, ITER_KIND_ARRAY_VALUES)))
}

/// `Iterator.prototype.toArray()` — drains `this` into an array.
pub fn iterator_to_array(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let _ = ctx.this_object(this, "Iterator.prototype.toArray", Some(v12_heap::Kind::Iterator))?;
    let values = drain_iterator(ctx, this)?;
    let arr = ctx.alloc_obj(JsObject::array(values));
    Ok(JsValue::object(arr))
}

/// Parses the `limit` argument via `ctx.to_number` (spec `ToNumber`);
/// non-finite/negative values clamp to 0.
fn take_limit(ctx: &mut Ctx, args: &[JsValue]) -> usize {
    let Some(&v) = args.first() else { return 0 };
    let n = ctx.to_number(v);
    if !n.is_finite() || n <= 0.0 {
        return 0;
    }
    n.trunc().min(1_000_000.0) as usize
}

/// `Iterator.prototype.take(limit)` — first `limit` values as an iterator.
pub fn iterator_take(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let _ = ctx.this_object(this, "Iterator.prototype.take", Some(v12_heap::Kind::Iterator))?;
    let limit = take_limit(ctx, args);
    let mut values = drain_iterator(ctx, this)?;
    values.truncate(limit.min(values.len()));
    array_values_iterator(ctx, values)
}

/// `Iterator.prototype.drop(limit)` — values after the first `limit`.
pub fn iterator_drop(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let _ = ctx.this_object(this, "Iterator.prototype.drop", Some(v12_heap::Kind::Iterator))?;
    let limit = take_limit(ctx, args);
    let values = drain_iterator(ctx, this)?;
    let rest = if limit < values.len() { values[limit..].to_vec() } else { Vec::new() };
    array_values_iterator(ctx, rest)
}

/// `Iterator.from(value)` — if already an iterator return it, else wrap an
/// array-like's values (v1 subset).
pub fn iterator_from(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    if let Some(o) = v.as_object()
        && ctx.heap.get(o).kind == v12_heap::Kind::Iterator
    {
        return Ok(v);
    }
    iterator_for(ctx, v, ITER_KIND_ARRAY_VALUES)
}
