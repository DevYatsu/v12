//! Array built-ins: push, pop, and length handling.
//!
//! Non-callback native bodies take `&mut Ctx` (`ctx::BuiltinFn`); the `&mut
//! Heap` dispatch site reaches them through `ctx::call_ctx`. Callback-taking
//! methods stay at the interpreter seam (`Interp::run_callback_builtin`) and
//! are untouched here. Target state: `ROADMAP.md` §2.

use v12_heap::{Handle, Heap, JsObject, JsValue, PropKey, V12Str};
use v12_native::Throw;

use super::{ctx::Ctx, helpers};

/// Maximum array length (2^32 - 1).
const MAX_ARRAY_LENGTH: u32 = u32::MAX;

/// Length property key, interned lazily via the heap.
fn length_prop(heap: &mut Heap) -> v12_heap::PropKey {
    let h = heap.intern_string(V12Str::latin1(b"length".to_vec()));
    v12_heap::PropKey::from_string(h)
}

/// `Array.prototype.push(...items)` – appends elements and updates `length`.
pub fn array_push(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.push", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)?;
    if len + args.len() as f64 > f64::from(MAX_ARRAY_LENGTH) {
        return Err(ctx.range_error("RangeError: invalid array length"));
    }
    // Spec places appended items at `length`-based indices, so a sparse
    // receiver (`new Array(len)`) grows at `len`, not at its store end.
    for (i, &item) in args.iter().enumerate() {
        index_set(&mut *ctx.heap, obj, len as i64 + i as i64, item)?;
    }
    let new_len = len + args.len() as f64;
    set_array_len(&mut *ctx.heap, obj, new_len)?;
    Ok(helpers::smi_or_f64(new_len as i64))
}

/// `Array.prototype.pop()` – removes the last element.
pub fn array_pop(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.pop", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)?;
    if len < 1.0 {
        set_array_len(&mut *ctx.heap, obj, 0.0)?;
        return Ok(JsValue::undefined());
    }
    let last = len - 1.0;
    let value = index_get(&mut *ctx.heap, obj, last as i64)?.0;
    index_delete(&mut *ctx.heap, obj, last as i64);
    set_array_len(&mut *ctx.heap, obj, last)?;
    Ok(value)
}

/// `Array.isArray(value)` – true if value is an Array exotic object.
pub fn array_is_array(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is = v
        .as_object()
        .is_some_and(|h| ctx.heap.get(h).kind == v12_heap::Kind::Array);
    Ok(JsValue::from_bool(is))
}

pub fn array_slice(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.slice", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    // W3.2: the third copy of the hand-rolled
    // `as_smi().or(as_f64).map(trunc).unwrap_or(0)` coercion, replaced by
    // `Ctx::to_integer_or_infinity` — `slice("1")` used to start at index 0.
    let to_idx = |heap: &mut Heap, v: JsValue| -> i64 {
        if v.is_undefined() {
            return 0;
        }
        super::ctx::heap_to_integer_or_infinity(heap, v) as i64
    };
    let start = if args.is_empty() {
        0
    } else {
        let n = to_idx(&mut *ctx.heap, args[0]);
        if n < 0 { (len + n).max(0) } else { n.min(len) }
    };
    let end = if args.len() < 2 || args[1].is_undefined() {
        len
    } else {
        let n = to_idx(&mut *ctx.heap, args[1]);
        if n < 0 { (len + n).max(0) } else { n.min(len) }
    };
    let slice = read_range(&mut *ctx.heap, obj, start, end)?;
    Ok(new_array(ctx, slice))
}

pub fn array_sort(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.sort", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    let mut elems = read_range(&mut *ctx.heap, obj, 0, len)?;
    // Filter holes (undefined) to end per spec simplified: keep order for undefined.
    elems.retain(|v| !v.is_undefined() && !v.is_hole());
    // `to_string` allocates per call; sort_by_cached_key computes each key once.
    elems.sort_by_cached_key(|a| ctx.to_string(*a));
    // `SortIndexedProperties` writes the whole `0..len` view, so an inherited
    // index becomes an own one: `sort.call(Object.create({0:"inh",length:3}))`
    // leaves `{"0":"inh"}`, not an empty object.
    elems.resize((len.clamp(0, MAX_SCAN_SPAN)) as usize, JsValue::undefined());
    for (i, &v) in elems.iter().enumerate() {
        index_set(&mut *ctx.heap, obj, i as i64, v)?;
    }
    Ok(this)
}

// ---------------------------------------------------------------------------
// Generic index/length plumbing
//
// Array methods must work on both real arrays (`elements_array`) and generic
// array-like receivers (`elements` vec / own `length` property), so reads and
// writes route through these helpers instead of touching the stores directly.
// ---------------------------------------------------------------------------

/// The interned integer-index property key for `i` (`ToString(i)`).
fn index_prop(heap: &mut Heap, i: i64) -> PropKey {
    let text = i.to_string();
    let h = heap.intern_string(V12Str::latin1_slice(text.as_bytes()));
    PropKey::from_string(h)
}

