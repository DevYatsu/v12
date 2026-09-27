//! Iterator built-ins: `%ArrayIteratorPrototype%` and the Array/Map/Set
//! iterator objects behind `Symbol.iterator` (`for-of`, spread, `yield*`).
//!
//! An iterator object is an ordinary object carrying its state in the internal
//! `elements` vector: `[kind, source, index]`.
//!
//! - `elements[0]`: iterator kind discriminant (Smi).
//! - `elements[1]`: the source object being iterated (array, map, or set) —
//!   for the two helper-wrapper kinds ([`ITER_KIND_HELPER_TAKE`],
//!   [`ITER_KIND_HELPER_DROP`]) it is a *source iterator* instead, and a
//!   fourth slot carries the limit.
//! - `elements[2]`: next index (Smi) — emitted (`take`) or discarded (`drop`)
//!   values for a wrapper.
//!
//! `next` is a native function routed through the registry
//! ([`NATIVE_ARRAY_ITERATOR_NEXT`]); each `next` call reads the state, walks
//! the source's elements (or pulls from the source iterator, for a wrapper),
//! and produces an ES iterator result object (`{value, done}`).
//!
//! The ES2025 iterator helpers split across two homes. `Iterator.from`,
//! `take`, `drop` and `toArray` are pure-heap and live here: `take`/`drop`
//! build a lazy wrapper and consume nothing, `toArray` drains. The eight
//! helpers that take a user callback (`map`/`filter`/`flatMap`/`reduce`/
//! `forEach`/`some`/`every`/`find`) cannot be — calling JS needs the machine —
//! so their bodies are at the interpreter seam
//! (`Interp::run_iterator_callback`) and this file only installs them.
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
/// Iterator kind: Set entries (`[value, value]` pairs).
pub const ITER_KIND_SET_ENTRIES: i32 = 7;
/// Iterator kind: a helper wrapper that emits at most `limit` values pulled
/// from its source iterator (`Iterator.prototype.take`). `elements` is
/// `[kind, source iterator, values emitted, limit]`.
pub const ITER_KIND_HELPER_TAKE: i32 = 8;
/// Iterator kind: a helper wrapper that discards the first `limit` values
/// pulled from its source iterator (`Iterator.prototype.drop`). Same
/// `elements` layout as [`ITER_KIND_HELPER_TAKE`]; the counter at
/// [`SLOT_INDEX`] counts *discarded* values.
pub const ITER_KIND_HELPER_DROP: i32 = 9;

/// True for the two lazily-pulling helper wrapper kinds.
const fn is_helper_wrapper(kind: i32) -> bool {
    kind == ITER_KIND_HELPER_TAKE || kind == ITER_KIND_HELPER_DROP
}

/// Index of the state slots inside an iterator's `elements` vector.
const SLOT_KIND: usize = 0;
const SLOT_SOURCE: usize = 1;
const SLOT_INDEX: usize = 2;
/// Fourth slot, present only on helper wrappers: the `take`/`drop` limit.
const SLOT_LIMIT: usize = 3;

/// Upper bound on a single eager drain, so a runaway iterator reports a
/// `RangeError` instead of exhausting memory.
const MAX_DRAIN: usize = 1_000_000;

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

/// Builds a lazy helper wrapper: an iterator that pulls from `source` (itself
/// any v12 iterator, wrappers included) instead of scanning a source object's
/// element store. Nothing is consumed until the wrapper's `next` runs, which
/// is what makes `take`/`drop` lazy per spec 27.1.4.
fn create_helper_iterator(
    ctx: &mut Ctx,
    source: Handle<JsObject>,
    kind: i32,
    limit: usize,
) -> JsValue {
    ctx.alloc_obj(JsObject {
        kind: v12_heap::Kind::Iterator,
        elements: vec![
            helpers::smi_or_f64(i64::from(kind)),
            JsValue::object(source),
            helpers::smi_or_f64(0),
            helpers::smi_or_f64(limit as i64),
        ],
        ..JsObject::default()
    })
    .into()
}

/// The one generic engine behind every helper: a single `next` pull against
/// any v12 iterator, through the same `iterator_next` the JS `next` method
/// uses. `None` means the source is exhausted. Because it recurses through
/// [`iterator_next`] rather than reading the source's element store, a helper
/// composed on a helper (`it.take(1).drop(1)`) works unchanged.
fn pull(ctx: &mut Ctx, iter: JsValue) -> Result<Option<JsValue>, Throw> {
    let result = iterator_next(ctx, iter, &[])?;
    let Some(obj) = result.as_object() else {
        return Err(ctx.type_error("TypeError: iterator next() did not return an object"));
    };
    let props = ctx.heap.get(obj).properties.clone();
    if props
        .get(1)
        .copied()
        .unwrap_or(JsValue::undefined())
        .is_true()
    {
        return Ok(None);
    }
    Ok(Some(props.first().copied().unwrap_or(JsValue::undefined())))
}

