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
//! Native bodies take `&mut Ctx` (`ctx::BuiltinFn`); the `&mut Heap` dispatch
//! site reaches them through `ctx::call_ctx`. Callback-taking methods stay at
//! the interpreter seam (`Interp::run_callback_builtin`) and are untouched
//! here. Target state: `ROADMAP.md` §2.

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
/// Iterator kind: the lazy `windows` wrapper — overlapping groups of
/// `SLOT_ARG` values, emitted from the pending buffer in `SLOT_PENDING`.
pub const ITER_KIND_HELPER_WINDOWS: i32 = 13;
/// Iterator kind: a `zip`/`zipKeyed` iterator built in `"strict"` mode whose
/// input columns had *unequal* lengths.
///
/// The spec requires such an iterator to yield the rows up to the shortest
/// column and then throw a `TypeError` on the following `next` (spec 27.1.3.2
/// step 15). The eager model here cannot know that at construction time, so the
/// fault is encoded as a kind: the rows array is truncated to the shortest
/// column and [`SLOT_ARG`] holds the row count past which `next` must throw.
/// Only the fault case uses this kind — an equal-length `strict` zip is an
/// ordinary array iterator.
pub const ITER_KIND_STRICT_FAULT: i32 = 15;

/// Iterator kind: the lazy `chunks` wrapper — consecutive non-overlapping
/// groups of `SLOT_ARG` values. Same layout as `windows`; only the buffer reset
/// after a yield differs.
pub const ITER_KIND_HELPER_CHUNKS: i32 = 14;

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