/// ES `LengthOfArrayLike`: `ToLength(Get(O, "length"))`.
///
/// The single length reader. `Get` goes through `[[Get]]`, so a `length` held
/// on the prototype (`Object.create({length: 3})`) counts and a **string**
/// `length` coerces: the hand-rolled `as_smi().or(as_f64)` that stood here
/// turned `{length: "3"}` into `NaN` and therefore into `0`, so `at(1)` read
/// `undefined` on a receiver that demonstrably had a `"b"` there. The clamp in
/// [`super::ctx::length_from`] stays load-bearing: `length` is not a u32
/// quantity (array-likes legally carry up to 2^53-1).
fn array_like_len(heap: &mut Heap, obj: Handle<JsObject>) -> Result<f64, Throw> {
    let key = length_prop(heap);
    // MUTATION 6: an elements-first probe ahead of the property path.
    if heap.get(obj).kind == v12_heap::Kind::Array
        && let Some(&flat) = heap.get(obj).elements.first()
        && !flat.is_undefined()
    {
        return Ok(super::ctx::length_from(
            super::ctx::heap_to_integer_or_infinity(heap, flat),
        ));
    }
    let v = crate::internal_methods::dispatch_get(heap, obj, key, JsValue::object(obj))
        .map_err(Throw::Value)?;
    if v.is_undefined() {
        // No `length` on the receiver or its chain. For the two kinds that
        // *have* an element store the store is the de-facto length (an
        // unbound array and a mapped `arguments`); anything else has none.
        let kind = heap.get(obj).kind;
        return Ok(if kind == v12_heap::Kind::Array {
            array_extent(heap, obj) as f64
        } else if kind == v12_heap::Kind::Arguments {
            element_extent(heap, obj) as f64
        } else {
            0.0
        });
    }
    Ok(super::ctx::length_from(
        super::ctx::heap_to_integer_or_infinity(heap, v),
    ))
}

/// Highest index the receiver's element store can hold, plus one.
///
/// NOT `element_len()`: for `Kind::Array` that counts *present* elements, so a
/// sparse array's trailing holes were invisible to every reader built on it.
/// `elements_array.length()` is the array-length view the spec's `length`
/// tracks; `Kind::Arguments` uses the flat `elements` vec, whose length *is*
/// its extent.
fn element_extent(heap: &Heap, obj: Handle<JsObject>) -> i64 {
    heap.get(obj).elements.len() as i64
}

/// `Kind::Array`'s element-store extent: the highest index inserted, plus one.
///
/// Reached only as `array_like_len`'s fallback — for an array with **no**
/// `length` property at all, which an engine-created array never is but a
/// heap-built `JsObject::array(Vec::new())` (what
/// `crates/v12-engine/src/tests.rs` builds) is. `ElementsArray::len()` counts
/// *present* entries and so under-reports a sparse array, and the high-water
/// mark has no cheap accessor — hence `dense_entries()`.
fn array_extent(heap: &Heap, obj: Handle<JsObject>) -> i64 {
    heap.get(obj)
        .elements_array
        .dense_entries()
        .last()
        .map_or(0, |&(i, _)| i64::from(i) + 1)
}

/// `HasProperty(O, ToString(i))` + `Get(O, ToString(i))`, the spec's pair.
///
/// `present` is a separate answer because no single-value read can express it:
/// `indexOf`/`fill`/`copyWithin` must tell a hole from a present `undefined`,
/// and `ordinary_get` collapses the two. This is the one genuinely new helper
/// in this file — `reflect.rs`'s `create_list_from_array_like` cannot use it
/// because `CreateDataProperty` has no hole semantics.
///
/// Errors propagate (`Result`, not the ADR draft's bare tuple): the only
/// failing rungs are the unimplemented `Proxy` traps, and swallowing those
/// would turn a `TypeError` into a silent no-op, which is the failure mode
/// this whole file is being fixed for.
fn index_get(heap: &mut Heap, obj: Handle<JsObject>, i: i64) -> Result<(JsValue, bool), Throw> {
    if i < 0 || i > i64::from(u32::MAX) - 1 {
        // 2^32-1 is the last valid array index; no property can be there.
        return Ok((JsValue::undefined(), false));
    }
    let idx = i as u32;
    // `Kind::Array`/`Kind::Arguments` fast path — NOT a second reader: this is
    // the *first* rung of `ordinary_get` (internal_methods.rs:975), reached
    // before the shape walk. Keeping it here is measured arithmetic:
    // `dispatch_get` re-derives the object kind and re-parses the key text on
    // every probe. It also has to stay, because `HasProperty` does not
    // consult the element store, so a routed `dispatch_has` would call a real
    // array element a hole.
    let kind = heap.get(obj).kind;
    if matches!(kind, v12_heap::Kind::Array | v12_heap::Kind::Arguments)
        && let Some(v) = heap.get(obj).get_element(idx)
    {
        return Ok((v, !v.is_hole()));
    }
    let key = index_prop(heap, i);
    if !crate::internal_methods::dispatch_has(heap, obj, key).map_err(Throw::Value)? {
        return Ok((JsValue::undefined(), false));
    }
    let v = crate::internal_methods::dispatch_get(heap, obj, key, JsValue::object(obj))
        .map_err(Throw::Value)?;
    Ok((v, !v.is_hole()))
}

