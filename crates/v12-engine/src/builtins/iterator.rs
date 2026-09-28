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
//! [`drain_iterable`] is the same pure-heap idea for the *consumer* side: the
//! generic `GetIterator`/`IteratorStep` a `&mut Heap` native can afford. Every
//! method that accepts an arbitrary iterable argument (the `Set.prototype`
//! relational methods) goes through it, so the recognised set of iterable kinds
//! is written down once, here, instead of per method.
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
/// from its source iterator (`Iterator.prototype.take`).
pub const ITER_KIND_HELPER_TAKE: i32 = 8;
/// Iterator kind: a helper wrapper that discards the first `limit` values
/// pulled from its source iterator (`Iterator.prototype.drop`).
pub const ITER_KIND_HELPER_DROP: i32 = 9;
/// Iterator kind: the lazy `map` wrapper.
pub const ITER_KIND_HELPER_MAP: i32 = 10;
/// Iterator kind: the lazy `filter` wrapper.
pub const ITER_KIND_HELPER_FILTER: i32 = 11;
/// Iterator kind: the lazy `flatMap` wrapper.
pub const ITER_KIND_HELPER_FLATMAP: i32 = 12;

/// Every kind at or below this discriminant is an *element-store* iterator:
/// one whose `next` scans the source object's element vector with no user code
/// and no machine. A helper wrapper over one of those can be stepped by this
/// file alone; anything else must be driven by the interpreter, which stores
/// the source's `next` method in the wrapper so it can call back into the
/// machine. This bound is the single source the interpreter's
/// `helper_layout::ENGINE_KINDS` mirror is checked against.
pub const ENGINE_KIND_MAX: i32 = ITER_KIND_HELPER_TAKE - 1;

/// True for a `take`/`drop` wrapper.
pub const fn is_limit_wrapper(kind: i32) -> bool {
    kind == ITER_KIND_HELPER_TAKE || kind == ITER_KIND_HELPER_DROP
}

/// True for a `map`/`filter`/`flatMap` wrapper: the three helpers that hold a
/// user callback, and so cannot be stepped without the machine.
pub const fn is_callback_wrapper(kind: i32) -> bool {
    matches!(
        kind,
        ITER_KIND_HELPER_MAP | ITER_KIND_HELPER_FILTER | ITER_KIND_HELPER_FLATMAP
    )
}

/// True for any of the five lazy helper wrappers.
pub const fn is_helper_wrapper(kind: i32) -> bool {
    is_limit_wrapper(kind) || is_callback_wrapper(kind)
}

/// What a helper wrapper's `next` must do, given its counter and (for
/// `take`/`drop`) its limit. The *decision* lives here so the two executors —
/// this file's machine-free pull and `v12-interp`'s machine-capable step — can
/// never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperStep {
    /// Report `done`: the limit is reached, or the source ran out during the
    /// `drop` phase.
    Done,
    /// Discard this many more source values before emitting (the `drop`
    /// phase). `0` means the discards are finished and this step emits.
    Skip(usize),
    /// Pull one source value and emit it.
    Emit,
}

/// The wrapper state machine, shared by both executors.
pub const fn wrapper_step(kind: i32, count: usize, limit: usize) -> WrapperStep {
    if kind == ITER_KIND_HELPER_TAKE {
        if count >= limit {
            return WrapperStep::Done;
        }
    } else if kind == ITER_KIND_HELPER_DROP {
        if count < limit {
            return WrapperStep::Skip(limit - count);
        }
    }
    WrapperStep::Emit
}