/// True for the two buffer-and-emit wrappers, `windows` and `chunks`.
///
/// They are deliberately **not** in [`is_helper_wrapper`] or
/// [`is_limit_wrapper`]: the interpreter's machine-capable wrapper executor
/// (`Interp::step_helper_wrapper`, in the barred `call_setup.rs`) branches on
/// exactly those two predicates and would treat an unknown kind as a callback
/// wrapper, calling `SLOT_ARG` as a function. Membership here is what keeps the
/// two executors apart — a `windows`/`chunks` wrapper is built with
/// `source_next = undefined`, so the seam's `helper_engine_can_drive` keeps it
/// on the engine side and the interpreter never steps one.
///
/// That invariant is the price of not being able to touch `call_setup.rs`:
/// a `windows`/`chunks` wrapper over a *machine-driven* source (a generator) is
/// not steppable, and building one over such a source is rejected rather than
/// silently mis-stepped. See [`iterator_windows`].
pub const fn is_windowing_wrapper(kind: i32) -> bool {
    matches!(kind, ITER_KIND_HELPER_WINDOWS | ITER_KIND_HELPER_CHUNKS)
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
/// - `[5]` `flatMap` only: the array whose elements are still to be yielded;
///   `windows`/`chunks` reuse it as the group buffer
/// - `[6]` `flatMap` only: the cursor into slot `[5]`; `windows`/`chunks` reuse
///   it as the `undersized` flag (a boolean, not an index)
pub const SLOT_KIND: usize = 0;
pub const SLOT_SOURCE: usize = 1;
pub const SLOT_INDEX: usize = 2;
pub const SLOT_SOURCE_NEXT: usize = 3;
pub const SLOT_ARG: usize = 4;
pub const SLOT_PENDING: usize = 5;
pub const SLOT_PENDING_INDEX: usize = 6;
/// `windows`/`chunks` only: `true` for the `"allow-partial"` undersized mode.
/// Shares slot [`SLOT_PENDING_INDEX`], which is meaningless for a windowing
/// wrapper (it keeps its whole buffer rather than walking it).
pub const SLOT_PARTIAL: usize = SLOT_PENDING_INDEX;

/// Number of slots every helper wrapper carries. Two of them are `flatMap`-only
/// (the pending array and its cursor); the rest are `undefined` for the kinds
/// that do not use them, so the vector is one fixed size for every wrapper.
pub const WRAPPER_SLOTS: usize = SLOT_PENDING_INDEX + 1;

/// Upper bound on a single eager drain, so a runaway iterator reports a
/// `RangeError` instead of exhausting memory.
pub const MAX_DRAIN: usize = 1_000_000;

/// Builds an iterator object over `source` with the given kind.
fn create_iterator(ctx: &mut Ctx, source: Handle<JsObject>, kind: i32) -> Handle<JsObject> {
    let proto = iterator_prototype(ctx);
    ctx.alloc_obj(JsObject {
        kind: v12_heap::Kind::Iterator,
        // Spec 27.1.2: `%ArrayIteratorPrototype%.[[Prototype]]` is
        // `%Iterator.prototype%`, and every array/Map/Set iterator inherits it
        // from there. Without the link the *helpers the interpreter knows about*
        // still work — the `Kind::Iterator` method surface synthesizes them by
        // name — but any helper reached only by ordinary property lookup reads
        // `undefined`, which is a silent hole that grows with every method
        // added. Linking the prototype removes the class of bug.
        prototype: proto,
        elements: vec![
            helpers::smi_or_f64(i64::from(kind)),
            JsValue::object(source),
            helpers::smi_or_f64(0),
        ],
        ..JsObject::default()
    })
}

/// The realm's `%Iterator.prototype%`, or `None` outside a realm (bare-heap
/// engine tests), where the object simply has no prototype.
///
/// `Ctx::intrinsic` is the canonical reader, but its slot table does not carry
/// `Iterator` (it is a post-ES2025 intrinsic, added to the realm without a
/// `GLOBAL_INTRINSICS` slot), so the slot is resolved from the table itself
/// here. Reading the table rather than hardcoding a number is what keeps this
/// correct if the intrinsic order ever changes.
fn iterator_prototype(ctx: &mut Ctx) -> Option<Handle<JsObject>> {
    let idx = v12_bytecode::GLOBAL_INTRINSICS
        .iter()
        .position(|name| *name == "Iterator")?;
    let global = ctx
        .global()
        .or_else(|| ctx.heap.realm_globals().first().copied())?;
    let ctor = ctx
        .heap
        .get(global)
        .properties
        .get(idx)
        .copied()?
        .as_object()?;
    ctx.heap.get(ctor).prototype
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
    let proto = iterator_prototype(ctx);
    let obj = ctx.alloc_obj(JsObject {
        kind: v12_heap::Kind::Iterator,
        // Same `%Iterator.prototype%` link as [`create_iterator`].
        prototype: proto,
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

/// One `next` of a `windows`/`chunks` wrapper: fill the pending buffer to the
/// group size, then emit it.
///
/// The buffer lives in `SLOT_PENDING` as a real Array, so the emitted value is
/// `CreateArrayFromList(buffer)` verbatim — a *fresh* array per group, which is
/// what `yields-distinct-arrays.js` checks, and what lets `windows` overlap
/// without copying a shared backing store.
///
/// `windows` drops the front element after emitting (so the next window shares
/// `size - 1` values); `chunks` empties the buffer. The two differ in exactly
/// that one line, which is why they share a kind-dispatched body rather than
/// getting two near-identical copies.
fn windowing_wrapper_next(
    ctx: &mut Ctx,
    iter: Handle<JsObject>,
    kind: i32,
    source: JsValue,
) -> Result<JsValue, Throw> {
    let size = window_size_of(ctx, ctx.heap.get(iter).elements[SLOT_ARG])?;
    let allow_partial = ctx.heap.get(iter).elements[SLOT_PARTIAL].is_true();
    // The held values live in the wrapper's own `elements`, past the fixed
    // slots, with their count in `SLOT_PENDING_INDEX`. They have to persist
    // across `next` calls: `windows` keeps `size - 1` values back so the next
    // window overlaps the last one, and re-pulling the source cannot reproduce
    // that (the source has already advanced past them).
    let mut held: Vec<JsValue> = {
        let o = ctx.heap.get(iter);
        let count = o.elements[SLOT_PENDING_INDEX]
            .as_smi()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        o.elements[WRAPPER_SLOTS..]
            .iter()
            .take(count)
            .copied()
            .collect()
    };
    loop {
        if held.len() == size {
            let group = held.clone();
            if kind == ITER_KIND_HELPER_WINDOWS {
                // Drop the front value: the next window shares `size - 1` of
                // them, which is what makes `windows` overlap.
                held.remove(0);
            } else {
                // `chunks` partitions, so the group is consumed whole.
                held.clear();
            }
            store_window_buffer(ctx, iter, &held);
            let group_v = JsValue::object(ctx.alloc_array(group));
            return Ok(iterator_result(ctx, group_v, false));
        }
        let Some(value) = pull(ctx, source)? else {
            // Exhausted with a partial group in hand. `chunks` always yields
            // its last short group; `windows` yields it only in the
            // `"allow-partial"` undersized mode and drops it under the default
            // `"only-full"`.
            let short = held.len() < size;
            let drop_tail = short && kind == ITER_KIND_HELPER_WINDOWS && !allow_partial;
            if held.is_empty() || drop_tail {
                return Ok(iterator_result(ctx, JsValue::undefined(), true));
            }
            let group = held.clone();
            store_window_buffer(ctx, iter, &[]);
            let group_v = JsValue::object(ctx.alloc_array(group));
            return Ok(iterator_result(ctx, group_v, false));
        };
        held.push(value);
        if held.len() > MAX_DRAIN {
            return Err(
                ctx.range_error("RangeError: iterator window buffer exceeds 1,000,000 values")
            );
        }
    }
}

/// Writes the windowing wrapper's held values back into its `elements`, past
/// the fixed slots, and records their count in `SLOT_PENDING_INDEX`.
fn store_window_buffer(ctx: &mut Ctx, iter: Handle<JsObject>, held: &[JsValue]) {
    let o = ctx.heap.get_mut(iter);
    o.elements.truncate(WRAPPER_SLOTS);
    o.elements.extend_from_slice(held);
    o.elements[SLOT_PENDING_INDEX] = helpers::smi_or_f64(held.len() as i64);
}

/// The `size` argument of a `windows`/`chunks` wrapper, read back from its slot.
///
/// Spec 27.1.4.8 steps 4-7 require a value that is *already* a Number (never
/// coerced through `valueOf`/`toString`), integral, and inside
/// `[1, 2^32 - 1]`. So the validation cannot be `ToNumber` — `'1'` is a
/// `TypeError`, not `1` — and the two failure kinds are distinct: a
/// non-Number or non-integral value is a `TypeError`, an out-of-range Number is
/// a `RangeError`.
fn window_size(ctx: &mut Ctx, value: JsValue) -> Result<usize, Throw> {
    let n = match value {
        v if v.as_smi().is_some() => match v.as_smi() {
            Some(n) => f64::from(n),
            None => f64::NAN,
        },
        v if v.as_f64().is_some() => match v.as_f64() {
            Some(n) => n,
            None => f64::NAN,
        },
        // NaN and ±Infinity are Numbers but not integral ones, so they land in
        // the `TypeError` arm (step 5) rather than the `RangeError` one.
        _ => {
            return Err(ctx.type_error(
                "TypeError: Iterator.prototype window size must be an integral Number",
            ));
        }
    };
    if !n.is_finite() || n.trunc() != n {
        return Err(
            ctx.type_error("TypeError: Iterator.prototype window size must be an integral Number")
        );
    }
    if n < 1.0 || n > 4_294_967_295.0 {
        return Err(ctx.range_error("RangeError: Iterator.prototype window size is out of range"));
    }
    Ok(n as usize)
}

/// [`window_size`] for the *stored* slot, where the value is a Number again
/// after the call-time check.
fn window_size_of(ctx: &mut Ctx, stored: JsValue) -> Result<usize, Throw> {
    window_size(ctx, stored)
}

/// The `undersized` option of `Iterator.prototype.windows` (step 5-6).
///
/// `undefined` means `"only-full"`; anything other than that literal or
/// `"allow-partial"` is a `TypeError`. The comparison is on the *text*, so a
/// `String` object is not silently accepted either.
fn undersized_option(ctx: &mut Ctx, value: JsValue) -> Result<bool, Throw> {
    if value.is_undefined() {
        return Ok(false);
    }
    let Some(h) = value.as_string() else {
        return Err(ctx.type_error("TypeError: Iterator.prototype.windows undersized option"));
    };
    match ctx.string_text(h).as_str() {
        "only-full" => Ok(false),
        "allow-partial" => Ok(true),
        _ => Err(ctx.type_error("TypeError: Iterator.prototype.windows undersized option")),
    }
}

/// Builds a `windows`/`chunks` wrapper.
///
/// The source's `next` is *not* resolved from the interpreter: a windowing
/// wrapper is stepped by this file alone (see [`is_windowing_wrapper`]), so a
/// source whose `next` needs the machine cannot be windowed. That is checked
/// here rather than deferred, so the failure is a `TypeError` at call time
/// instead of a mis-stepped buffer later.
fn create_windowing_iterator(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
    kind: i32,
) -> Result<JsValue, Throw> {
    let name = if kind == ITER_KIND_HELPER_WINDOWS {
        "Iterator.prototype.windows"
    } else {
        "Iterator.prototype.chunks"
    };
    require_iterator(ctx, this, name)?;
    let size = window_size(ctx, args.first().copied().unwrap_or(JsValue::undefined()))?;
    // `chunks` has no second parameter, so its trailing slot stays false.
    let allow_partial = if kind == ITER_KIND_HELPER_WINDOWS {
        undersized_option(ctx, args.get(1).copied().unwrap_or(JsValue::undefined()))?
    } else {
        false
    };
    if !engine_can_drive(ctx, this) {
        return Err(ctx.type_error(format!(
            "TypeError: {name} needs an iterator this engine can step"
        )));
    }
    let buffer = JsValue::object(ctx.alloc_array(Vec::new()));
    let mut elements = vec![
        helpers::smi_or_f64(i64::from(kind)),
        this,
        helpers::smi_or_f64(0),
        // Always `undefined`: this file steps the wrapper.
        JsValue::undefined(),
        helpers::smi_or_f64(size as i64),
        // `buffer` is retained for introspection parity with `flatMap`'s
        // pending slot; the values actually held live past the fixed slots and
        // are counted in `SLOT_PENDING_INDEX` (see `windowing_wrapper_next`).
        buffer,
        JsValue::from_bool(allow_partial),
    ];
    elements.resize(WRAPPER_SLOTS, JsValue::undefined());
    let proto = iterator_prototype(ctx);
    Ok(JsValue::object(ctx.alloc_obj(JsObject {
        kind: v12_heap::Kind::Iterator,
        // Same `%Iterator.prototype%` link as [`create_iterator`], so the
        // windowing wrapper answers the helpers by ordinary lookup.
        prototype: proto,
        elements,
        ..JsObject::default()
    })))
}

/// True when `iter` is an iterator this file can pull from without the machine:
/// an element-store iterator, or a wrapper whose chain is machine-free all the
/// way down.
fn engine_can_drive(ctx: &mut Ctx, iter: JsValue) -> bool {
    let Some(obj) = iter.as_object() else {
        return false;
    };
    if ctx.heap.get(obj).kind != v12_heap::Kind::Iterator {
        return false;
    }
    let kind = ctx.heap.get(obj).elements[SLOT_KIND]
        .as_smi()
        .unwrap_or(ITER_KIND_ARRAY_VALUES);
    if (0..=ENGINE_KIND_MAX).contains(&kind) {
        return true;
    }
    if is_windowing_wrapper(kind) {
        return engine_can_drive(ctx, ctx.heap.get(obj).elements[SLOT_SOURCE]);
    }
    // Every other wrapper was handed its source's real `next` method, which is
    // an object exactly when the chain needs the machine.
    ctx.heap.get(obj).elements[SLOT_SOURCE_NEXT]
        .as_object()
        .is_none()
}

/// `Iterator.prototype.windows(windowSize[, undersized])` — overlapping groups
/// of `windowSize` values (spec 27.1.4.8).
pub fn iterator_windows(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    create_windowing_iterator(ctx, this, args, ITER_KIND_HELPER_WINDOWS)
}

/// `Iterator.prototype.chunks(chunkSize)` — consecutive non-overlapping groups,
/// the last one possibly short (spec 27.1.4.9).
pub fn iterator_chunks(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    create_windowing_iterator(ctx, this, args, ITER_KIND_HELPER_CHUNKS)
}

/// The realm's `%Object.prototype%`, or `None` outside a realm (a bare-heap
/// engine test), where a plain object correctly has no prototype at all.
fn object_prototype(ctx: &mut Ctx) -> Option<Handle<JsObject>> {
    ctx.intrinsic("Object")
        .and_then(|v| v.as_object())
        .and_then(|ctor| ctx.heap.get(ctor).prototype)
}

/// Allocates an ES iterator result `{value, done}`.
fn iterator_result(ctx: &mut Ctx, value: JsValue, done: bool) -> JsValue {
    // `CreateIteratorResultObject` step 1: the result is an
    // `OrdinaryObjectCreate(%Object.prototype%)`. Every iterator's `next` result
    // flows through here, so leaving the link off makes
    // `Object.getPrototypeOf(it.next())` answer `null` for *all* of them — which
    // test262's `assertIteratorResult` (and so `Iterator.zip`'s
    // `basic-strict.js`) checks on every single row.
    let prototype = object_prototype(ctx);
    let heap = &mut *ctx.heap;
    let h = helpers::alloc_obj(heap, JsObject::default());
    if let Some(prototype) = prototype {
        heap.get_mut(h).prototype = Some(prototype);
    }
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
    if is_windowing_wrapper(kind) {
        return windowing_wrapper_next(ctx, iter, kind, JsValue::object(source));
    }
    // A `strict` zip over unequal columns yields its rows and *then* throws,
    // which the plain `index >= len` completion below would report as a clean
    // `done`. The check therefore has to precede it (see
    // [`ITER_KIND_STRICT_FAULT`]).
    if kind == ITER_KIND_STRICT_FAULT {
        let rows = ctx
            .heap
            .get(iter)
            .elements
            .get(SLOT_ARG)
            .and_then(|v| v.as_smi())
            .map(i64::from)
            .unwrap_or(0) as usize;
        if index >= rows {
            return Err(ctx.type_error(
                "TypeError: Iterator.zip strict mode received rows of unequal length",
            ));
        }
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
            ITER_KIND_ARRAY_VALUES | ITER_KIND_STRICT_FAULT => source_elem(ctx.heap, source, index),
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

/// Validates and parses the `limit` argument of `take`/`drop`
/// (spec 27.1.4.3 steps 2-9).
///
/// The spec has *three* distinct `RangeError` conditions before the integer
/// limit is even computed, and all three used to be silently clamped to 0 here:
///
/// 1. `numLimit` is `NaN` — which includes `take()` and `take(undefined)`, since
///    `ToNumber(undefined)` is `NaN`, not 0;
/// 2. `numLimit` is finite and greater than `2**53 - 1`, so no integer limit
///    can represent it;
/// 3. the truncated limit is negative.
///
/// `-0.5` is deliberately *not* condition 3: `ToIntegerOrInfinity(-0.5)` is
/// `-0`, and `-0 < 0` is false, so `take(-0.5)` yields nothing rather than
/// throwing. `Infinity` is not condition 2 either (it is not finite), so
/// `take(Infinity)` is a valid unbounded take.
///
/// Every value that does *not* throw keeps the clamping it always had, so this
/// is purely additive: a negative or `NaN` limit used to mean "take nothing",
/// which was a wrong value rather than a missing error.
fn take_limit(ctx: &mut Ctx, method: &str, args: &[JsValue]) -> Result<usize, Throw> {
    let n = ctx.to_number(args.first().copied().unwrap_or(JsValue::undefined()));
    if n.is_nan() || (n.is_finite() && n > MAX_SAFE_INTEGER) {
        return Err(ctx.range_error(format!("RangeError: {method} limit is not a usable Number")));
    }
    let integer_limit = n.trunc();
    if integer_limit < 0.0 {
        return Err(ctx.range_error(format!("RangeError: {method} limit must not be negative")));
    }
    // A `+∞` limit means "take everything"; the drain cap already expresses
    // that, because `MAX_DRAIN` is past any finite sequence the corpus builds.
    if !integer_limit.is_finite() {
        return Ok(MAX_DRAIN);
    }
    if integer_limit <= 0.0 {
        return Ok(0);
    }
    Ok(integer_limit.min(MAX_DRAIN as f64) as usize)
}

/// `2**53 - 1`, the largest integer a `Number` represents exactly. Step 7 of
/// `take`/`drop` rejects anything strictly above it.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

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
    let limit = take_limit(ctx, "Iterator.prototype.take", args)?;
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
    let limit = take_limit(ctx, "Iterator.prototype.drop", args)?;
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
                // `chunks_exact(2)` is the stride every Map write in
                // `map.rs` uses; the crate's pre-existing `chunks_exact`
                // warnings in that file are the accepted policy set, and a
                // lone `as_chunks` here would only make the two halves
                // disagree.
                #[allow(clippy::chunks_exact_to_as_chunks)]
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
        let mut out = Vec::new();
        for c in text.chars() {
            let unit = ctx.heap.intern_text(&c.to_string());
            out.push(JsValue::string(unit));
        }
        return Ok(Some(out));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Eager helpers and constructor statics
// ---------------------------------------------------------------------------

/// `Iterator.prototype.includes(value)` (spec 27.1.4.7) — whether `this`
/// yields `value`, by SameValueZero.
///
/// Eager, but *short-circuiting*: the moment a match is found the source is
/// abandoned rather than drained, so `it.includes(2)` on a five-element
/// iterator pulls two values and leaves the rest. The spec requires the early
/// exit, and the observable consequence (the iterator is left where the match
/// was found, not exhausted) is asserted in `es2025_surface.rs`.
pub fn iterator_includes(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    require_iterator(ctx, this, "Iterator.prototype.includes")?;
    let needle = args.first().copied().unwrap_or(JsValue::undefined());
    let mut taken = 0usize;
    loop {
        match pull(ctx, this)? {
            Some(value) => {
                taken += 1;
                if taken > MAX_DRAIN {
                    return Err(
                        ctx.range_error("RangeError: iterator drain exceeds 1,000,000 values")
                    );
                }
                if same_value_zero(value, needle) {
                    return Ok(JsValue::true_());
                }
            }
            None => return Ok(JsValue::false_()),
        }
    }
}

/// `Iterator.prototype.join(separator = ",")` (spec 27.1.4.6).
///
/// The separator goes through `ToString`; every yielded value does too. `ToString`
/// on an *object* needs the machine, so an object value renders through the
/// Tier-0 display subset (`[object Object]`) — a documented v1 boundary, not a
/// silent one. The alternative (skipping the conversion) would produce a
/// *wrong* string rather than an approximated one, which is worse.
pub fn iterator_join(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    require_iterator(ctx, this, "Iterator.prototype.join")?;
    let sep = match args.first() {
        None => ",".to_string(),
        Some(v) if v.is_undefined() => ",".to_string(),
        Some(v) => ctx.to_string(*v),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut taken = 0usize;
    while let Some(value) = pull(ctx, this)? {
        taken += 1;
        if taken > MAX_DRAIN {
            return Err(ctx.range_error("RangeError: iterator drain exceeds 1,000,000 values"));
        }
        parts.push(if value.is_null() || value.is_undefined() {
            // 27.1.4.6 step 7: a nullish value contributes the *empty* string,
            // not `"null"`/`"undefined"` — the join of `[1, null, 2]` has an
            // empty middle field, so `[null, 2].join()` is `",2"`.
            String::new()
        } else {
            ctx.to_string(value)
        });
    }
    Ok(JsValue::string(ctx.heap.intern_text(&parts.join(&sep))))
}

/// SameValueZero on engine values: bit-identical, with `-0.0` equal to `+0.0`.
///
/// The numeric arm normalises *across* the Smi/f64 split rather than testing
/// `as_f64` on both sides: `[0]` stores a Smi and `[-0]` an f64 `-0.0`, so a
/// predicate that only compares when *both* sides are floats reports
/// `[-0].includes(0)` as `false` — the exact case test262's
/// `samevaluezero-zeroes.js` checks. `map::same_value_zero` has the same
/// limitation; it is duplicated here rather than shared because that one is
/// private to its module.
fn same_value_zero(a: JsValue, b: JsValue) -> bool {
    let a_num = a.as_smi().map(f64::from).or_else(|| a.as_f64());
    let b_num = b.as_smi().map(f64::from).or_else(|| b.as_f64());
    if let (Some(x), Some(y)) = (a_num, b_num) {
        return (x.is_nan() && y.is_nan()) || x == y;
    }
    a == b
}

/// A real `%IteratorPrototype%`-inheriting iterator over a freshly built array.
///
/// `zip`, `zipKeyed` and `concat` all reduce to "compute the whole sequence,
/// then hand back an iterator over it". That is eager where the spec is lazy,
/// which is stated plainly rather than hidden: a `zip` over an infinite
/// iterable would not terminate. In exchange, no interpreter seam is needed,
/// and the returned object is a *real* iterator — it inherits
/// `%Iterator.prototype%` and answers `next` — rather than a bare object
/// holding an array's worth of elements.
fn zip_result(ctx: &mut Ctx, values: Vec<JsValue>, fault: bool) -> JsValue {
    let h = ctx.alloc_array(values);
    let proto = iterator_prototype(ctx);
    // A faulting strict zip carries its row count in `SLOT_ARG`; an ordinary one
    // keeps the three-element layout every other array iterator has.
    let mut elements = vec![
        helpers::smi_or_f64(i64::from(if fault {
            ITER_KIND_STRICT_FAULT
        } else {
            ITER_KIND_ARRAY_VALUES
        })),
        JsValue::object(h),
        helpers::smi_or_f64(0),
    ];
    if fault {
        let rows = ctx.heap.get(h).element_len();
        // The count lives in `SLOT_ARG`, which is index 4 — the same slot the
        // `take`/`drop`/`map` wrappers use for their own argument. The base
        // three-element layout stops at index 2, so the row count has to be
        // pushed *into* its slot rather than appended: appending would land it
        // at index 3 (`SLOT_SOURCE_NEXT`) and read back as no fault at all.
        elements.resize(SLOT_ARG, JsValue::undefined());
        elements.push(helpers::smi_or_f64(rows as i64));
    }
    let iter = ctx.alloc_obj(JsObject {
        kind: v12_heap::Kind::Iterator,
        elements,
        prototype: proto,
        ..JsObject::default()
    });
    JsValue::object(iter)
}

/// Drains one entry of a `zip`/`zipKeyed`/`concat` input list.
///
/// All three statics open with the same `IsObject` gate, and it is `IsObject`
/// rather than "is iterable": spec 27.1.3.2 step 1 reads *"If iterables is not
/// an Object, throw a TypeError exception"*, `GetIteratorFlattenable` step 2
/// reads the same of every element, and `Iterator.concat` step 3 does the same
/// of every item. A primitive string is iterable, so the previous
/// `require_object_coercible` + structural-drain let `Iterator.zip("")` and
/// `Iterator.concat("abc")` through instead of throwing.
fn drain_one(ctx: &mut Ctx, method: &str, value: JsValue) -> Result<Vec<JsValue>, Throw> {
    if value.as_object().is_none() {
        return Err(ctx.type_error(format!("TypeError: {method} argument is not an object")));
    }
    super::iterator::drain_iterable(ctx, value)?
        .ok_or_else(|| ctx.type_error(format!("TypeError: {method} argument is not iterable")))
}

/// The `options.mode` of a `zip`/`zipKeyed` call (spec 27.1.3.2 steps 3-5).
///
/// Exactly three string values are valid — `"shortest"` (also the default),
/// `"longest"` and `"strict"`. There is deliberately no `"pad"` mode: `padding`
/// is a *separate* option that only `longest` consults, and accepting it here
/// would let `Iterator.zip([], {mode: "pad"})` through as something the spec
/// says must throw.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ZipMode {
    Shortest,
    Longest,
    Strict,
}

fn zip_mode(ctx: &mut Ctx, options: JsValue) -> Result<ZipMode, Throw> {
    if options.is_undefined() {
        return Ok(ZipMode::Shortest);
    }
    if options.as_object().is_none() {
        return Err(ctx.type_error("TypeError: Iterator.zip options must be an object"));
    }
    let mode = own_or_inherited_read(ctx, options, "mode")?;
    if mode.is_undefined() {
        return Ok(ZipMode::Shortest);
    }
    let Some(h) = mode.as_string() else {
        return Err(ctx.type_error("TypeError: Iterator.zip options.mode must be a string"));
    };
    match ctx.string_text(h).as_str() {
        "shortest" => Ok(ZipMode::Shortest),
        "longest" => Ok(ZipMode::Longest),
        "strict" => Ok(ZipMode::Strict),
        _ => Err(ctx.type_error("TypeError: Iterator.zip options.mode is invalid")),
    }
}

/// The `options.padding` of a `zip`/`zipKeyed` call, defaulting to `undefined`.
///
/// Only consulted in `"longest"` mode. An *iterable* `padding` is drained by the
/// spec (a fresh iterator is pulled for every value needed); this engine reads
/// the option and uses the value directly, so a non-iterable `padding` is not
/// rejected here. That gap is one of the `padding-iteration.js` assertions and
/// is a machine-work omission, not a wrong value.
fn zip_padding(ctx: &mut Ctx, options: JsValue) -> JsValue {
    if options.is_undefined() {
        return JsValue::undefined();
    }
    own_or_inherited_read(ctx, options, "padding").unwrap_or(JsValue::undefined())
}

/// How many rows a `zip`/`zipKeyed` produces, and whether it must fault.
///
/// `padding` is the fill value for `"longest"`; the row count follows the mode
/// (shortest column, longest column, or shortest column with a latent fault).
fn zip_shape(lens: &[usize], mode: ZipMode) -> (usize, bool) {
    let min = lens.iter().copied().min().unwrap_or(0);
    let max = lens.iter().copied().max().unwrap_or(0);
    match mode {
        ZipMode::Shortest => (min, false),
        ZipMode::Longest => (max, false),
        // Only unequal columns fault. Equal ones complete normally, so they stay
        // a plain array iterator with no kind of their own.
        ZipMode::Strict => (min, min != max),
    }
}

/// Reads `name` off `obj` through the ordinary `[[Get]]`, so an inherited
/// `mode` is seen (the spec's `Get(options, "mode")`) while an own property
/// shadows it. Only `undefined` and a data value are possible here; a getter is
/// machine work and is reported as absent, which is the honest answer for a
/// pure native rather than a wrong one.
///
/// `pub` because `map::argument_set` implements the same `Get` for the set-like
/// validation of the Set relational methods.
pub fn own_or_inherited_read(ctx: &mut Ctx, obj: JsValue, name: &str) -> Result<JsValue, Throw> {
    let Some(h) = obj.as_object() else {
        return Ok(JsValue::undefined());
    };
    let key = v12_heap::PropKey::from_string(ctx.heap.intern_text(name));
    crate::internal_methods::dispatch_get(ctx.heap, h, key, obj).map_err(Throw::Value)
}

/// `Iterator.zip(iterables[, options])` (spec 27.1.3.2) — one array per
/// position, from a *list* of iterables.
///
/// The corpus is explicit that this is not a rest parameter: `length` is 1 and
/// the arguments are `(iterables, options)`. Getting that wrong is invisible in
/// a `zip([1,2],[3,4])` style call only because that call happens to work — the
/// first argument is then the whole list and the rest is ignored, which yields
/// rows of length 1 instead of 2.
pub fn iterator_zip(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    // Order is observable: step 1 gates `iterables` before step 2 ever touches
    // `options`, which `iterables-primitive.js` pins with an `options` object
    // whose `mode`/`padding` getters throw. Validating the argument first is
    // what makes that test pass, so the `options` read cannot be hoisted.
    let list = args.first().copied().unwrap_or(JsValue::undefined());
    if list.as_object().is_none() {
        return Err(ctx.type_error("TypeError: Iterator.zip argument is not an object"));
    }
    let options = args.get(1).copied().unwrap_or(JsValue::undefined());
    let mode = zip_mode(ctx, options)?;
    let padding = zip_padding(ctx, options);
    let entries = drain_one(ctx, "Iterator.zip", list)?;
    let mut columns: Vec<Vec<JsValue>> = Vec::with_capacity(entries.len());
    for entry in entries {
        columns.push(drain_one(ctx, "Iterator.zip", entry)?);
    }
    let (rows, fault) = zip_shape(
        &columns.iter().map(Vec::len).collect::<Vec<_>>(),
        mode,
    );
    let mut out = Vec::with_capacity(rows);
    for i in 0..rows {
        let row: Vec<JsValue> = columns
            .iter()
            .map(|c| c.get(i).copied().unwrap_or(padding))
            .collect();
        out.push(JsValue::object(ctx.alloc_array(row)));
    }
    Ok(zip_result(ctx, out, fault))
}

/// `Iterator.zipKeyed(iterables[, options])` (spec 27.1.3.3) — one
/// null-prototype object per position, keyed by the *own enumerable* keys of
/// the `iterables` object.
///
/// The corpus is explicit that the argument is an object, not a list: each of
/// its enumerable own values is an iterable, and the key is the mapping key. A
/// symbol key is honoured too, so the key is kept as a `PropKey` rather than a
/// string.
pub fn iterator_zip_keyed(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let options = args.get(1).copied().unwrap_or(JsValue::undefined());
    let mode = zip_mode(ctx, options)?;
    let padding = zip_padding(ctx, options);
    let input = args.first().copied().unwrap_or(JsValue::undefined());
    let Some(obj) = input.as_object() else {
        return Err(ctx.type_error("TypeError: Iterator.zipKeyed requires an object"));
    };
    // Own keys first (spec: `[[OwnPropertyKeys]]`), then the enumerable filter
    // (`[[GetOwnProperty]]`), which is what excludes inherited and hidden
    // properties.
    let (shape, keys) = {
        let o = ctx.heap.get(obj);
        (ctx.heap.shape_of(obj), o.property_keys.clone())
    };
    let mut columns: Vec<(v12_heap::PropKey, Vec<JsValue>)> = Vec::new();
    for key in keys.into_iter().flatten() {
        let Some(desc) = ctx.heap.lookup_property(shape, key) else {
            continue;
        };
        if !desc.attrs().enumerable() {
            continue;
        }
        let value = crate::internal_methods::dispatch_get(ctx.heap, obj, key, input)
            .map_err(Throw::Value)?;
        if value.is_undefined() {
            // `undefined` is skipped entirely, not zipped as a value.
            continue;
        }
        columns.push((key, drain_one(ctx, "Iterator.zipKeyed", value)?));
    }
    let lens: Vec<usize> = columns.iter().map(|(_, c)| c.len()).collect();
    let (rows, fault) = zip_shape(&lens, mode);
    let mut out = Vec::with_capacity(rows);
    for i in 0..rows {
        // `OrdinaryObjectCreate(null)`: the result has a *null* prototype, so
        // it is allocated with no `prototype` link and never inherits anything.
        let result = ctx.alloc_obj(JsObject::default());
        for (key, column) in &columns {
            let value = column.get(i).copied().unwrap_or(padding);
            define_keyed_prop(ctx, result, *key, value);
        }
        out.push(JsValue::object(result));
    }
    Ok(zip_result(ctx, out, fault))
}

/// Shape-bound install of a data property under an arbitrary `PropKey`.
///
/// `Ctx` exposes string-keyed and symbol-keyed installs but not the general
/// case, and `zipKeyed` needs both (a key read off the input object may be a
/// string *or* a symbol). Mirrors `Ctx::define_data_prop_with_attrs` without
/// touching `ctx.rs`, which is outside this lane's file ownership.
fn define_keyed_prop(ctx: &mut Ctx, obj: Handle<JsObject>, key: v12_heap::PropKey, value: JsValue) {
    let shape = ctx.heap.shape_of_mut(obj);
    debug_assert!(ctx.heap.lookup_property(shape, key).is_none());
    let child = ctx.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    ctx.heap.bind_shape(obj, child);
    let slot = (ctx.heap.get(child).num_own - 1) as usize;
    let settings = ctx.heap.get_mut(obj);
    if settings.properties.len() <= slot {
        settings.properties.resize(slot + 1, JsValue::hole());
    }
    if settings.property_keys.len() <= slot {
        settings.property_keys.resize(slot + 1, None);
    }
    settings.properties[slot] = value;
    settings.property_keys[slot] = Some(key);
}

/// `Iterator.concat(...items)` (spec 27.1.3.4) — every input, in order. This
/// one *is* a rest parameter (`length` 0).
pub fn iterator_concat(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut all: Vec<JsValue> = Vec::new();
    for arg in args {
        all.extend(drain_one(ctx, "Iterator.concat", *arg)?);
        if all.len() > MAX_DRAIN {
            return Err(ctx.range_error("RangeError: Iterator.concat exceeds 1,000,000 values"));
        }
    }
    Ok(zip_result(ctx, all, false))
}