/// `Set(O, ToString(i), v, O)`.
///
/// Behaviour change this file now owns: for a **Proxy** receiver this reaches
/// `proxy_set` (`internal_methods.rs:1237`) and throws `TypeError` from the
/// unimplemented-trap stub, where the deleted `write_index` silently wrote a
/// store the plain property read never consulted. That is the correct honest
/// failure — the spec requires a real `[[Set]]` — but it is a change.
fn index_set(heap: &mut Heap, obj: Handle<JsObject>, i: i64, v: JsValue) -> Result<(), Throw> {
    if i < 0 || i > i64::from(u32::MAX) - 1 {
        return Ok(());
    }
    // `Kind::Array`/`Kind::Arguments` fast path — the first rung of
    // `ordinary_set` (`internal_methods.rs:1043`), kept for the same measured
    // reason as `index_get`'s: routing through `dispatch_set` interns one
    // heap string per index, and the interpreter's GC-stress tests collect on
    // every safepoint, so the extra allocations widened a root window in
    // `promise_capability_seam` far enough to lose a live record.
    let kind = heap.get(obj).kind;
    if matches!(kind, v12_heap::Kind::Array | v12_heap::Kind::Arguments) {
        heap.get_mut(obj).set_element(i as u32, v);
        return Ok(());
    }
    let key = index_prop(heap, i);
    crate::internal_methods::dispatch_set(heap, obj, key, v, JsValue::object(obj))
        .map_err(Throw::Value)?;
    Ok(())
}

/// `Delete(O, ToString(i))` — `pop`/`shift`/`splice`'s DeletePropertyOrThrow.
///
/// There is no `dispatch_delete` to route through (only the table slot at
/// `internal_methods.rs:67` exists), so this mirrors the two rungs a `[[Delete]]`
/// has: the element store for the kinds that own one, and the receiver's own
/// shape/dictionary storage — hole-out, keeping the descriptor — which is what
/// `Interp::delete_property` (`v12-interp/src/property.rs:3623`) does for
/// ordinary objects. An index the receiver does not own is a successful no-op,
/// so an inherited index is never touched.
fn index_delete(heap: &mut Heap, obj: Handle<JsObject>, i: i64) {
    if i < 0 || i > i64::from(u32::MAX) - 1 {
        return;
    }
    let idx = i as u32;
    if heap.get(obj).kind == v12_heap::Kind::Array {
        // Hole the slot, never truncate: `shift`/`splice` call this in a loop
        // and a truncate needs the lattice extent, which costs a full
        // `elements_snapshot()` — quadratic. `set_array_len` does the one
        // trim per call, after the loop.
        heap.get_mut(obj).set_element(idx, JsValue::hole());
        return;
    }
    if let Some(slot) = heap.get_mut(obj).elements.get_mut(idx as usize) {
        *slot = JsValue::hole();
    }
    let key = index_prop(heap, i);
    let shape = heap.shape_of(obj);
    if let Some(desc) = heap.lookup_property(shape, key)
        && let Some(slot) = desc.slot()
        && let Some(cell) = heap.get_mut(obj).properties.get_mut(slot as usize)
    {
        *cell = JsValue::hole();
    }
}

/// One past the highest index that can hold data on `obj` **or anywhere on its
/// prototype chain** — element store, own shape integer keys, dictionary-rung
/// integer keys, and the same three rungs of each prototype.
///
/// This answers "where does data stop", and nothing else. It is deliberately
/// NOT the deleted `dense_bound`: that helper also returned `min(.., len)` and
/// was used to *narrow* the scan range of `indexOf`/`includes`/`Array.from` and
/// the write range of `fill`/`copyWithin`, so a receiver whose indices it could
/// not see (dictionary rung, inherited) silently read as empty. Here it only
/// ever *skips work* at indices that are provably holes, which is what keeps
/// `Array.from(new Array(2**32))` from walking a billion `[[Get]]` calls.
fn visible_extent(heap: &mut Heap, obj: Handle<JsObject>) -> i64 {
    if heap.get(obj).kind == v12_heap::Kind::Array {
        // A real array's indices live in a lattice that exposes no cheap
        // high-water mark to this crate (`ElementsArray::len` counts *present*
        // entries, which is not an extent — that conflation is what hid a
        // sparse array's tail from the deleted readers). Both `index_get` and
        // `read_range` take the array fast path instead, so nothing is
        // narrowed here.
        return i64::MAX;
    }
    let mut extent = element_extent(heap, obj);
    let mut cur = Some(obj);
    while let Some(o) = cur {
        if extent >= MAX_SCAN_SPAN {
            // Already past the scan cap: nothing further can widen it.
            break;
        }
        // Collect key handles first: decoding a key's text needs `&mut heap`
        // and cannot hold the descriptor borrow across it.
        let mut keys: Vec<v12_heap::Handle<v12_heap::V12Str>> = heap
            .get(heap.shape_of(o))
            .descriptors
            .as_slice()
            .iter()
            .filter_map(|d| d.key().string())
            .collect();
        if let Some(map) = heap.get(o).dictionary.as_ref() {
            keys.extend(map.keys().filter_map(|k| k.string()));
        }
        for key in keys {
            let text = helpers::heap_string_text(heap, key);
            if let Some(n) = index_text(&text) {
                extent = extent.max(n + 1);
            }
        }
        cur = heap.get(o).prototype;
    }
    extent
}

/// `ToString(k)` is a canonical array index (an integer in `[0, 2^32-2]`,
/// no leading zero). Mirrors `internal_methods::prop_key_as_index`.
fn index_text(text: &str) -> Option<i64> {
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.as_bytes()[0] == b'0' {
        return None;
    }
    text.parse::<i64>().ok()
}

/// The highest index a whole-array read walks: `len`, capped at
/// [`MAX_SCAN_SPAN`] but never below the data the receiver can see (so
/// `{3000000: "x", length: 4000000}` is read whole). A pure function of
/// `(len, extent)` so callers compute `visible_extent` once.
fn read_limit(len: i64, extent: i64) -> i64 {
    if len <= MAX_SCAN_SPAN {
        len
    } else {
        len.min(MAX_SCAN_SPAN.max(extent))
    }
}