/// Index of the state slots inside an iterator's `elements` vector.
///
/// Every helper wrapper uses the same fixed seven slots:
///
/// - `[0]` kind discriminant (Smi)
/// - `[1]` the source iterator object
/// - `[2]` the counter: values emitted (`take`), values discarded (`drop`),
///   or values pulled from the source (`map`/`filter`/`flatMap`)
/// - `[3]` the source's `next` method, resolved once when the wrapper was
///   built — or `undefined` when the whole chain is machine-free and this
///   file steps it through [`pull`]
/// - `[4]` the wrapper's argument: the `limit` for `take`/`drop`, the callback
///   for `map`/`filter`/`flatMap`
/// - `[5]` `flatMap` only: the array whose elements are still to be yielded
/// - `[6]` `flatMap` only: the cursor into slot `[5]`
pub const SLOT_KIND: usize = 0;
pub const SLOT_SOURCE: usize = 1;
pub const SLOT_INDEX: usize = 2;
pub const SLOT_SOURCE_NEXT: usize = 3;
pub const SLOT_ARG: usize = 4;
pub const SLOT_PENDING: usize = 5;
pub const SLOT_PENDING_INDEX: usize = 6;

/// Number of slots every helper wrapper carries. Two of them are `flatMap`-only
/// (the pending array and its cursor); the rest are `undefined` for the kinds
/// that do not use them, so the vector is one fixed size for every wrapper.
pub const WRAPPER_SLOTS: usize = SLOT_PENDING_INDEX + 1;

/// Upper bound on a single eager drain, so a runaway iterator reports a
/// `RangeError` instead of exhausting memory.
pub const MAX_DRAIN: usize = 1_000_000;

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