/// `next` for the two lazy helper wrapper kinds: `take` stops after `limit`
/// emissions, `drop` discards `limit` values first. Both delegate every real
/// read to the shared pull, so an underlying iterator is advanced only as far
/// as the wrapper actually needs.
fn helper_wrapper_next(
    ctx: &mut Ctx,
    iter: Handle<JsObject>,
    kind: i32,
    count: usize,
    limit: usize,
    source: JsValue,
) -> Result<JsValue, Throw> {
    let mut count = count;
    if kind == ITER_KIND_HELPER_TAKE {
        if count >= limit {
            return Ok(iterator_result(ctx, JsValue::undefined(), true));
        }
    } else {
        while count < limit {
            if pull(ctx, source)?.is_none() {
                return Ok(iterator_result(ctx, JsValue::undefined(), true));
            }
            count += 1;
        }
        // The discards are done: the wrapper now passes the source through, so
        // its counter stops moving and a later `next` resumes here.
        ctx.heap.get_mut(iter).elements[SLOT_INDEX] = helpers::smi_or_f64(count as i64);
    }
    match pull(ctx, source)? {
        Some(value) => {
            if kind == ITER_KIND_HELPER_TAKE {
                ctx.heap.get_mut(iter).elements[SLOT_INDEX] = helpers::smi_or_f64(count as i64 + 1);
            }
            Ok(iterator_result(ctx, value, false))
        }
        None => Ok(iterator_result(ctx, JsValue::undefined(), true)),
    }
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
    let receiver_kind = ctx.heap.get(obj).kind;
    let valid = match kind {
        ITER_KIND_MAP_ENTRIES | ITER_KIND_MAP_KEYS | ITER_KIND_MAP_VALUES => {
            receiver_kind == v12_heap::Kind::Map
        }
        ITER_KIND_SET_VALUES | ITER_KIND_SET_ENTRIES => receiver_kind == v12_heap::Kind::Set,
        ITER_KIND_ARRAY_VALUES | ITER_KIND_ARRAY_ENTRIES | ITER_KIND_ARRAY_KEYS => {
            matches!(
                receiver_kind,
                v12_heap::Kind::Array | v12_heap::Kind::Arguments
            )
        }
        _ => false,
    };
    if !valid {
        return Err(ctx.type_error("TypeError: iterator called on incompatible receiver"));
    }
    Ok(JsValue::object(create_iterator(ctx, obj, kind)))
}

/// `Iterator( )` (spec 27.1.2.1). The interpreter presents the constructor
/// itself as `this` for `new Iterator()` and `undefined` for a plain call, so
/// the receiver *is* the `NewTarget` test the spec opens with: a plain call is
/// a `TypeError`, a construct returns a fresh object inheriting
/// `Iterator.prototype` (which is what makes `class S extends Iterator {}`
/// constructible).
pub fn iterator_construct(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(ctor) = this.as_object() else {
        return Err(ctx.type_error("TypeError: Iterator requires new"));
    };
    let proto = ctx.heap.get(ctor).prototype;
    let obj = ctx.alloc_obj(JsObject {
        prototype: proto,
        ..JsObject::default()
    });
    Ok(JsValue::object(obj))
}