/// `Kind::Array` read: the lattice iterator already yields every index up to
/// its own high-water mark with holes intact. That is both the cheapest read
/// for a real array and the only one that can see an index no shape key names,
/// so it stays as the fast path. Trailing holes are padded out to the
/// requested span — capped by `MAX_SCAN_SPAN` — because `slice(new Array(3))`
/// is three `undefined`s in the spec and `[]` here.
fn array_range(heap: &Heap, obj: Handle<JsObject>, start: i64, end: i64) -> Vec<JsValue> {
    let all = heap.get(obj).elements_snapshot();
    let lo = start.max(0) as usize;
    let hi = (end.max(start).max(0) as usize).min(all.len());
    let mut out: Vec<JsValue> = all[lo.min(hi)..hi]
        .iter()
        .map(|v| {
            if v.is_hole() {
                JsValue::undefined()
            } else {
                *v
            }
        })
        .collect();
    out.resize(
        (end - start).clamp(0, MAX_SCAN_SPAN) as usize,
        JsValue::undefined(),
    );
    out
}

/// Reads `start..end` through `index_get`, holes reading as `undefined` —
/// which is exactly what `slice`/`join`/`flat`/`concat`/`Array.from` put in
/// their result. Indices at or past the visible extent skip the dispatcher:
/// they are provably holes, and skipping is what keeps a huge `length` cheap.
fn read_range(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    start: i64,
    end: i64,
) -> Result<Vec<JsValue>, Throw> {
    if heap.get(obj).kind == v12_heap::Kind::Array {
        return Ok(array_range(heap, obj, start, end));
    }
    let extent = visible_extent(heap, obj);
    let end = end.min(read_limit(end, extent));
    let mut out: Vec<JsValue> = Vec::new();
    for i in start..end.max(start) {
        if i >= extent {
            out.push(JsValue::undefined());
            continue;
        }
        out.push(index_get(heap, obj, i)?.0);
    }
    Ok(out)
}

/// `Set(O, "length", len, O)` — the one length writer.
///
/// The deleted `set_array_len` only ever lowered `length` on `Kind::Array`,
/// which is why `unshift` on an `arguments` object returned the right count
/// and left `length` at the old value, and why a generic receiver's `length`
/// was never lowered at all. Routing the write through `[[Set]]` also gives a
/// frozen receiver the spec's refusal instead of a silent overwrite.
///
/// `Kind::Array` keeps its store shrink first: `length` is not implemented as
/// an array-length exotic here, so lowering the property alone would leave the
/// lattice holding entries past the new `length` (`push` would then append
/// into them).
fn set_array_len(heap: &mut Heap, obj: Handle<JsObject>, len: f64) -> Result<(), Throw> {
    let len = len.min(f64::from(u32::MAX));
    let key = length_prop(heap);
    let value = helpers::smi_or_f64(len as i64);
    if heap.get(obj).kind == v12_heap::Kind::Array {
        // `length` is not an array-length exotic here, so lowering the
        // property alone leaves the lattice holding entries past the new
        // `length` — `pop` returned `"c"` and `a[2]` still read `"c"` at
        // `length === 2`. Trim the store first.
        //
        // Gated on the length *view*, not on a snapshot: `push` lowers
        // nothing, and a snapshot per `push` would make a 1e5-element push
        // loop quadratic.
        let view = array_like_len(heap, obj)?;
        if len < view {
            let kept = heap.get(obj).elements_snapshot();
            let keep = (len.max(0.0) as usize).min(kept.len());
            heap.get_mut(obj).replace_elements(kept[..keep].to_vec());
        }
    }
    crate::internal_methods::dispatch_set(heap, obj, key, value, JsValue::object(obj))
        .map_err(Throw::Value)?;
    Ok(())
}

/// Relative-index normalization shared by `at`/`slice`-style methods:
/// negative values count from `len`, the result is clamped to `[0, len]`.
///
/// The coercion is [`Ctx::to_integer_or_infinity`] (its `heap_` twin). W3.2
/// replaced the private
/// `as_smi().map(i64::from).or_else(|| as_f64().map(trunc)).unwrap_or(0)`
/// that stood here — and two more copies of it in `slice` and `splice`. That
/// shape coerced **no** string or boolean at all: `at("1")` read index 0 where
/// the spec says 1. `as i64` still does the saturating cast (Infinity → `i64::MAX`,
/// which the `(len + n).max(0)`/`n.min(len)` arms below reduce to `len`/`0`
/// exactly as before).
fn relative_index(heap: &mut Heap, v: Option<JsValue>, len: i64, default: i64) -> i64 {
    let Some(v) = v else { return default };
    if v.is_undefined() {
        return default;
    }
    let n = super::ctx::heap_to_integer_or_infinity(heap, v) as i64;
    if n < 0 { (len + n).max(0) } else { n.min(len) }
}

/// Allocates an array and links it to `%Array.prototype%` (agent-created
/// arrays must carry the realm prototype, like literal arrays do through the
/// interpreter's `NewArray` arm). Roots it too.
fn new_array(ctx: &mut Ctx, elements: Vec<JsValue>) -> JsValue {
    JsValue::object(ctx.alloc_array(elements))
}