/// Builds a lazy helper wrapper: an iterator that pulls from `source` (any
/// iterator) instead of scanning a source object's element store. Nothing is
/// consumed until the wrapper's `next` runs, which is what makes the helpers
/// lazy per spec 27.1.4.
///
/// `source_next` is the source's `next` method, resolved once by the caller.
/// Pass [`JsValue::undefined`] when the whole chain is machine-free, in which
/// case this file steps the wrapper through [`pull`]; pass the real method when
/// the chain needs the machine, in which case the interpreter intercepts
/// `next` and drives it. `arg` is the `limit` (take/drop) or the callback
/// (map/filter/flatMap).
pub fn create_helper_iterator(
    ctx: &mut Ctx,
    source: JsValue,
    kind: i32,
    arg: JsValue,
    source_next: JsValue,
) -> JsValue {
    let mut elements = vec![
        helpers::smi_or_f64(i64::from(kind)),
        source,
        helpers::smi_or_f64(0),
        source_next,
        arg,
    ];
    elements.resize(WRAPPER_SLOTS, JsValue::undefined());
    let obj = ctx.alloc_obj(JsObject {
        kind: v12_heap::Kind::Iterator,
        elements,
        ..JsObject::default()
    });
    JsValue::object(obj)
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

/// `next` for a `take`/`drop` wrapper whose chain is machine-free, so every
/// real read goes through [`pull`]. The control flow comes from
/// [`wrapper_step`], shared with the interpreter's machine-capable executor;
/// a callback wrapper is rejected here because running a callback needs the
/// machine, which this signature cannot reach.
fn helper_wrapper_next(
    ctx: &mut Ctx,
    iter: Handle<JsObject>,
    kind: i32,
    count: usize,
    limit: usize,
    source: JsValue,
) -> Result<JsValue, Throw> {
    if is_callback_wrapper(kind) {
        return Err(
            ctx.type_error("TypeError: this iterator helper must be driven by the interpreter")
        );
    }
    let mut count = count;
    loop {
        match wrapper_step(kind, count, limit) {
            WrapperStep::Done => return Ok(iterator_result(ctx, JsValue::undefined(), true)),
            WrapperStep::Skip(0) => break,
            WrapperStep::Skip(n) => {
                for _ in 0..n {
                    if pull(ctx, source)?.is_none() {
                        return Ok(iterator_result(ctx, JsValue::undefined(), true));
                    }
                    count += 1;
                }
                // The discards are done: the wrapper now passes the source
                // through, so its counter stops moving and a later `next`
                // resumes here.
                ctx.heap.get_mut(iter).elements[SLOT_INDEX] = helpers::smi_or_f64(count as i64);
            }
            WrapperStep::Emit => break,
        }
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

/// Brand check shared by every `Iterator.prototype` helper: spec 27.1.4
/// step 1-3 only requires an Object receiver, because the `next` method lookup
/// that follows is what rejects a non-iterator. Since the interpreter now owns
/// the `GetMethod(O, "next")` gate (it has to — the method may be a getter on
/// a user object), this side only enforces object-ness, which is what lets
/// `take`/`drop` wrap a generator or a plain `{next(){}}` object.
fn require_iterator(ctx: &mut Ctx, this: JsValue, name: &str) -> Result<Handle<JsObject>, Throw> {
    this.as_object()
        .ok_or_else(|| ctx.type_error(format!("TypeError: {name} called on a non-object")))
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
    // A helper wrapper pulls from the iterator in `elements[1]`, and its
    // `limit`/`callback` argument lives in `elements[4]`. No exhaustion flag is
    // needed: a `take` past its limit and a `drop` over a drained source both
    // keep answering `done` on every later `next`, which is the spec's
    // requirement for a completed iterator.
    if is_helper_wrapper(kind) {
        let limit = if is_limit_wrapper(kind) {
            wrapper_limit(ctx, ctx.heap.get(iter).elements[SLOT_ARG])
        } else {
            0
        };
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
///
/// The array goes through `Ctx::alloc_array`, so it carries `%Array.prototype%`
/// and a `length` own property. Hand-rolling it with `JsObject::array` +
/// `alloc_obj` (what this did before) leaves the object on the root shape with a
/// `null` prototype, which fails `result.constructor === Array` and
/// `result.map(…)` far away from the real cause.
pub fn iterator_to_array(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    require_iterator(ctx, this, "Iterator.prototype.toArray")?;
    let values = drain_iterator(ctx, this)?;
    Ok(JsValue::object(ctx.alloc_array(values)))
}

/// The stored `limit` of a `take`/`drop` wrapper, read back through
/// `ToNumber` on every `next` (the value was already coerced once at call time
/// by [`take_limit`]; re-reading the stored Smi is a no-op conversion and keeps
/// the slot type uniform).
fn wrapper_limit(ctx: &mut Ctx, arg: JsValue) -> usize {
    let n = ctx.to_number(arg);
    if !n.is_finite() || n <= 0.0 {
        return 0;
    }
    n.trunc().min(MAX_DRAIN as f64) as usize
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
///
/// `args[0]` is the raw `limit` (spec 27.1.4.3 step 2 `ToNumber` runs here, at
/// call time, not at the first pull) and `args[1]` is the source's `next`
/// method as resolved by the interpreter, or `undefined` when the chain is
/// machine-free and this file will step the wrapper itself.
pub fn iterator_take(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let source = require_iterator(ctx, this, "Iterator.prototype.take")?;
    let limit = take_limit(ctx, args);
    let source_next = args.get(1).copied().unwrap_or(JsValue::undefined());
    Ok(create_helper_iterator(
        ctx,
        JsValue::object(source),
        ITER_KIND_HELPER_TAKE,
        helpers::smi_or_f64(limit as i64),
        source_next,
    ))
}

/// `Iterator.prototype.drop(limit)` — a lazy iterator over everything after
/// the first `limit` values. The discards happen on the first pull, not here.
/// `args` is shaped exactly as for [`iterator_take`].
pub fn iterator_drop(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let source = require_iterator(ctx, this, "Iterator.prototype.drop")?;
    let limit = take_limit(ctx, args);
    let source_next = args.get(1).copied().unwrap_or(JsValue::undefined());
    Ok(create_helper_iterator(
        ctx,
        JsValue::object(source),
        ITER_KIND_HELPER_DROP,
        helpers::smi_or_f64(limit as i64),
        source_next,
    ))
}

/// Builds one of the three lazy callback wrappers (`map`/`filter`/`flatMap`).
///
/// These are pure allocation: the callback is *stored*, never called, so the
/// wrapper is returned before any value is pulled (spec 27.1.4.5 step 6). The
/// interpreter owns the `next` half, because calling the callback needs the
/// machine.
///
/// `args[0]` is the callback, `args[1]` the source's `next` method as resolved
/// once by the interpreter.
pub fn create_callback_iterator(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
    kind: i32,
) -> Result<JsValue, Throw> {
    let _ = require_iterator(ctx, this, "Iterator.prototype helper")?;
    let callback = args.first().copied().unwrap_or(JsValue::undefined());
    let source_next = args.get(1).copied().unwrap_or(JsValue::undefined());
    Ok(create_helper_iterator(
        ctx,
        this,
        kind,
        callback,
        source_next,
    ))
}

/// `Iterator.prototype.map(mapper)` — the wrapper builder; see
/// [`create_callback_iterator`].
pub fn iterator_map(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    create_callback_iterator(ctx, this, args, ITER_KIND_HELPER_MAP)
}

/// `Iterator.prototype.filter(predicate)` — see [`create_callback_iterator`].
pub fn iterator_filter(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    create_callback_iterator(ctx, this, args, ITER_KIND_HELPER_FILTER)
}

/// `Iterator.prototype.flatMap(mapper)` — see [`create_callback_iterator`].
pub fn iterator_flatmap(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    create_callback_iterator(ctx, this, args, ITER_KIND_HELPER_FLATMAP)
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

// ---------------------------------------------------------------------------
// Structural iterable drain
// ---------------------------------------------------------------------------

/// The values `v` yields when iterated, in iteration order, or `None` when
/// `v` is not a kind this engine can iterate without the machine.
///
/// This is the whole of `GetIterator`/`IteratorStep` for a `&mut Heap` native.
/// The recognised set is deliberately narrow and is exactly the set whose
/// iteration needs no user code:
///
/// - `Array` / `Arguments` — the element lattice, holes as `undefined` (which
///   is what `Array.prototype[Symbol.iterator]` yields);
/// - `Set` — its value store;
/// - `Map` — entry *pairs*, built as real arrays because a `Map` iterates
///   `[key, value]` arrays, not a flattened two-slot record;
/// - `Iterator` — the engine's own iterator objects, through [`drain_iterator`]
///   (so a helper wrapper composes: `a.take(2).values()` drains);
/// - a string primitive — by Unicode code point, per `String.prototype
///   [Symbol.iterator]`'s surrogate-pair handling, so an astral character is
///   one value rather than two lone surrogates.
///
/// A user-defined `@@iterator` (a generator, a hand-rolled `next`) is **not**
/// reachable from here: calling its `next` is machine work, which is what the
/// interpreter seam in `v12-interp/src/call_setup.rs` exists for. Callers must
/// treat `None` as "not iterable" and say so, rather than silently yielding
/// nothing.
pub fn drain_iterable(ctx: &mut Ctx, v: JsValue) -> Result<Option<Vec<JsValue>>, Throw> {
    if let Some(obj) = v.as_object() {
        return match ctx.heap.get(obj).kind {
            v12_heap::Kind::Array | v12_heap::Kind::Arguments => {
                Ok(Some(ctx.heap.get(obj).elements_snapshot()))
            }
            v12_heap::Kind::Set => Ok(Some(
                ctx.heap
                    .get(obj)
                    .elements
                    .iter()
                    .copied()
                    .filter(|e| !e.is_hole())
                    .collect(),
            )),
            v12_heap::Kind::Map => {
                let pairs: Vec<[JsValue; 2]> = ctx
                    .heap
                    .get(obj)
                    .elements
                    .chunks_exact(2)
                    .map(|p| [p[0], p[1]])
                    .collect();
                Ok(Some(
                    pairs
                        .into_iter()
                        .map(|[k, v]| {
                            if k.is_hole() {
                                JsValue::undefined()
                            } else {
                                JsValue::object(ctx.alloc_array(vec![k, v]))
                            }
                        })
                        .collect(),
                ))
            }
            v12_heap::Kind::Iterator => drain_iterator(ctx, v).map(Some),
            _ => Ok(None),
        };
    }
    if let Some(h) = v.as_string() {
        // Code points, not code units: `for (const c of "a\u{1F600}")` yields two
        // values in every engine, and a lone surrogate is never observable.
        let text = ctx.string_text(h);
        return Ok(Some(
            text.chars().map(|c| JsValue::string(ctx.heap.intern_text(&c.to_string()))).collect(),
        ));
    }
    Ok(None)
}
