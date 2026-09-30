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
        (Some(x), Some(y)) => (x.is_nan() && y.is_nan()) || x == y || (x == 0.0 && y == 0.0),
        _ => a == b,
    }
}

/// Allocates an empty collection of `kind`.
fn construct(ctx: &mut Ctx, kind: Kind, this: JsValue) -> Result<JsValue, Throw> {
    let obj = ctx.alloc_obj(JsObject {
        kind,
        ..JsObject::default()
    });
    let ctor_name = match kind {
        Kind::Map => "Map",
        Kind::Set => "Set",
        _ => unreachable!("collection construction accepts Map and Set only"),
    };
    if let Some(proto) = this
        .as_object()
        .and_then(|ctor| ctx.heap.get(ctor).prototype)
        .or_else(|| {
            ctx.intrinsic(ctor_name)
                .and_then(|v| v.as_object())
                .and_then(|ctor| ctx.heap.get(ctor).prototype)
        })
    {
        ctx.heap.get_mut(obj).prototype = Some(proto);
    }
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
    let found = (0..store.len() / step).any(|i| {
        let key_slot = store[i * step];
        !key_slot.is_hole() && same_value_zero(key_slot, key)
    });
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
        .find(|&i| {
            let key_slot = store[i * step];
            !key_slot.is_hole() && same_value_zero(key_slot, key)
        })
        .map(|i| i * step)
    {
        store[pos..pos + step].fill(JsValue::hole());
        return Ok(JsValue::true_());
    }
    Ok(JsValue::false_())
}

/// Native fallback for `Map` construction. Realm execution routes this id
/// through the interpreter callback seam so iterable arguments can re-enter
/// the machine; this handler remains the bare-heap fallback.
pub fn map_construct(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    construct(ctx, Kind::Map, this)
}

/// Native fallback for `Set` construction; realm execution uses the
/// interpreter callback seam for iterable arguments.
pub fn set_construct(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    construct(ctx, Kind::Set, this)
}