/// `Array.prototype.indexOf(search, fromIndex?)` – strict-equality scan;
/// holes are skipped (they never match, even `undefined`).
pub fn array_index_of(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.indexOf", None)?;
    let heap = &mut *ctx.heap;
    let search = args.first().copied().unwrap_or(JsValue::undefined());
    let len = array_like_len(heap, obj)? as i64;
    // Real arrays scan present entries only: the element store can be far
    // shorter than the length view (huge/sparse receivers), and ranging over
    // `0..len` would visit billions of holes. Holes never match, so entries
    // outside `[from, len)` cannot contribute either.
    if heap.get(obj).kind == v12_heap::Kind::Array {
        let from = relative_index(heap, args.get(1).copied(), len, 0).max(0);
        for (i, v) in heap.get(obj).elements_array.dense_entries() {
            let i = i64::from(i);
            if i < from {
                continue;
            }
            if i >= len {
                break;
            }
            if helpers::strict_equals(heap, search, v) {
                return Ok(helpers::js_number(i as f64));
            }
        }
        return Ok(JsValue::from_i32_smi(-1).unwrap_or_else(|| JsValue::from_f64(-1.0)));
    }
    // A hole never matches, so the walk stops at the visible extent: past it
    // `HasProperty` is false for *every* index, inherited or own. The extent
    // covers the prototype chain, which the deleted `dense_bound` did not —
    // that omission is why `indexOf(Object.create({0:"inh",length:3}), "inh")`
    // answered `-1`.
    let extent = visible_extent(heap, obj).min(len);
    let from = relative_index(heap, args.get(1).copied(), len, 0).max(0);
    for i in from..extent {
        let (v, present) = index_get(heap, obj, i)?;
        if present && helpers::strict_equals(heap, search, v) {
            return Ok(helpers::js_number(i as f64));
        }
    }
    Ok(JsValue::from_i32_smi(-1).unwrap_or_else(|| JsValue::from_f64(-1.0)))
}