/// Brand check shared by every `Iterator.prototype` helper: the receiver must
/// be one of v12's iterator objects. The spec only requires "an object with a
/// `next` method", but v1's iterator surface is the concrete `Kind::Iterator`
/// (the same gate `run_iterator_callback` applies on the interpreter seam), so
/// one check here keeps the whole helper family consistent.
fn require_iterator(ctx: &mut Ctx, this: JsValue, name: &str) -> Result<Handle<JsObject>, Throw> {
    ctx.this_object(this, name, Some(v12_heap::Kind::Iterator))
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
    // Helper wrappers (`take`/`drop`) carry a fourth slot and pull from the
    // iterator in `elements[1]` instead of scanning an element store. No
    // exhaustion flag is needed: a `take` past its limit and a `drop` over a
    // drained source both keep answering `done` on every later `next`, which
    // is the spec's requirement for a completed iterator.
    if is_helper_wrapper(kind) {
        let limit = ctx
            .heap
            .get(iter)
            .elements
            .get(SLOT_LIMIT)
            .and_then(|v| v.as_smi())
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        return helper_wrapper_next(ctx, iter, kind, index, limit, JsValue::object(source));
    }
    let mut index = index;
    loop {
        let len = source_len(ctx.heap, source);
        if index >= len {
            // Done: mark the iterator exhausted and return {undefined, true}.
            ctx.heap.get_mut(iter).elements[SLOT_INDEX] =
                helpers::smi_or_f64(i64::from(v12_heap::JsValue::SMI_MAX));
            return Ok(iterator_result(ctx, JsValue::undefined(), true));
        }
        let tombstoned = match kind {
            ITER_KIND_MAP_ENTRIES | ITER_KIND_MAP_KEYS | ITER_KIND_MAP_VALUES => {
                source_elem(ctx.heap, source, 2 * index).is_hole()
            }
            ITER_KIND_SET_VALUES | ITER_KIND_SET_ENTRIES => {
                source_elem(ctx.heap, source, index).is_hole()
            }
            _ => false,
        };
        if tombstoned {
            index += 1;
            ctx.heap.get_mut(iter).elements[SLOT_INDEX] = helpers::smi_or_f64(index as i64);
            continue;
        }
        // Advance the index before producing the value: iterator state updates
        // happen on each `next`, even when the value is a pair.
        ctx.heap.get_mut(iter).elements[SLOT_INDEX] = helpers::smi_or_f64(index as i64 + 1);
        let value = match kind {
            ITER_KIND_ARRAY_VALUES => source_elem(ctx.heap, source, index),
            ITER_KIND_ARRAY_KEYS => helpers::smi_or_f64(index as i64),
            ITER_KIND_ARRAY_ENTRIES | ITER_KIND_MAP_ENTRIES => {
                let (key, val) = if kind == ITER_KIND_ARRAY_ENTRIES {
                    (
                        helpers::smi_or_f64(index as i64),
                        source_elem(ctx.heap, source, index),
                    )
                } else {
                    (
                        source_elem(ctx.heap, source, 2 * index),
                        source_elem(ctx.heap, source, 2 * index + 1),
                    )
                };
                JsValue::object(ctx.alloc_array(vec![key, val]))
            }
            ITER_KIND_MAP_KEYS => source_elem(ctx.heap, source, 2 * index),
            ITER_KIND_MAP_VALUES => source_elem(ctx.heap, source, 2 * index + 1),
            ITER_KIND_SET_VALUES => source_elem(ctx.heap, source, index),
            ITER_KIND_SET_ENTRIES => {
                let value = source_elem(ctx.heap, source, index);
                JsValue::object(ctx.alloc_array(vec![value, value]))
            }
            _ => JsValue::undefined(),
        };
        return Ok(iterator_result(ctx, value, false));
    }
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

/// Eagerly drains `this` through the shared pull into a `Vec`. Used by the
/// helpers whose spec algorithm consumes the whole iterator at once
/// (`toArray`); the lazy helpers return a wrapper instead.
fn drain_iterator(ctx: &mut Ctx, this: JsValue) -> Result<Vec<JsValue>, Throw> {
    let mut out = Vec::new();
    while let Some(v) = pull(ctx, this)? {
        out.push(v);
        if out.len() > MAX_DRAIN {
            return Err(ctx.range_error("RangeError: iterator drain exceeds 1,000,000 values"));
        }
    }
    Ok(out)
}

/// `Iterator.prototype.toArray()` — drains `this` into an array.
pub fn iterator_to_array(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    require_iterator(ctx, this, "Iterator.prototype.toArray")?;
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
    n.trunc().min(MAX_DRAIN as f64) as usize
}

/// `Iterator.prototype.take(limit)` — a lazy iterator over the first `limit`
/// values of `this`. Nothing is pulled until the result is consumed, and no
/// more than `limit` values are ever pulled from `this`.
pub fn iterator_take(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let source = require_iterator(ctx, this, "Iterator.prototype.take")?;
    let limit = take_limit(ctx, args);
    Ok(create_helper_iterator(
        ctx,
        source,
        ITER_KIND_HELPER_TAKE,
        limit,
    ))
}

/// `Iterator.prototype.drop(limit)` — a lazy iterator over everything after
/// the first `limit` values. The discards happen on the first pull, not here.
pub fn iterator_drop(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let source = require_iterator(ctx, this, "Iterator.prototype.drop")?;
    let limit = take_limit(ctx, args);
    Ok(create_helper_iterator(
        ctx,
        source,
        ITER_KIND_HELPER_DROP,
        limit,
    ))
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