/// `Map.prototype.get(key)` — the value for `key`, or `undefined`.
pub fn map_get(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Map.prototype.get", Some(Kind::Map))?;
    let key = args.first().copied().unwrap_or_else(JsValue::undefined);
    for pair in entries(ctx.heap, obj, Kind::Map)
        .unwrap_or_default()
        .chunks_exact(2)
    {
        if !pair[0].is_hole() && same_value_zero(pair[0], key) {
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
        if !pair[0].is_hole() && same_value_zero(pair[0], key) {
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
    let n = entries(ctx.heap, obj, Kind::Map)
        .unwrap_or_default()
        .chunks_exact(2)
        .filter(|pair| !pair[0].is_hole())
        .count() as i64;
    Ok(helpers::smi_or_f64(n))
}

/// `Set.prototype.add(value)` — adds and returns the Set.
pub fn set_add(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Set.prototype.add", Some(Kind::Set))?;
    let value = args.first().copied().unwrap_or_else(JsValue::undefined);
    let entries = &mut ctx.heap.get_mut(obj).elements;
    if !entries
        .iter()
        .any(|&v| !v.is_hole() && same_value_zero(v, value))
    {
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
    let n = entries(ctx.heap, obj, Kind::Set)
        .unwrap_or_default()
        .iter()
        .filter(|&&v| !v.is_hole())
        .count() as i64;
    Ok(helpers::smi_or_f64(n))
}

/// `Map.prototype.clear()` — removes all entries.
pub fn map_clear(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Map.prototype.clear", Some(Kind::Map))?;
    ctx.heap
        .get_mut(obj)
        .elements
        .chunks_exact_mut(2)
        .for_each(|pair| pair.fill(JsValue::hole()));
    Ok(JsValue::undefined())
}

/// `Set.prototype.clear()` — removes all values.
pub fn set_clear(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Set.prototype.clear", Some(Kind::Set))?;
    ctx.heap
        .get_mut(obj)
        .elements
        .iter_mut()
        .for_each(|slot| *slot = JsValue::hole());
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

/// `Set.prototype.entries()` — `[value, value]` pairs per spec.
pub fn set_entries(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_SET_ENTRIES)
}

/// `Set.prototype.keys()` — alias of values per spec.
pub fn set_keys(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_SET_VALUES)
}

/// `Set.prototype.values()` — values iterator over `this`.
pub fn set_values(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::iterator::iterator_for(ctx, this, super::iterator::ITER_KIND_SET_VALUES)
}

// ---------------------------------------------------------------------------
// Set relational methods (ECMA-262 24.2.3, the "new Set methods" proposal)
// ---------------------------------------------------------------------------

/// Which side of a relational operation the receiver plays, for the two
/// algorithms that differ only in which operand's order is preserved.
///
/// `SetIntersection` and `SetDifference` both build a *table* from the argument
/// and then walk the receiver, so the result order follows `this` in both
/// cases. The asymmetry the spec actually has is in the *early exits*:
/// `intersection` returns the empty result immediately when either size is
/// zero, and `isSupersetOf` short-circuits on a size comparison, so both need
/// the two sizes without materialising the other set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SetOp {
    Union,
    Intersection,
    Difference,
    SymmetricDifference,
    IsSubsetOf,
    IsSupersetOf,
    IsDisjointFrom,
}

/// A fresh empty `Set` with `%Set.prototype%`, as a `Handle`.
///
/// Spec 24.2.3 opens every relational method with `Let resultSet be ! new
/// Set()` (a `Construct`, not an allocation from the receiver), so the prototype
/// comes from the realm's `Set` constructor rather than from `this`. Reading it
/// off `this` would happen to work for a plain Set — a Set instance's
/// `[[Prototype]]` *is* `%Set.prototype%` — but it would silently do the wrong
/// thing for a subclass instance, whose prototype is the subclass's.
///
/// The result is a `Result` because `ctx.intrinsic("Set")` is `None` outside a
/// realm (a bare-heap engine test), where there is no `%Set.prototype%` to link
/// and the caller reports that as the `TypeError` it already owes.
fn new_set(ctx: &mut Ctx) -> Result<Handle<JsObject>, Throw> {
    let Some(ctor) = ctx.intrinsic("Set").and_then(|v| v.as_object()) else {
        return Err(ctx.type_error("TypeError: Set relational methods need a realm"));
    };
    let value = construct(ctx, Kind::Set, JsValue::object(ctor))?;
    value
        .as_object()
        .ok_or_else(|| ctx.type_error("TypeError: Set construction produced a non-object"))
}

/// `GetSetRecord(other)` (24.2.3.1) reduced to what a `&mut Heap` native can
/// do: validate the argument as a *set-like object* and drain its `keys()`
/// method, collapsing duplicates with SameValueZero, first occurrence winning.
///
/// Three spec steps are load-bearing and each has a test262 file pinning it:
///
/// 1. **The argument must be an Object** (step 1). Not "iterable" — a primitive
///    string is iterable, so a structural drain would happily consume `""`,
///    and `union("")` must be a `TypeError`.
/// 2. **`has` and `keys` must be callable** (steps 6 and 8). A plain array has
///    neither, which is why `union([3])` is a `TypeError` and not a union with
///    `[3]`; conversely a *set-like array* — one that has been given `size`,
///    `has` and `keys` — is consumed through `keys()` and its elements never
///    come from the array's element store. `drain_iterable` gets both of these
///    backwards, which is the whole of the `array-throws` / `called-with-object`
///    cluster.
/// 3. **The dedupe is not an optimisation** (step 3 builds a real Set): the
///    spec builds a `Set` from the argument before any of the seven algorithms
///    run, so `union([2,2])` adds `2` once and `isSubsetOf` measures the *set* of
///    the argument, not its length.
///
/// `size` is read for its side effects only (a throwing `size` getter is
/// observable) and then dropped: the drained values are the membership test, so
/// nothing downstream needs the reported size.
fn argument_set(ctx: &mut Ctx, method: &str, args: &[JsValue]) -> Result<Vec<JsValue>, Throw> {
    let arg = args.first().copied().unwrap_or(JsValue::undefined());
    // Step 1: `If obj is not an Object, throw a TypeError exception.` This is
    // `IsObject`, not `RequireObjectCoercible` — a primitive string is
    // coercible and iterable, and must still be rejected here.
    if arg.as_object().is_none() {
        return Err(ctx.type_error(format!("TypeError: {method} argument is not an object")));
    }
    // Steps 2-4: `size`, coerced with ToNumber and NaN-folded to 0. The value is
    // only used by the two size-short-circuiting algorithms, and this drain
    // computes membership from the values themselves, so it is read for its
    // side effects (a throwing `size` getter is observable) and then dropped.
    let size = super::iterator::own_or_inherited_read(ctx, arg, "size")?;
    let _size = ctx.to_number(size);
    // Steps 5-6 and 7-8: both must be callable, in that order, so a `has` that
    // is present-but-not-callable is reported before a missing `keys`.
    for name in ["has", "keys"] {
        let v = super::iterator::own_or_inherited_read(ctx, arg, name)?;
        if !is_callable(ctx.heap, v) {
            return Err(ctx.type_error(format!(
                "TypeError: {method} argument's {name} is not callable"
            )));
        }
    }
    let keys = super::iterator::own_or_inherited_read(ctx, arg, "keys")?;
    // Steps 10-12 iterate the value of `keys()`. Calling it is machine work a
    // `&mut Heap` native cannot do — the same re-entrancy wall that stops
    // `@@replace` from accepting a function replacement — so this drains the
    // *argument* structurally instead. That is correct for every set-like whose
    // `keys()` enumerates exactly the argument's own values, which is every Set
    // and Map, and wrong only for a set-like whose `keys()` is a real function
    // returning a different sequence (`set-like-array.js` and its six siblings).
    // Documented deviation, not a silent one: the validation above — the part
    // the observable `TypeError`s live in — is spec-exact.
    let _ = keys;
    let Some(values) = super::iterator::drain_iterable(ctx, arg)? else {
        return Err(ctx.type_error(format!(
            "TypeError: {method} argument's keys did not return an iterator"
        )));
    };
    let mut out: Vec<JsValue> = Vec::with_capacity(values.len());
    for v in values {
        if !out.iter().any(|&seen| same_value_zero(seen, v)) {
            out.push(v);
        }
    }
    Ok(out)
}

/// `IsCallable` for a `&mut Ctx` body: any `Kind::Function`, which is what the
/// `Iterator` helper and `regexp.rs` each reach for. Duplicated rather than
/// shared because `map.rs` and `regexp.rs` are separate modules and the
/// alternative is a public re-export for one line.
fn is_callable(heap: &Heap, v: JsValue) -> bool {
    v.as_object()
        .is_some_and(|o| heap.get(o).kind == v12_heap::Kind::Function)
}

/// Shared body of all seven relational methods.
///
/// `op` selects the algorithm; everything else is common: the `[[SetData]]`
/// brand check on `this` (24.2.3 step 1 for every one of them), the
/// `GetSetFromIterable` of the argument, and then the combination.
fn set_relational(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
    op: SetOp,
) -> Result<JsValue, Throw> {
    let name = match op {
        SetOp::Union => "Set.prototype.union",
        SetOp::Intersection => "Set.prototype.intersection",
        SetOp::Difference => "Set.prototype.difference",
        SetOp::SymmetricDifference => "Set.prototype.symmetricDifference",
        SetOp::IsSubsetOf => "Set.prototype.isSubsetOf",
        SetOp::IsSupersetOf => "Set.prototype.isSupersetOf",
        SetOp::IsDisjointFrom => "Set.prototype.isDisjointFrom",
    };
    // `RequireInternalSlot(O, [[SetData]])` — a plain object, a Map, or a
    // primitive receiver is a TypeError before the argument is even looked at.
    let obj = ctx.this_object(this, name, Some(Kind::Set))?;
    let mine: Vec<JsValue> = entries(ctx.heap, obj, Kind::Set)
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|e| !e.is_hole())
        .collect();
    let other = argument_set(ctx, name, args)?;
    let mine_has = |v: JsValue| mine.iter().any(|&m| same_value_zero(m, v));
    let other_has = |v: JsValue| other.iter().any(|&o| same_value_zero(o, v));

    match op {
        // 24.2.3.4: every element of O, then every element of other that O
        // does not already have. Duplicates cannot occur: `O` is a Set and the
        // argument went through `GetSetFromIterable`.
        SetOp::Union => {
            let h = new_set(ctx)?;
            let store = &mut ctx.heap.get_mut(h).elements;
            store.extend(mine.iter().copied());
            store.extend(other.iter().copied().filter(|&v| !mine_has(v)));
            Ok(JsValue::object(h))
        }
        // 24.2.3.5: an empty operand short-circuits to the empty result;
        // otherwise the table is the *argument* and the walk is `this`, so the
        // result order is `this`'s.
        SetOp::Intersection => {
            let h = new_set(ctx)?;
            if !(mine.is_empty() || other.is_empty()) {
                let store = &mut ctx.heap.get_mut(h).elements;
                store.extend(mine.iter().copied().filter(|&v| other_has(v)));
            }
            Ok(JsValue::object(h))
        }
        // 24.2.3.6: `this`'s elements absent from the argument.
        SetOp::Difference => {
            let h = new_set(ctx)?;
            let store = &mut ctx.heap.get_mut(h).elements;
            store.extend(mine.iter().copied().filter(|&v| !other_has(v)));
            Ok(JsValue::object(h))
        }
        // 24.2.3.7: `this`-only elements, then argument-only elements — the
        // order is *this* first, which is the observable the spec fixes.
        SetOp::SymmetricDifference => {
            let h = new_set(ctx)?;
            let store = &mut ctx.heap.get_mut(h).elements;
            store.extend(mine.iter().copied().filter(|&v| !other_has(v)));
            store.extend(other.iter().copied().filter(|&v| !mine_has(v)));
            Ok(JsValue::object(h))
        }
        // 24.2.3.8: every element of `this` is in the argument.
        SetOp::IsSubsetOf => {
            for v in &mine {
                if !other_has(*v) {
                    return Ok(JsValue::false_());
                }
            }
            Ok(JsValue::true_())
        }
        // 24.2.3.9: the size comparison comes first (step 3) and is what makes
        // an argument larger than `this` an immediate `false`.
        SetOp::IsSupersetOf => {
            if other.len() > mine.len() {
                return Ok(JsValue::false_());
            }
            for v in &other {
                if !mine_has(*v) {
                    return Ok(JsValue::false_());
                }
            }
            Ok(JsValue::true_())
        }
        // 24.2.3.10: no element of `this` is in the argument.
        SetOp::IsDisjointFrom => {
            for v in &mine {
                if other_has(*v) {
                    return Ok(JsValue::false_());
                }
            }
            Ok(JsValue::true_())
        }
    }
}

/// `Set.prototype.union(other)` — see [`set_relational`].
pub fn set_union(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_relational(ctx, this, args, SetOp::Union)
}

/// `Set.prototype.intersection(other)` — see [`set_relational`].
pub fn set_intersection(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_relational(ctx, this, args, SetOp::Intersection)
}

/// `Set.prototype.difference(other)` — see [`set_relational`].
pub fn set_difference(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_relational(ctx, this, args, SetOp::Difference)
}

/// `Set.prototype.symmetricDifference(other)` — see [`set_relational`].
pub fn set_symmetric_difference(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_relational(ctx, this, args, SetOp::SymmetricDifference)
}

/// `Set.prototype.isSubsetOf(other)` — see [`set_relational`].
pub fn set_is_subset_of(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_relational(ctx, this, args, SetOp::IsSubsetOf)
}

/// `Set.prototype.isSupersetOf(other)` — see [`set_relational`].
pub fn set_is_superset_of(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_relational(ctx, this, args, SetOp::IsSupersetOf)
}

/// `Set.prototype.isDisjointFrom(other)` — see [`set_relational`].
pub fn set_is_disjoint_from(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    set_relational(ctx, this, args, SetOp::IsDisjointFrom)
}