/// `Array.prototype.lastIndexOf(search, fromIndex?)` – backwards scan.
pub fn array_last_index_of(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.lastIndexOf", None)?;
    let heap = &mut *ctx.heap;
    let search = args.first().copied().unwrap_or(JsValue::undefined());
    let len = array_like_len(heap, obj)? as i64;
    // Entry scan like `indexOf`: only present elements can match, so a
    // huge length view never widens the walk.
    if heap.get(obj).kind == v12_heap::Kind::Array {
        let from = relative_index(heap, args.get(1).copied(), len, len - 1);
        for (i, v) in heap
            .get(obj)
            .elements_array
            .dense_entries()
            .into_iter()
            .rev()
        {
            let i = i64::from(i);
            if i > from || i >= len {
                continue;
            }
            if helpers::strict_equals(heap, search, v) {
                return Ok(helpers::js_number(i as f64));
            }
        }
        return Ok(JsValue::from_i32_smi(-1).unwrap_or_else(|| JsValue::from_f64(-1.0)));
    }
    let extent = visible_extent(heap, obj).min(len);
    if extent <= 0 {
        return Ok(JsValue::from_i32_smi(-1).unwrap_or_else(|| JsValue::from_f64(-1.0)));
    }
    let from = relative_index(heap, args.get(1).copied(), len, len - 1).min(extent - 1);
    let mut i = from.max(0);
    loop {
        let (v, present) = index_get(heap, obj, i)?;
        if present && helpers::strict_equals(heap, search, v) {
            return Ok(helpers::js_number(i as f64));
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    Ok(JsValue::from_i32_smi(-1).unwrap_or_else(|| JsValue::from_f64(-1.0)))
}

/// `Array.prototype.includes(search, fromIndex?)` – `SameValueZero` scan
/// (NaN matches NaN); holes compare as `undefined`.
pub fn array_includes(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.includes", None)?;
    let heap = &mut *ctx.heap;
    let search = args.first().copied().unwrap_or(JsValue::undefined());
    let len = array_like_len(heap, obj)? as i64;
    // Entry scan: present elements compare by value; any hole in
    // `[from, len)` reads as `undefined`, so an `undefined` search hits when
    // the range holds fewer present entries than indices.
    if heap.get(obj).kind == v12_heap::Kind::Array {
        let from = relative_index(heap, args.get(1).copied(), len, 0).max(0);
        let mut present: i64 = 0;
        for (i, v) in heap.get(obj).elements_array.dense_entries() {
            let i = i64::from(i);
            if i < from {
                continue;
            }
            if i >= len {
                break;
            }
            present += 1;
            if helpers::same_value_zero(heap, search, v) {
                return Ok(JsValue::from_bool(true));
            }
        }
        if search.is_undefined() && len - from > present {
            return Ok(JsValue::from_bool(true));
        }
        return Ok(JsValue::from_bool(false));
    }
    let extent = visible_extent(heap, obj).min(len);
    let from = relative_index(heap, args.get(1).copied(), len, 0).max(0);
    for i in from..extent {
        let (v, present) = index_get(heap, obj, i)?;
        // A hole reads as `undefined` for `includes`.
        if (present && helpers::same_value_zero(heap, search, v))
            || (!present && search.is_undefined())
        {
            return Ok(JsValue::from_bool(true));
        }
    }
    // Every index at or beyond the visible extent reads as a hole, i.e.
    // `undefined` — a huge sparse receiver still includes `undefined`.
    if search.is_undefined() && extent < len {
        return Ok(JsValue::from_bool(true));
    }
    Ok(JsValue::from_bool(false))
}

/// `Array.prototype.concat(...items)` – `IsConcatSpreadable` arguments
/// contribute their elements, everything else is appended as-is.
///
/// The receiver is *not* unconditionally spread: `IsConcatSpreadable(O)` is
/// false for a non-Array `O` that has no `@@isConcatSpreadable`, so
/// `concat.call(argumentsObj)` appends the object as one element. The deleted
/// `elements_snapshot()` version spread the receiver unconditionally, which
/// turned `concat.call(arguments(1,2,3))` into `[1,2,3]` where the spec says
/// `[argumentsObj]`.
pub fn array_concat(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut out: Vec<JsValue> = Vec::new();
    let this_obj = this.as_object();
    if let Some(obj) = this_obj
        && ctx.heap.get(obj).kind == v12_heap::Kind::Array
    {
        let len = array_like_len(&mut *ctx.heap, obj)? as i64;
        out.extend(read_range(&mut *ctx.heap, obj, 0, len)?);
    } else {
        out.push(this);
    }
    for &item in args {
        let is_spread = item
            .as_object()
            .is_some_and(|h| ctx.heap.get(h).kind == v12_heap::Kind::Array);
        if is_spread {
            let h = item.as_object().expect("checked spread");
            let len = array_like_len(&mut *ctx.heap, h)? as i64;
            out.extend(read_range(&mut *ctx.heap, h, 0, len)?);
        } else {
            out.push(item);
        }
    }
    Ok(new_array(ctx, out))
}

/// `Array.prototype.at(index)` – relative indexing, out-of-range → undefined.
pub fn array_at(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.at", None)?;
    let heap = &mut *ctx.heap;
    let len = array_like_len(heap, obj)? as i64;
    let idx = relative_index(heap, args.first().copied(), len, 0);
    if idx < 0 || idx >= len {
        return Ok(JsValue::undefined());
    }
    Ok(index_get(heap, obj, idx)?.0)
}

/// `Array.prototype.reverse()` – in place, returns the receiver.
pub fn array_reverse(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.reverse", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    let mut elems = read_range(&mut *ctx.heap, obj, 0, len)?;
    elems.reverse();
    // Write the whole range back: a hole that moved must become an explicit
    // `undefined` (and vice versa), which `replace_elements` on a flat vec
    // could not express for a generic receiver at all.
    for (i, &v) in elems.iter().enumerate() {
        index_set(&mut *ctx.heap, obj, i as i64, v)?;
    }
    Ok(this)
}

/// `Array.prototype.shift()` – removes and returns the first element.
pub fn array_shift(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.shift", None)?;
    let heap = &mut *ctx.heap;
    let len = array_like_len(heap, obj)? as i64;
    if len == 0 {
        return Ok(JsValue::undefined());
    }
    let first = index_get(heap, obj, 0)?.0;
    // Spec: an absent source index DELETES the destination rather than
    // writing `undefined` — which is why shifting `Object.create({0:"inh",
    // length:3})` leaves no own index behind instead of two `undefined`s.
    for k in 1..len {
        let (v, present) = index_get(heap, obj, k)?;
        if present {
            index_set(heap, obj, k - 1, v)?;
        } else {
            index_delete(heap, obj, k - 1);
        }
    }
    index_delete(heap, obj, len - 1);
    set_array_len(heap, obj, (len - 1) as f64)?;
    Ok(first)
}

/// `Array.prototype.unshift(...items)` – prepends, returns the new length.
pub fn array_unshift(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.unshift", None)?;
    let heap = &mut *ctx.heap;
    let len = array_like_len(heap, obj)? as i64;
    let n = args.len() as i64;
    // Back to front, so no read is clobbered by an earlier write.
    for k in (0..len).rev() {
        let (v, present) = index_get(heap, obj, k)?;
        if present {
            index_set(heap, obj, k + n, v)?;
        } else {
            index_delete(heap, obj, k + n);
        }
    }
    for (i, &item) in args.iter().enumerate() {
        index_set(heap, obj, i as i64, item)?;
    }
    // No `if kind == Array` guard: the deleted guard is why `unshift` on an
    // `arguments` object returned 4 and left `length` at 3.
    set_array_len(heap, obj, (len + n) as f64)?;
    Ok(helpers::js_number((len + n) as f64))
}

/// `Array.prototype.splice(start, deleteCount?, ...items)` – removes and
/// inserts, returning the removed elements.
///
/// Implemented as the spec's own in-place steps 7/8/10/11 rather than as a
/// "rewrite the whole store" snapshot: on a generic receiver the removed
/// elements must come *out* of the property table, and a `length` the store
/// never knew about must still come down. The deleted snapshot version clamped
/// every index to the store length, which is why `splice` on `{0:"a",1:"b",
/// 2:"c",length:3}` returned `[]` and changed nothing.
pub fn array_splice(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.splice", None)?;
    let heap = &mut *ctx.heap;
    let len = array_like_len(heap, obj)? as i64;
    let start = relative_index(heap, args.first().copied(), len, 0)
        .max(0)
        .min(len);
    let delete_count = match args.get(1).copied() {
        None => len - start,
        Some(v) if v.is_undefined() => len - start,
        Some(v) => {
            // W3.2: fourth copy of the same coercion, same replacement.
            let d = super::ctx::heap_to_integer_or_infinity(heap, v) as i64;
            d.max(0).min(len - start)
        }
    };
    let items: Vec<JsValue> = args.iter().skip(2).copied().collect();
    let insert_count = items.len() as i64;

    let mut removed: Vec<JsValue> = Vec::new();
    for j in 0..delete_count.min(MAX_SCAN_SPAN) {
        removed.push(index_get(heap, obj, start + j)?.0);
    }

    if insert_count < delete_count {
        // Step 7: close the gap down, then delete the tail that fell out of it.
        for k in start..len - delete_count {
            let (v, present) = index_get(heap, obj, k + delete_count)?;
            if present {
                index_set(heap, obj, k, v)?;
            } else {
                index_delete(heap, obj, k);
            }
        }
        for k in (len - delete_count)..(len - insert_count) {
            index_delete(heap, obj, k);
        }
    } else if insert_count > delete_count {
        // Step 8: open a gap of `insert_count - delete_count` and slide the
        // tail up into it, back to front so no read is clobbered.
        let mut k = len - delete_count;
        while k > start {
            let (v, present) = index_get(heap, obj, k + delete_count - 1)?;
            if present {
                index_set(heap, obj, k + insert_count - 1, v)?;
            } else {
                index_delete(heap, obj, k + insert_count - 1);
            }
            k -= 1;
        }
    }

    for (i, &item) in items.iter().enumerate() {
        index_set(heap, obj, start + i as i64, item)?;
    }
    set_array_len(heap, obj, (len - delete_count + insert_count).max(0) as f64)?;
    Ok(new_array(ctx, removed))
}

/// Upper bound on entries a single `fill`/`copyWithin` call materializes in
/// the element store. Realistic tests stay far below it; a huge `length`
/// property on a sparse receiver clamps instead of hanging the runner or
/// growing the store toward 2^32 entries (OOM). A **permanent, named**
/// deviation — pinned by `max_write_span_is_a_declared_deviation` in
/// `crates/v12-engine/tests/array_generic_receivers.rs`, because the silent
/// narrowing this replaced (`dense_bound`) is what made `fill`'s range *wrong*
/// rather than merely capped.
const MAX_WRITE_SPAN: i64 = 1_000_000;

/// Upper bound on the index range a whole-array read walks. Separate from
/// [`MAX_WRITE_SPAN`] because a read past the visible extent is skipped for
/// free (see [`visible_extent`]), so this only bounds the *size of the result
/// vector*, never correctness.
const MAX_SCAN_SPAN: i64 = 1_000_000;

/// `Array.prototype.fill(value, start?, end?)`.
pub fn array_fill(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.fill", None)?;
    let heap = &mut *ctx.heap;
    let len = array_like_len(heap, obj)? as i64;
    let start = relative_index(heap, args.get(1).copied(), len, 0).max(0);
    let end = relative_index(heap, args.get(2).copied(), len, len).max(0);
    let value = args.first().copied().unwrap_or(JsValue::undefined());
    // `end` here is the spec's range, capped by `MAX_WRITE_SPAN` — a declared
    // deviation, not a silent one. The old `dense_bound`-based clamp narrowed
    // the range to the indices the *old* reader could see, which is why
    // `fill.call({0:"a",length:3}, "q", 0, 3)` wrote into a store no property
    // read consulted and left the object literally unchanged.
    let hi = end.min(start + MAX_WRITE_SPAN);
    for i in start..hi {
        index_set(heap, obj, i, value)?;
    }
    Ok(this)
}

/// `Array.prototype.copyWithin(target, start, end?)` – interior memmove.
pub fn array_copy_within(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.copyWithin", None)?;
    let heap = &mut *ctx.heap;
    let len = array_like_len(heap, obj)? as i64;
    let target = relative_index(heap, args.first().copied(), len, 0).max(0);
    let start = relative_index(heap, args.get(1).copied(), len, 0).max(0);
    let end = relative_index(heap, args.get(2).copied(), len, len).max(0);
    if target >= len || start >= end {
        return Ok(this);
    }
    let count = (end - start).min(len - target).min(MAX_WRITE_SPAN);
    // Snapshot the source before writing: the spec picks its copy direction
    // from whether the ranges overlap, and on a generic receiver there is no
    // cheaper way to be right than reading the window once.
    let mut src: Vec<(JsValue, bool)> = Vec::with_capacity(count.max(0) as usize);
    for k in 0..count {
        src.push(index_get(heap, obj, start + k)?);
    }
    for (k, (v, present)) in src.into_iter().enumerate() {
        let d = target + k as i64;
        if present {
            index_set(heap, obj, d, v)?;
        } else {
            // An absent source deletes the destination; writing `undefined`
            // there would leave an own property the spec never creates.
            index_delete(heap, obj, d);
        }
    }
    Ok(this)
}

/// `Array.prototype.flat(depth?)` – flattens nested arrays to `depth`
/// (default 1); holes read as `undefined`.
pub fn array_flat(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.flat", None)?;
    let depth = match args.first().copied() {
        None => 1.0,
        Some(v) if v.is_undefined() => 1.0,
        Some(v) => ctx.to_number(v),
    };
    let heap = &mut *ctx.heap;
    let mut out: Vec<JsValue> = Vec::new();
    flatten_into(heap, obj, depth, &mut out)?;
    Ok(new_array(ctx, out))
}

fn flatten_into(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    depth: f64,
    out: &mut Vec<JsValue>,
) -> Result<(), Throw> {
    let len = array_like_len(heap, obj)? as i64;
    for i in 0..len {
        let (v, present) = index_get(heap, obj, i)?;
        if !present {
            // `FlattenIntoArray` steps 5-6: an index the source does not *have*
            // is skipped, not read as `undefined`. That is why `flat` on
            // `Object.create({length: 3})` is `[]` and not three `undefined`s.
            continue;
        }
        let nested = v
            .as_object()
            .filter(|h| heap.get(*h).kind == v12_heap::Kind::Array);
        if let (Some(h), true) = (nested, depth >= 1.0) {
            flatten_into(heap, h, depth - 1.0, out)?;
        } else {
            out.push(v);
        }
    }
    Ok(())
}

/// `Array.prototype.toString()` – spec step 3: `Get(O, "join")`, call it if it
/// is callable, otherwise `OrdinaryToPrimitive(O, string)`.
///
/// The deleted body called `join` unconditionally, which is why
/// `Array.prototype.toString.call({0:"a",length:3})` answered `""` where the
/// spec (and `node`) answer `"[object Object]"`.
pub fn array_to_string(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.toString", None)?;
    let key = PropKey::from_string(ctx.heap.intern_text("join"));
    let func =
        crate::internal_methods::dispatch_get(&mut *ctx.heap, obj, key, JsValue::object(obj))
            .map_err(Throw::Value)?;
    let callable = func
        .as_object()
        .is_some_and(|o| ctx.heap.get(o).kind == v12_heap::Kind::Function);
    if callable {
        return join_text(ctx, this, ",");
    }
    // `OrdinaryToPrimitive(O, string)` lands on `Object.prototype.toString`,
    // whose builtin tag is fixed by the receiver's exotic kind. A native
    // cannot re-enter the interpreter, so a receiver that *overrides*
    // `toString` is not honoured here — a recorded gap, not a wrong answer
    // for the built-in tags.
    let tag = match ctx.heap.get(obj).kind {
        v12_heap::Kind::Arguments => "Arguments",
        v12_heap::Kind::Array => "Array",
        _ => "Object",
    };
    Ok(JsValue::string(
        ctx.heap.intern_text(&format!("[object {tag}]")),
    ))
}

/// `Array.prototype.join`'s text, computed from the index/length readers
/// rather than from the element store.
///
/// `array_to_string` cannot delegate to the registry's `join` body
/// (`builtins/mod.rs:1773`) and get a correct answer: that body is
/// `elements_snapshot()`-based, so it answers `""` for a generic receiver and
/// renders a hole as `<unprintable>` — both measured divergences from `node`.
fn join_text(ctx: &mut Ctx, this: JsValue, sep: &str) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.join", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    let end = read_limit(len, visible_extent(&mut *ctx.heap, obj));
    let mut parts: Vec<String> = Vec::new();
    for i in 0..end {
        let (v, _present) = index_get(&mut *ctx.heap, obj, i)?;
        if v.is_undefined() || v.is_null() || v.is_hole() {
            parts.push(String::new());
        } else {
            parts.push(ctx.to_string(v));
        }
    }
    Ok(JsValue::string(ctx.heap.intern_text(&parts.join(sep))))
}

/// `Array.of(...items)` – a new array from the argument list.
pub fn array_of(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(new_array(ctx, args.to_vec()))
}

/// `Array.from(arrayLike)` – array-likes (via `length` + indexed reads) and
/// strings (per code point). Iterable objects need the interpreter's
/// iterator protocol and are not supported by this native path.
pub fn array_from(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let Some(&source) = args.first() else {
        return Ok(new_array(ctx, Vec::new()));
    };
    if let Some(h) = source.as_string() {
        // Flatten/intern through `ctx`, before `heap` is borrowed below.
        let text = ctx.string_text(h);
        let chars: Vec<JsValue> = text
            .chars()
            .map(|c| JsValue::string(ctx.heap.intern_text(&c.to_string())))
            .collect();
        return Ok(new_array(ctx, chars));
    }
    let heap = &mut *ctx.heap;
    let Some(obj) = source.as_object() else {
        return Ok(new_array(ctx, Vec::new()));
    };
    let len = array_like_len(heap, obj)? as i64;
    // A huge `length` property on a sparse receiver describes mostly holes;
    // `read_range` fills them as `undefined` without dispatching, and the
    // length property is fixed afterwards so the result still reads
    // `length === len`. The visible extent includes the prototype chain, so
    // `Array.from(Object.create({0:"inh", length:3}))` sees the inherited
    // index — the deleted `dense_bound` saw own shape keys only.
    let items = read_range(heap, obj, 0, len)?;
    let arr_v = new_array(ctx, items);
    if let Some(arr) = arr_v.as_object() {
        set_array_len(&mut *ctx.heap, arr, len as f64)?;
    }
    Ok(arr_v)
}

/// `Array([length|elem, ...])` – callable/constructible form: one numeric
/// argument sets the length; otherwise the arguments become the elements.
pub fn array_construct(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let items: Vec<JsValue> = if args.len() == 1 {
        let n = args[0].as_smi().map(f64::from).or(args[0].as_f64());
        match n {
            Some(len) if len.trunc() == len && (0.0..=4294967295.0).contains(&len) => {
                // Spec: only the "length" property is defined; no index
                // properties are created. Materializing `len` elements would
                // explode on huge lengths (test262 uses 2**32 - 1).
                // `alloc_array_sparse` links `%Array.prototype%` and binds
                // the canonical length shape.
                let arr = ctx.alloc_array_sparse(len as u32);
                return Ok(JsValue::object(arr));
            }
            _ => args.to_vec(),
        }
    } else {
        args.to_vec()
    };
    Ok(new_array(ctx, items))
}
