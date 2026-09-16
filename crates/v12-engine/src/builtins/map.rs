//! Map and Set built-ins.
//!
//! Minimal but correct v1: entries live in the object's `elements` vec as
//! `[k1, v1, k2, v2, …]` pairs (Map) or `[v1, v2, …]` values (Set), with
//! linear-scan membership (SameValueZero on `JsValue` bits — the engine's
//! identity semantics for keys). O(n) lookups are acceptable at this stage;
//! a hash-backed table replaces this when the value model gains a proper
//! key hash.
//!
//! Map and Set share one storage layout modulo entry stride (2 vs 1), so
//! the per-op implementations are the stride-generic helpers below; the
//! `pub` natives are thin `Kind`-specialized wrappers.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`. Callback-taking methods (`forEach`) stay at the
//! interpreter seam (`Interp::run_callback_builtin`) and are untouched here.

use v12_heap::{Handle, Heap, JsObject, JsValue, Kind};
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

/// Entry stride for a collection kind: Map pairs, Set single values.
fn stride(kind: Kind) -> usize {
    match kind {
        Kind::Map => 2,
        _ => 1,
    }
}

/// The collection's entry store, or `None` when `obj` is not of `kind`.
fn entries(heap: &Heap, obj: Handle<JsObject>, kind: Kind) -> Option<&[JsValue]> {
    if heap.get(obj).kind != kind {
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

/// Allocates an empty collection of `kind`.
fn construct(ctx: &mut Ctx, kind: Kind) -> Result<JsValue, Throw> {
    let obj = ctx.alloc_obj(JsObject {
        kind,
        ..JsObject::default()
    });
    Ok(JsValue::object(obj))
}

/// Membership test over `kind`'s entries: keys at every `stride`-th slot.
fn has(
    ctx: &mut Ctx,
    this: JsValue,
    method: &str,
    kind: Kind,
    key: JsValue,
) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, method, Some(kind))?;
    let step = stride(kind);
    let store = entries(ctx.heap, obj, kind).unwrap_or_default();
    let found = (0..store.len() / step).any(|i| same_value_zero(store[i * step], key));
    Ok(JsValue::from_bool(found))
}

/// Removes the first entry whose leading slot matches `key`; reports presence.
fn delete(
    ctx: &mut Ctx,
    this: JsValue,
    method: &str,
    kind: Kind,
    key: JsValue,
) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, method, Some(kind))?;
    let step = stride(kind);
    let store = &mut ctx.heap.get_mut(obj).elements;
    if let Some(pos) = (0..store.len() / step)
        .find(|&i| same_value_zero(store[i * step], key))
        .map(|i| i * step)
    {
        store.drain(pos..pos + step);
        return Ok(JsValue::true_());
    }
    Ok(JsValue::false_())
}

/// `Map(iterable?)` — creates a Map. The iterable is ignored for v1
/// (constructor from entries is a later conformance item).
pub fn map_construct(ctx: &mut Ctx, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    construct(ctx, Kind::Map)
}

/// `Set(iterable?)` — creates a Set.
pub fn set_construct(ctx: &mut Ctx, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    construct(ctx, Kind::Set)
}

/// `Map.prototype.get(key)` — the value for `key`, or `undefined`.
pub fn map_get(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Map.prototype.get", Some(Kind::Map))?;
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    for pair in entries(ctx.heap, obj, Kind::Map)
        .unwrap_or_default()
        .chunks_exact(2)
    {
        if same_value_zero(pair[0], key) {
            return Ok(pair[1]);
        }
    }
    Ok(JsValue::undefined())
}

/// `Map.prototype.set(key, value)` — sets and returns the Map.
pub fn map_set(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Map.prototype.set", Some(Kind::Map))?;
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    let value = args.get(1).copied().unwrap_or_else(JsValue::undefined);
    let entries = &mut ctx.heap.get_mut(obj).elements;
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
pub fn map_has(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    has(ctx, this, "Map.prototype.has", Kind::Map, key)
}

/// `Map.prototype.delete(key)` — removes and returns `true` if present.
pub fn map_delete(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    delete(ctx, this, "Map.prototype.delete", Kind::Map, key)
}

/// `Map.prototype.size` getter — number of entries. Wired as a native.
pub fn map_size(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Map.prototype.size", Some(Kind::Map))?;
    let n = (entries(ctx.heap, obj, Kind::Map).unwrap_or_default().len() / 2) as i64;
    Ok(helpers::smi_or_f64(n))
}

/// `Set.prototype.add(value)` — adds and returns the Set.
pub fn set_add(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Set.prototype.add", Some(Kind::Set))?;
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    let entries = &mut ctx.heap.get_mut(obj).elements;
    if !entries.iter().any(|&v| same_value_zero(v, value)) {
        entries.push(value);
    }
    Ok(this)
}

/// `Set.prototype.has(value)` — membership.
pub fn set_has(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    has(ctx, this, "Set.prototype.has", Kind::Set, value)
}

/// `Set.prototype.delete(value)` — removes and returns `true` if present.
pub fn set_delete(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    delete(ctx, this, "Set.prototype.delete", Kind::Set, value)
}

/// `Set.prototype.size` getter — number of values.
pub fn set_size(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Set.prototype.size", Some(Kind::Set))?;
    let n = entries(ctx.heap, obj, Kind::Set).unwrap_or_default().len() as i64;
    Ok(helpers::smi_or_f64(n))
}

/// `Map.prototype.clear()` — removes all entries.
pub fn map_clear(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Map.prototype.clear", Some(Kind::Map))?;
    ctx.heap.get_mut(obj).elements.clear();
    Ok(JsValue::undefined())
}

/// `Set.prototype.clear()` — removes all values.
pub fn set_clear(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Set.prototype.clear", Some(Kind::Set))?;
    ctx.heap.get_mut(obj).elements.clear();
    Ok(JsValue::undefined())
}

/// `Map.prototype.entries()` — entries iterator over `this`.
pub fn map_entries(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_MAP_ENTRIES)
}

/// `Map.prototype.keys()` — keys iterator over `this`.
pub fn map_keys(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_MAP_KEYS)
}

/// `Map.prototype.values()` — values iterator over `this`.
pub fn map_values(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_MAP_VALUES)
}

/// `Set.prototype.entries()` — `[value, value]` pairs per spec; v1 reuses
/// values iteration (entries surface present, pair shape deferred).
pub fn set_entries(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_SET_VALUES)
}

/// `Set.prototype.keys()` — alias of values per spec.
pub fn set_keys(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_SET_VALUES)
}

/// `Set.prototype.values()` — values iterator over `this`.
pub fn set_values(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_SET_VALUES)
}
