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
        index_set(ctx, obj, len as i64 + i as i64, item)?;
    }
    let new_len = len + args.len() as f64;
    set_array_len(ctx, obj, new_len)?;
    Ok(helpers::smi_or_f64(new_len as i64))
}

/// `Array.prototype.pop()` – removes the last element.
pub fn array_pop(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.pop", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)?;
    if len < 1.0 {
        set_array_len(ctx, obj, 0.0)?;
        return Ok(JsValue::undefined());
    }
    let last = len - 1.0;
    let value = index_get(&mut *ctx.heap, obj, last as i64)?.0;
    delete_index_or_throw(ctx, obj, last as i64)?;
    set_array_len(ctx, obj, last)?;
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
    let slice = read_slots(&mut *ctx.heap, obj, start, end)?;
    Ok(new_array_slots(ctx, slice))
}

pub fn array_sort(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.sort", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    // Only the indices the receiver HAS take part: `SortIndexedProperties` step
    // 3 collects `Get(O, k)` for every *present* k, so a hole is not a value
    // here — reading it would manufacture an `undefined` element.
    let slots = read_slots(&mut *ctx.heap, obj, 0, len)?;
    let mut present: Vec<JsValue> = slots.iter().filter_map(|s| *s).collect();
    // Undefined sorts after every defined element, and the undefined elements
    // keep their relative order — that is what `sort` has always done here and
    // it is what `node` does. Sorting them *by their string key* would put
    // `undefined` between `"n"` and `"z"`, which is wrong: `sort([undefined,
    // "z"])` is `[z, undefined]`.
    let (defined, undefined): (Vec<JsValue>, Vec<JsValue>) =
        present.iter().partition(|v| !v.is_undefined());
    let mut defined = defined;
    // `to_string` allocates per call; sort_by_cached_key computes each key once.
    defined.sort_by_cached_key(|a| ctx.to_string(*a));
    present = defined;
    present.extend(undefined);
    // Write back: the sorted values land at `0..present.len()`, and every index
    // from there to `len` is DELETED if the receiver owns it. Writing
    // `undefined` across the tail instead is what made a holey receiver gain
    // own keys `node` never creates — and deleting unconditionally would
    // delete an index the receiver owns that sorting left below `len`.
    let n = present.len() as i64;
    for i in 0..n.min(len) {
        index_set(ctx, obj, i, present[i as usize])?;
    }
    for i in n..len.min(MAX_SCAN_SPAN) {
        delete_index_or_throw(ctx, obj, i)?;
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

/// ES `Set(O, ToString(i), v, true)` — the `true` is `Throw`, so a refusal is
/// a `TypeError` rather than a silent no-op.
///
/// Takes `&mut Ctx`, not `&mut Heap`, because the throw needs the realm's
/// `TypeError` intrinsic (`Ctx::type_error`) and `Heap` does not carry the
/// global. Every mutating method below therefore routes its spec writes through
/// this one function rather than reaching the store directly.
///
/// `dispatch_set` already answers the boolean correctly (it is what
/// `Reflect.set` reports, and `Reflect.set(o, k, v)` on a frozen key answers
/// `false` in v12 exactly as in node). **This helper used to discard it**
/// (`.map_err(Throw::Value)?` keeps the `Err` and drops the `Ok(false)`), which
/// is why seven mutating methods proceeded as though a refused write had
/// succeeded. Only `push` threw, because appending to a frozen receiver reached
/// the non-extensible check on a NEW index rather than the writable check on an
/// existing one.
fn index_set(ctx: &mut Ctx, obj: Handle<JsObject>, i: i64, v: JsValue) -> Result<(), Throw> {
    if i < 0 || i > i64::from(u32::MAX) - 1 {
        return Ok(());
    }
    // `Kind::Array`/`Kind::Arguments` fast path — the first rung of
    // `ordinary_set` (`internal_methods.rs:1043`), kept for the same measured
    // reason as `index_get`'s: routing through `dispatch_set` interns one
    // heap string per index, and the interpreter's GC-stress tests collect on
    // every safepoint, so the extra allocations widened a root window in
    // `promise_capability_seam` far enough to lose a live record.
    //
    // CONCESSION, and it is a real one: this rung cannot honour a refusal,
    // because v12 models **no per-index attributes for a real array** —
    // `Object.defineProperty(a, 2, {configurable: false})` is silently dropped
    // and `Object.freeze([1,2,3])` leaves every element reporting
    // `writable:true, configurable:true` (measured; named in the frozen-array
    // test in `tests/array_generic_receivers.rs`). `ordinary_set` has the
    // identical hole for the identical reason, so this is ONE missing
    // `v12-heap` feature rather than two omissions — and answering
    // "configurable" unconditionally here would make the rung LOOK correct
    // while `getOwnPropertyDescriptor` still reported the opposite, which is
    // the same class of lie this whole batch is about.
    let kind = ctx.heap.get(obj).kind;
    if matches!(kind, v12_heap::Kind::Array | v12_heap::Kind::Arguments) {
        ctx.heap.get_mut(obj).set_element(i as u32, v);
        return Ok(());
    }
    let key = index_prop(&mut *ctx.heap, i);
    if !crate::internal_methods::dispatch_set(&mut *ctx.heap, obj, key, v, JsValue::object(obj))
        .map_err(Throw::Value)?
    {
        return Err(ctx.type_error("TypeError: cannot assign to read only property"));
    }
    Ok(())
}

/// ES `DeletePropertyOrThrow(O, ToString(i))` — the spec's own step 7/8
/// primitive, used by `pop`/`shift`/`splice`/`sort` and by `write_slot`'s
/// "absent source DELETES the destination" rule.
///
/// # This replaced a second implementation of `[[Delete]]`
///
/// The deleted `index_delete` returned `()` and did the hole-out by hand: the
/// element store for `Kind::Array`, then the receiver's own shape/dictionary
/// storage. It performed **no descriptor check at all**, so
/// `Array.prototype.reverse.call(Object.freeze({0:"a",length:3}))` deleted a
/// non-configurable own property and answered no question — the descriptor was
/// gone while `Object.isFrozen` kept answering `true`, which is the
/// integrity-level invariant `CONTEXT.md` states. The rule was never missing:
/// `Interp::delete_property` (`v12-interp/src/property.rs`) has carried the
/// `configurable` refusal since before this file existed, and the interpreter's
/// own probe (`Reflect.deleteProperty`, and plain `delete` on the same
/// receiver) answers `false` and leaves the object bit-identical. The array
/// builtins were **bypassing existing machinery**, so this calls it
/// ([`internal_methods::dispatch_delete`]) rather than growing a new copy.
///
/// A refused delete leaves the receiver completely untouched: no hole, no
/// descriptor change, no `length` write, because the `?` unwinds before the
/// caller's next step. Asserting only "it throws" cannot tell that apart from
/// "throw, then corrupt" — which is why every row of
/// `a_refused_delete_leaves_the_receiver_uncorrupted` also reads the surviving
/// state.
///
/// Same concession as `index_set`'s `Kind::Array` rung, for the same reason:
/// `ordinary_delete`'s element-store rung has nothing to check either, because
/// the attributes do not exist to check.
fn delete_index_or_throw(ctx: &mut Ctx, obj: Handle<JsObject>, i: i64) -> Result<(), Throw> {
    if i < 0 || i > i64::from(u32::MAX) - 1 {
        return Ok(());
    }
    let key = index_prop(&mut *ctx.heap, i);
    if !crate::internal_methods::dispatch_delete(&mut *ctx.heap, obj, key).map_err(Throw::Value)? {
        return Err(ctx.type_error("TypeError: cannot delete a non-configurable property"));
    }
    Ok(())
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
        // `read_slots` take the array fast path instead, so nothing is
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

/// One index of a whole-array read: `None` is a hole (the receiver does not
/// have the index), `Some(undefined)` is a *present* `undefined`.
///
/// This is the type the shared reader returns, and it is the whole point: a
/// `Vec<JsValue>` cannot tell the two apart, because a hole reads as
/// `undefined`. Every method whose spec output is built with
/// `CreateDataProperty`-per-present-index (`slice`, `concat`, `sort`,
/// `reverse`, `splice`'s removed array, `map`) materialises a hole into an own
/// `undefined` the moment that distinction is dropped — which is exactly what
/// the deleted `Vec<JsValue>` reader did. `Array.from` is the deliberate
/// exception: its spec step is an unconditional `CreateDataProperty` per index,
/// so it *wants* the flattened form and takes it explicitly.
type Slot = Option<JsValue>;

/// `index_get`'s answer as a [`Slot`], so a caller that has the `(value,
/// present)` pair can hand it straight to [`write_slot`] without restating the
/// `if present { set } else { delete }` shape at every site. (A `From` impl
/// would read better but trips the orphan rule: both `Option` and the tuple
/// are foreign to this crate.)
fn slot_of(answer: (JsValue, bool)) -> Slot {
    answer.1.then_some(answer.0)
}

/// `Kind::Array` read: the lattice iterator already yields every index up to
/// its own high-water mark with holes intact. That is both the cheapest read
/// for a real array and the only one that can see an index no shape key names,
/// so it stays as the fast path. Trailing holes are padded out to the
/// requested span — capped by `MAX_SCAN_SPAN` — because `slice(new Array(3))`
/// is a length-3 holey array in the spec and `[]` here.
fn array_slots(heap: &Heap, obj: Handle<JsObject>, start: i64, end: i64) -> Vec<Slot> {
    let all = heap.get(obj).elements_snapshot();
    let lo = start.max(0) as usize;
    let hi = (end.max(start).max(0) as usize).min(all.len());
    let mut out: Vec<Slot> = all[lo.min(hi)..hi]
        .iter()
        .map(|v| (!v.is_hole()).then_some(*v))
        .collect();
    out.resize((end - start).clamp(0, MAX_SCAN_SPAN) as usize, None);
    out
}

/// Reads `start..end` through `index_get`, keeping each index's presence.
///
/// Indices at or past the visible extent skip the dispatcher: they are provably
/// holes, and skipping is what keeps a huge `length` cheap.
fn read_slots(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    start: i64,
    end: i64,
) -> Result<Vec<Slot>, Throw> {
    if heap.get(obj).kind == v12_heap::Kind::Array {
        return Ok(array_slots(heap, obj, start, end));
    }
    let extent = visible_extent(heap, obj);
    let end = end.min(read_limit(end, extent));
    let mut out: Vec<Slot> = Vec::new();
    for i in start..end.max(start) {
        if i >= extent {
            out.push(None);
            continue;
        }
        let (value, present) = index_get(heap, obj, i)?;
        out.push(present.then_some(value));
    }
    Ok(out)
}

/// `Set(O, ToString(i), v)` for a present slot and `Delete(O, ToString(i))` for
/// a hole — the spec's own "absent source deletes the destination" rule, which
/// `shift`/`unshift`/`copyWithin` and now `sort`/`reverse` all need. Writing
/// `undefined` at a hole is the bug this helper exists to make impossible: it
/// leaves an own property the receiver never had.
fn write_slot(ctx: &mut Ctx, obj: Handle<JsObject>, i: i64, slot: Slot) -> Result<(), Throw> {
    match slot {
        Some(v) => index_set(ctx, obj, i, v),
        None => delete_index_or_throw(ctx, obj, i),
    }
}

/// `Set(O, "length", len, true)` — the one length writer, and the other half
/// of defect A: `true` is `Throw`, so a frozen `length` is a `TypeError`.
///
/// The deleted `set_array_len` only ever lowered `length` on `Kind::Array`,
/// which is why `unshift` on an `arguments` object returned the right count
/// and left `length` at the old value, and why a generic receiver's `length`
/// was never lowered at all. Routing the write through `[[Set]]` gives a frozen
/// receiver the spec's refusal — but it gave this helper the BOOLEAN, and the
/// boolean was discarded, so the refusal reached nobody.
///
/// The `Kind::Array` store trim happens AFTER the `[[Set]]`, matching ES
/// `ArraySetLength`, so a refused length write throws having mutated nothing.
fn set_array_len(ctx: &mut Ctx, obj: Handle<JsObject>, len: f64) -> Result<(), Throw> {
    let len = len.min(f64::from(u32::MAX));
    let key = length_prop(&mut *ctx.heap);
    let value = helpers::smi_or_f64(len as i64);
    // The OLD length view, read BEFORE the write. It is the trim's bound, and
    // it has to be: `array_like_len` reads the `length` property, so asking it
    // after the write returns the NEW length and `len < view` is never true —
    // which silently stopped `pop` from trimming at all
    // (`builtin_tests::array_push_and_pop_length` caught it).
    //
    // Read before the write rather than before this function for the same
    // reason the write is before the trim: a refusal must be able to happen
    // before any mutation. Trimming first meant a refused length write on a
    // frozen array threw *after* it had already shortened the element store, so
    // `Object.freeze([1,2,3]).pop()` left `[3,1,2,<hole>]` — a throw that still
    // corrupted, which is the exact shape
    // `a_refused_delete_leaves_the_receiver_uncorrupted` exists to reject.
    let is_array = ctx.heap.get(obj).kind == v12_heap::Kind::Array;
    let old_view = array_like_len(&mut *ctx.heap, obj)?;

    // `Set` BEFORE the store trim, which is ES `ArraySetLength`'s own order
    // (step 1 writes the property, step 2 deletes the indices at/above it).
    let wrote = crate::internal_methods::dispatch_set(
        &mut *ctx.heap,
        obj,
        key,
        value,
        JsValue::object(obj),
    )
    .map_err(Throw::Value)?;
    if !wrote {
        return Err(ctx.type_error("TypeError: cannot assign to read only property"));
    }
    if is_array {
        // `length` is not an array-length exotic here, so lowering the
        // property alone leaves the lattice holding entries past the new
        // `length` — `pop` returned `"c"` and `a[2]` still read `"c"` at
        // `length === 2`.
        //
        // Gated on the length *view*, not on a snapshot: `push` lowers
        // nothing, and a snapshot per `push` would make a 1e5-element push
        // loop quadratic.
        if len < old_view {
            let kept = ctx.heap.get(obj).elements_snapshot();
            let keep = (len.max(0.0) as usize).min(kept.len());
            ctx.heap
                .get_mut(obj)
                .replace_elements(kept[..keep].to_vec());
        }
    }
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

/// [`new_array`] for a read that kept each index's presence: `None` becomes a
/// real hole in the new array's element lattice, so `i in result` is false
/// exactly where the source had no index.
fn new_array_slots(ctx: &mut Ctx, slots: Vec<Slot>) -> JsValue {
    new_array(
        ctx,
        slots
            .into_iter()
            .map(|s| s.unwrap_or_else(JsValue::hole))
            .collect(),
    )
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
    let mut out: Vec<Slot> = Vec::new();
    let this_obj = this.as_object();
    if let Some(obj) = this_obj
        && ctx.heap.get(obj).kind == v12_heap::Kind::Array
    {
        let len = array_like_len(&mut *ctx.heap, obj)? as i64;
        out.extend(read_slots(&mut *ctx.heap, obj, 0, len)?);
    } else {
        out.push(Some(this));
    }
    for &item in args {
        let is_spread = item
            .as_object()
            .is_some_and(|h| ctx.heap.get(h).kind == v12_heap::Kind::Array);
        if is_spread {
            let h = item.as_object().expect("checked spread");
            let len = array_like_len(&mut *ctx.heap, h)? as i64;
            out.extend(read_slots(&mut *ctx.heap, h, 0, len)?);
        } else {
            out.push(Some(item));
        }
    }
    Ok(new_array_slots(ctx, out))
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
    let mut slots = read_slots(&mut *ctx.heap, obj, 0, len)?;
    slots.reverse();
    // Permute presence along with the value: a hole has to travel to its
    // mirrored index and stay a hole. Writing the whole range back — which is
    // all a `Vec<JsValue>` could do — left three present `undefined`s where
    // `reverse(new Array(3))` leaves none.
    for (i, slot) in slots.iter().enumerate() {
        write_slot(ctx, obj, i as i64, *slot)?;
    }
    Ok(this)
}

/// `Array.prototype.shift()` – removes and returns the first element.
pub fn array_shift(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.shift", None)?;
    // No `heap` local: the writers take `&mut Ctx` (they can throw), so the
    // read-only reborrow would have to be re-taken on every line.
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    if len == 0 {
        return Ok(JsValue::undefined());
    }
    let first = index_get(&mut *ctx.heap, obj, 0)?.0;
    // Spec: an absent source index DELETES the destination rather than
    // writing `undefined` — which is why shifting `Object.create({0:"inh",
    // length:3})` leaves no own index behind instead of two `undefined`s.
    for k in 1..len {
        let slot = slot_of(index_get(&mut *ctx.heap, obj, k)?);
        write_slot(ctx, obj, k - 1, slot)?;
    }
    delete_index_or_throw(ctx, obj, len - 1)?;
    set_array_len(ctx, obj, (len - 1) as f64)?;
    Ok(first)
}

/// `Array.prototype.unshift(...items)` – prepends, returns the new length.
pub fn array_unshift(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "Array.prototype.unshift", None)?;
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    let n = args.len() as i64;
    // Back to front, so no read is clobbered by an earlier write.
    for k in (0..len).rev() {
        let slot = slot_of(index_get(&mut *ctx.heap, obj, k)?);
        write_slot(ctx, obj, k + n, slot)?;
    }
    for (i, &item) in args.iter().enumerate() {
        index_set(ctx, obj, i as i64, item)?;
    }
    // No `if kind == Array` guard: the deleted guard is why `unshift` on an
    // `arguments` object returned 4 and left `length` at 3.
    set_array_len(ctx, obj, (len + n) as f64)?;
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
    let len = array_like_len(&mut *ctx.heap, obj)? as i64;
    let start = relative_index(&mut *ctx.heap, args.first().copied(), len, 0)
        .max(0)
        .min(len);
    let delete_count = match args.get(1).copied() {
        None => len - start,
        Some(v) if v.is_undefined() => len - start,
        Some(v) => {
            // W3.2: fourth copy of the same coercion, same replacement.
            let d = super::ctx::heap_to_integer_or_infinity(&mut *ctx.heap, v) as i64;
            d.max(0).min(len - start)
        }
    };
    let items: Vec<JsValue> = args.iter().skip(2).copied().collect();
    let insert_count = items.len() as i64;

    // Spec steps 5-6: the removed array gets one entry per index in
    // `[start, start + deleteCount)`, and an index the receiver does not *have*
    // leaves a HOLE in it — `splice.call(new Array(3), 0, 3)` returns
    // `[<3 empty items>]`, not three `undefined`s.
    let mut removed: Vec<Slot> = Vec::new();
    for j in 0..delete_count.min(MAX_SCAN_SPAN) {
        let (value, present) = index_get(&mut *ctx.heap, obj, start + j)?;
        removed.push(present.then_some(value));
    }

    if insert_count < delete_count {
        // Step 7: close the gap down, then delete the tail that fell out of it.
        for k in start..len - delete_count {
            let slot = slot_of(index_get(&mut *ctx.heap, obj, k + delete_count)?);
            write_slot(ctx, obj, k, slot)?;
        }
        for k in (len - delete_count)..(len - insert_count) {
            delete_index_or_throw(ctx, obj, k)?;
        }
    } else if insert_count > delete_count {
        // Step 8: open a gap of `insert_count - delete_count` and slide the
        // tail up into it, back to front so no read is clobbered.
        let mut k = len - delete_count;
        while k > start {
            let slot = slot_of(index_get(&mut *ctx.heap, obj, k + delete_count - 1)?);
            write_slot(ctx, obj, k + insert_count - 1, slot)?;
            k -= 1;
        }
    }

    for (i, &item) in items.iter().enumerate() {
        index_set(ctx, obj, start + i as i64, item)?;
    }
    set_array_len(ctx, obj, (len - delete_count + insert_count).max(0) as f64)?;
    Ok(new_array_slots(ctx, removed))
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
        index_set(ctx, obj, i, value)?;
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
    let mut src: Vec<Slot> = Vec::with_capacity(count.max(0) as usize);
    for k in 0..count {
        src.push(slot_of(index_get(heap, obj, start + k)?));
    }
    for (k, slot) in src.into_iter().enumerate() {
        // An absent source deletes the destination; writing `undefined` there
        // would leave an own property the spec never creates.
        write_slot(ctx, obj, target + k as i64, slot)?;
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
    // `read_slots` fills them as `None` without dispatching, and the length
    // property is fixed afterwards so the result still reads `length === len`.
    // The visible extent includes the prototype chain, so
    // `Array.from(Object.create({0:"inh", length:3}))` sees the inherited
    // index — the deleted `dense_bound` saw own shape keys only.
    //
    // `Array.from` is the one caller that FLATTENS the slots: its spec step is
    // an unconditional `CreateDataProperty(O, k, Get(next, k))` for every
    // `k < len`, so `Array.from(new Array(3))` has three own `undefined`s where
    // `slice` produces three holes.
    let items = read_slots(heap, obj, 0, len)?
        .into_iter()
        .map(|s| s.unwrap_or_else(JsValue::undefined))
        .collect();
    let arr_v = new_array(ctx, items);
    if let Some(arr) = arr_v.as_object() {
        set_array_len(ctx, arr, len as f64)?;
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
