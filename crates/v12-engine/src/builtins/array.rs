//! Array built-ins: push, pop, and length handling.

use v12_heap::{Handle, Heap, JsObject, JsValue, PropKey, V12Str};
use v12_native::Throw;

use super::{helpers, intern_type_error};

/// Maximum array length (2^32 - 1).
const MAX_ARRAY_LENGTH: u32 = u32::MAX;

/// Length property key, interned lazily via the heap.
fn length_prop(heap: &mut Heap) -> v12_heap::PropKey {
    let h = heap.intern_string(V12Str::latin1(b"length".to_vec()));
    v12_heap::PropKey::from_string(h)
}

/// `Array.prototype.push(...items)` – appends elements and updates `length`.
pub fn array_push(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.push", None)?;
    let len = array_length(heap, obj);
    if len as usize + args.len() > MAX_ARRAY_LENGTH as usize {
        return Err((intern_type_error(heap, "RangeError: invalid array length")).into());
    }
    for &item in args {
        heap.get_mut(obj).push_element(item);
    }
    let new_len = heap.get(obj).element_len() as u32;
    sync_length(heap, obj, new_len);
    Ok(helpers::smi_or_f64(i64::from(new_len)))
}

/// `Array.prototype.pop()` – removes the last element.
pub fn array_pop(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.pop", None)?;
    let popped = heap.get_mut(obj).pop_element().unwrap_or(JsValue::undefined());
    let value = if popped.is_hole() {
        JsValue::undefined()
    } else {
        popped
    };
    let new_len = heap.get(obj).element_len() as u32;
    sync_length(heap, obj, new_len);
    Ok(value)
}

fn array_length(heap: &mut Heap, obj: Handle<JsObject>) -> u32 {
    let key = length_prop(heap);
    let shape = heap.root_shape();
    if let Some(desc) = heap.lookup_property(shape, key)
        && let Some(slot) = desc.slot()
    {
        let idx = slot as usize;
        let v = heap
            .get(obj)
            .properties
            .get(idx)
            .copied()
            .unwrap_or(JsValue::undefined());
        if let Some(n) = v.as_smi()
            && let Ok(u) = u32::try_from(n)
        {
            return u;
        }
        if let Some(n) = v.as_f64()
            && n.is_finite()
            && n >= 0.0
            && n.fract() == 0.0
        {
            return n as u32;
        }
    }
    // Fallback to elements length.
    heap.get(obj).element_len() as u32
}

/// `Array.isArray(value)` – true if value is an Array exotic object.
pub fn array_is_array(_heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is = v
        .as_object()
        .is_some_and(|h| _heap.get(h).kind == v12_heap::Kind::Array);
    Ok(JsValue::from_bool(is))
}

pub fn array_slice(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.slice", None)?;
    let elems: Vec<JsValue> = heap.get(obj).elements_snapshot();
    let len = elems.len() as i64;
    let to_idx = |v: JsValue| -> i64 {
        if v.is_undefined() { return 0; }
        
        v.as_smi().map(i64::from).or_else(|| v.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0)
    };
    let start = if args.is_empty() { 0 } else { let n=to_idx(args[0]); if n<0 { (len+n).max(0) } else { n.min(len) } };
    let end = if args.len()<2 || args[1].is_undefined() { len } else { let n=to_idx(args[1]); if n<0 { (len+n).max(0) } else { n.min(len) } };
    let slice = if start>=end { Vec::new() } else { elems[start as usize..end as usize].to_vec() };
    let arr = heap.alloc(JsObject::array(slice));
    heap.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

pub fn array_sort(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.sort", None)?;
    let mut elems: Vec<JsValue> = heap.get(obj).elements_snapshot();
    // Filter holes (undefined) to end per spec simplified: keep order for undefined.
    elems.retain(|v| !v.is_undefined() && !v.is_hole());
    // `value_text` allocates per call; sort_by_cached_key computes each key once.
    elems.sort_by_cached_key(|a| helpers::value_text(heap, *a));
    heap.get_mut(obj).replace_elements(elems);
    Ok(this)
}

fn sync_length(heap: &mut Heap, obj: Handle<JsObject>, len: u32) {
    let key = length_prop(heap);
    let shape = heap.root_shape();
    if let Some(desc) = heap.lookup_property(shape, key)
        && let Some(slot) = desc.slot()
    {
        let idx = slot as usize;
        if idx < heap.get(obj).properties.len() {
            heap.get_mut(obj).properties[idx] = JsValue::from_f64(f64::from(len));
            return;
        }
    }
    // No length slot yet: create one via shape extension if needed.
    if heap.get(obj).properties.len() < 1024 {
        let _child = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
        heap.get_mut(obj)
            .properties
            .push(JsValue::from_f64(f64::from(len)));
    }
}

// ---------------------------------------------------------------------------
// Generic index/length plumbing
//
// Array methods must work on both real arrays (`elements_array`) and generic
// array-like receivers (`elements` vec / own `length` property), so reads and
// writes route through these helpers instead of touching the stores directly.
// ---------------------------------------------------------------------------

/// Reads own index `i`: arrays use the element lattice; ordinary objects use
/// the flat `elements` vec first, then integer-indexed *shape* properties
/// (`{0: 5}` binds `"0"` through the shape, not the element store).
fn read_index(heap: &mut Heap, obj: Handle<JsObject>, i: u32) -> Option<JsValue> {
    if heap.get(obj).kind == v12_heap::Kind::Array {
        return heap.get(obj).get_element(i);
    }
    if let Some(v) = heap
        .get(obj)
        .elements
        .get(i as usize)
        .filter(|v| !v.is_hole())
        .copied()
    {
        return Some(v);
    }
    let key = {
        let h = heap.intern_string(V12Str::latin1_slice(i.to_string().as_bytes()));
        PropKey::from_string(h)
    };
    let shape = heap.shape_of(obj);
    let slot = heap.lookup_property(shape, key)?.slot()?;
    heap.get(obj).properties.get(slot as usize).copied()
}

/// Writes index `i` (lattice promotion for arrays, resize for ordinary).
fn write_index(heap: &mut Heap, obj: Handle<JsObject>, i: u32, v: JsValue) {
    heap.get_mut(obj).set_element(i, v);
}

/// The highest index an element read can return data for; beyond it every
/// read is a hole (or, for `includes`, `undefined`). Arrays keep indices in
/// the element store; ordinary objects may also carry them as shape-bound
/// integer keys, so take the max of the two. Bounds the huge-length scan
/// loops that would otherwise spin billions of no-op iterations.
fn dense_bound(heap: &mut Heap, obj: Handle<JsObject>, len: i64) -> i64 {
    let mut bound = heap.get(obj).element_len() as i64;
    if heap.get(obj).kind != v12_heap::Kind::Array {
        let shape = heap.shape_of(obj);
        // Collect the keys first: decoding each key's text needs `&mut heap`
        // and cannot hold the descriptor borrow across it.
        let keys: Vec<_> = heap.get(shape).descriptors.as_slice().iter().filter_map(|d| d.key().string()).collect();
        for key in keys {
            let text = helpers::string_text(heap, key);
            if text.len() <= 10 && !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
            {
                if let Ok(n) = text.parse::<i64>() {
                    bound = bound.max(n + 1);
                }
            }
        }
    }
    bound.clamp(0, len)
}

/// The receiver's `length`: `properties[0]` for arrays (the array-layout
/// contract), the own `length` property for generic receivers, else the
/// element count.
fn array_len(heap: &mut Heap, obj: Handle<JsObject>) -> u32 {
    if heap.get(obj).kind == v12_heap::Kind::Array {
        if let Some(&v) = heap.get(obj).properties.first() {
            let n = v.as_smi().map(f64::from).or(v.as_f64()).unwrap_or(f64::NAN);
            if n.is_finite() && n >= 0.0 {
                return n.min(f64::from(u32::MAX)) as u32;
            }
        }
        return heap.get(obj).element_len() as u32;
    }
    let key = length_prop(heap);
    let shape = heap.shape_of(obj);
    if let Some(desc) = heap.lookup_property(shape, key)
        && let Some(slot) = desc.slot()
        && let Some(&v) = heap.get(obj).properties.get(slot as usize)
    {
        let n = v.as_smi().map(f64::from).or(v.as_f64()).unwrap_or(f64::NAN);
        if n.is_finite() && n > 0.0 {
            return n.min(f64::from(u32::MAX)) as u32;
        }
        return 0;
    }
    heap.get(obj).element_len() as u32
}

/// Updates an array's `length` slot and shrinks the element store when the
/// new length is shorter (holes mark the dropped tail).
fn set_array_len(heap: &mut Heap, obj: Handle<JsObject>, len: u32) {
    if heap.get(obj).kind == v12_heap::Kind::Array {
        let old = heap.get(obj).element_len() as u32;
        if len < old {
            let kept: Vec<JsValue> = heap.get(obj).elements_snapshot();
            heap.get_mut(obj).replace_elements(kept[..len as usize].to_vec());
        }
        if let Some(slot) = heap.get_mut(obj).properties.first_mut() {
            *slot = helpers::smi_or_f64(i64::from(len));
            return;
        }
    }
    // Generic receivers keep `length` as an ordinary property; syncing it
    // must not materialize the flat store toward a huge `length` (a single
    // `resize` toward 2^32 entries OOMs the runner). Only touch the store
    // near actually-stored data.
    let store_len = heap.get(obj).element_len() as u64;
    if u64::from(len) <= store_len + MAX_WRITE_SPAN as u64 {
        write_index(heap, obj, len.saturating_sub(1), JsValue::undefined());
    }
}

/// Relative-index normalization shared by `at`/`slice`-style methods:
/// negative values count from `len`, the result is clamped to `[0, len]`.
fn relative_index(v: Option<JsValue>, len: i64, default: i64) -> i64 {
    let Some(v) = v else { return default };
    if v.is_undefined() {
        return default;
    }
    let n = v.as_smi().map(i64::from).or_else(|| v.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0);
    if n < 0 { (len + n).max(0) } else { n.min(len) }
}

fn new_array(heap: &mut Heap, elements: Vec<JsValue>) -> JsValue {
    let arr = heap.alloc(JsObject::array(elements));
    heap.add_root(JsValue::object(arr));
    JsValue::object(arr)
}

/// `Array.prototype.indexOf(search, fromIndex?)` – strict-equality scan;
/// holes are skipped (they never match, even `undefined`).
pub fn array_index_of(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.indexOf", None)?;
    let search = args.first().copied().unwrap_or(JsValue::undefined());
    let len = i64::from(array_len(heap, obj));
    // Real arrays scan present entries only: the element store can be far
    // shorter than the length view (huge/sparse receivers), and ranging over
    // `0..len` would visit billions of holes. Holes never match, so entries
    // outside `[from, len)` cannot contribute either.
    if heap.get(obj).kind == v12_heap::Kind::Array {
        let from = relative_index(args.get(1).copied(), len, 0).max(0);
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
    // Holes never match, so scanning past the last stored element (or
    // shape-bound integer key) cannot find anything — bound the loop.
    let dense = dense_bound(heap, obj, len);
    let from = relative_index(args.get(1).copied(), len, 0).max(0).min(dense);
    for i in from..dense {
        if let Some(v) = read_index(heap, obj, i as u32)
            && helpers::strict_equals(heap, search, v)
        {
            return Ok(helpers::js_number(f64::from(i as u32)));
        }
    }
    Ok(JsValue::from_i32_smi(-1).unwrap_or_else(|| JsValue::from_f64(-1.0)))
}

/// `Array.prototype.lastIndexOf(search, fromIndex?)` – backwards scan.
pub fn array_last_index_of(
    heap: &mut Heap,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.lastIndexOf", None)?;
    let search = args.first().copied().unwrap_or(JsValue::undefined());
    let len = i64::from(array_len(heap, obj));
    // Entry scan like `indexOf`: only present elements can match, so a
    // huge length view never widens the walk.
    if heap.get(obj).kind == v12_heap::Kind::Array {
        let from = relative_index(args.get(1).copied(), len, len - 1);
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
    let dense = dense_bound(heap, obj, len);
    let mut from = relative_index(args.get(1).copied(), len, len - 1).min(dense - 1);
    if from >= dense {
        from = dense - 1;
    }
    let mut i = from;
    while i >= 0 {
        if let Some(v) = read_index(heap, obj, i as u32)
            && helpers::strict_equals(heap, search, v)
        {
            return Ok(helpers::js_number(f64::from(i as u32)));
        }
        i -= 1;
    }
    Ok(JsValue::from_i32_smi(-1).unwrap_or_else(|| JsValue::from_f64(-1.0)))
}

/// `Array.prototype.includes(search, fromIndex?)` – `SameValueZero` scan
/// (NaN matches NaN); holes compare as `undefined`.
pub fn array_includes(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.includes", None)?;
    let search = args.first().copied().unwrap_or(JsValue::undefined());
    let len = i64::from(array_len(heap, obj));
    // Entry scan: present elements compare by value; any hole in
    // `[from, len)` reads as `undefined`, so an `undefined` search hits when
    // the range holds fewer present entries than indices.
    if heap.get(obj).kind == v12_heap::Kind::Array {
        let from = relative_index(args.get(1).copied(), len, 0).max(0);
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
    let dense = dense_bound(heap, obj, len);
    let from = relative_index(args.get(1).copied(), len, 0).max(0).min(dense);
    for i in from..dense {
        match read_index(heap, obj, i as u32) {
            Some(v) if helpers::same_value_zero(heap, search, v) => return Ok(JsValue::from_bool(true)),
            // A hole reads as `undefined` for `includes`.
            None if search.is_undefined() => return Ok(JsValue::from_bool(true)),
            _ => {}
        }
    }
    // Every index at or beyond the dense bound reads as a hole, i.e.
    // `undefined` — a huge sparse receiver still includes `undefined`.
    if search.is_undefined() && dense < len {
        return Ok(JsValue::from_bool(true));
    }
    Ok(JsValue::from_bool(false))
}

/// `Array.prototype.concat(...items)` – array arguments contribute their
/// elements (holes preserved), everything else is appended as-is.
pub fn array_concat(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut out: Vec<JsValue> = Vec::new();
    let this_obj = this.as_object();
    if let Some(obj) = this_obj {
        out.extend(heap.get(obj).elements_snapshot());
    }
    for &item in args {
        let is_spread = item
            .as_object()
            .is_some_and(|h| heap.get(h).kind == v12_heap::Kind::Array);
        if is_spread {
            let h = item.as_object().expect("checked spread");
            out.extend(heap.get(h).elements_snapshot());
        } else {
            out.push(item);
        }
    }
    Ok(new_array(heap, out))
}

/// `Array.prototype.at(index)` – relative indexing, out-of-range → undefined.
pub fn array_at(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.at", None)?;
    let len = i64::from(array_len(heap, obj));
    let idx = relative_index(args.first().copied(), len, 0);
    if idx < 0 || idx >= len {
        return Ok(JsValue::undefined());
    }
    Ok(read_index(heap, obj, idx as u32).unwrap_or(JsValue::undefined()))
}

/// `Array.prototype.reverse()` – in place, returns the receiver.
pub fn array_reverse(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.reverse", None)?;
    let mut elems: Vec<JsValue> = heap.get(obj).elements_snapshot();
    elems.reverse();
    heap.get_mut(obj).replace_elements(elems);
    Ok(this)
}

/// `Array.prototype.shift()` – removes and returns the first element.
pub fn array_shift(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.shift", None)?;
    let len = array_len(heap, obj);
    if len == 0 {
        return Ok(JsValue::undefined());
    }
    let first = read_index(heap, obj, 0).unwrap_or(JsValue::undefined());
    // The element store can be empty while the length property is nonzero
    // (sparse arrays): shifting then only moves the length counter.
    let mut elems: Vec<JsValue> = heap.get(obj).elements_snapshot();
    if !elems.is_empty() {
        elems.remove(0);
        heap.get_mut(obj).replace_elements(elems);
    }
    set_array_len(heap, obj, len - 1);
    Ok(first)
}

/// `Array.prototype.unshift(...items)` – prepends, returns the new length.
pub fn array_unshift(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.unshift", None)?;
    let mut elems: Vec<JsValue> = heap.get(obj).elements_snapshot();
    elems.splice(0..0, args.iter().copied());
    let len = elems.len();
    heap.get_mut(obj).replace_elements(elems);
    if heap.get(obj).kind == v12_heap::Kind::Array {
        set_array_len(heap, obj, len as u32);
    }
    Ok(helpers::js_number(f64::from(len as u32)))
}

/// `Array.prototype.splice(start, deleteCount?, ...items)` – removes and
/// inserts, returning the removed elements.
pub fn array_splice(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.splice", None)?;
    let len = i64::from(array_len(heap, obj));
    let start = relative_index(args.first().copied(), len, 0).max(0);
    let delete_count = match args.get(1).copied() {
        None => len - start,
        Some(v) if v.is_undefined() => len - start,
        Some(v) => {
            let d = v.as_smi().map(i64::from).or_else(|| v.as_f64().map(|f| f.trunc() as i64)).unwrap_or(0);
            d.max(0).min(len - start)
        }
    };
    let elems: Vec<JsValue> = heap.get(obj).elements_snapshot();
    // The element store can be shorter than the length property (huge-length
    // arrays, sparse arrays): clamp every index to the real snapshot.
    let start = (start as usize).min(elems.len());
    let delete_count = (delete_count as usize).min(elems.len() - start);
    let removed: Vec<JsValue> = elems[start..start + delete_count].to_vec();
    let mut next: Vec<JsValue> = Vec::with_capacity(elems.len());
    next.extend_from_slice(&elems[..start]);
    next.extend(args.iter().skip(2).copied());
    next.extend_from_slice(&elems[start + delete_count..]);
    heap.get_mut(obj).replace_elements(next);
    set_array_len(heap, obj, (len - delete_count as i64 + args.len().saturating_sub(2) as i64) as u32);
    Ok(new_array(heap, removed))
}

/// Upper bound on entries a single `fill`/`copyWithin` call materializes in
/// the element store. Realistic tests stay far below it; a huge `length`
/// property on a sparse receiver clamps to the stored region instead of
/// hanging the runner or growing the store toward 2^32 entries (OOM).
const MAX_WRITE_SPAN: i64 = 1_000_000;

/// `Array.prototype.fill(value, start?, end?)`.
pub fn array_fill(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.fill", None)?;
    let len = i64::from(array_len(heap, obj));
    let start = relative_index(args.get(1).copied(), len, 0).max(0);
    let end = relative_index(args.get(2).copied(), len, len).max(0);
    let value = args.first().copied().unwrap_or(JsValue::undefined());
    // A huge `length` on a sparse receiver would materialize billions of
    // entries: write the stored region plus bounded growth. Full arrays
    // (store length == length) always take the exact path below.
    let store_len = heap.get(obj).element_len() as i64;
    let mut hi = end;
    if hi - start > MAX_WRITE_SPAN || end > store_len + MAX_WRITE_SPAN {
        hi = start.max(
            dense_bound(heap, obj, len)
                .min(end)
                .min(store_len + MAX_WRITE_SPAN),
        );
    }
    for i in start..hi {
        write_index(heap, obj, i as u32, value);
    }
    Ok(this)
}

/// `Array.prototype.copyWithin(target, start, end?)` – interior memmove.
pub fn array_copy_within(
    heap: &mut Heap,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.copyWithin", None)?;
    let len = i64::from(array_len(heap, obj));
    let target = relative_index(args.first().copied(), len, 0).max(0);
    let start = relative_index(args.get(1).copied(), len, 0).max(0);
    let end = relative_index(args.get(2).copied(), len, len).max(0);
    if target >= len || start >= end {
        return Ok(this);
    }
    let count = (end - start).min(len - target);
    let dense = dense_bound(heap, obj, len);
    let store_len = heap.get(obj).element_len() as i64;
    // The span only needs to cover source indices holding data and dest
    // indices already stored; beyond both, every read is a hole and the
    // write just grows the store toward a huge `length` (hang/OOM).
    let live = (dense.min(end) - start)
        .max(0)
        .max((store_len - target).max(0));
    let count = count.min(live);
    // Fresh growth past the store stays bounded even when clamping above
    // leaves a far-away destination (e.g. target near 2^32 on a 1-element
    // receiver): holes past the store read back as holes either way.
    let cap = store_len + MAX_WRITE_SPAN;
    let elems: Vec<JsValue> = heap.get(obj).elements_snapshot();
    for k in 0..count {
        let d = target + k;
        let v = elems.get((start + k) as usize).copied().unwrap_or(JsValue::hole());
        if d >= store_len && (v.is_hole() || d > cap) {
            continue;
        }
        write_index(heap, obj, d as u32, v);
    }
    Ok(this)
}

/// `Array.prototype.flat(depth?)` – flattens nested arrays to `depth`
/// (default 1); holes read as `undefined`.
pub fn array_flat(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.flat", None)?;
    let depth = match args.first().copied() {
        None => 1.0,
        Some(v) if v.is_undefined() => 1.0,
        Some(v) => helpers::to_number(heap, v),
    };
    let mut out: Vec<JsValue> = Vec::new();
    flatten_into(heap, obj, depth, &mut out);
    Ok(new_array(heap, out))
}

fn flatten_into(heap: &mut Heap, obj: Handle<JsObject>, depth: f64, out: &mut Vec<JsValue>) {
    for v in heap.get(obj).elements_snapshot() {
        if v.is_hole() {
            out.push(JsValue::undefined());
            continue;
        }
        let nested = v
            .as_object()
            .filter(|h| heap.get(*h).kind == v12_heap::Kind::Array);
        if let (Some(h), true) = (nested, depth >= 1.0) {
            flatten_into(heap, h, depth - 1.0, out);
        } else {
            out.push(v);
        }
    }
}

/// `Array.prototype.toString()` – `join()` with the default separator.
pub fn array_to_string(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    super::array_join(heap, this, &[])
}

/// `Array.of(...items)` – a new array from the argument list.
pub fn array_of(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(new_array(heap, args.to_vec()))
}

/// `Array.from(arrayLike)` – array-likes (via `length` + indexed reads) and
/// strings (per code point). Iterable objects need the interpreter's
/// iterator protocol and are not supported by this native path.
pub fn array_from(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let Some(&source) = args.first() else {
        return Ok(new_array(heap, Vec::new()));
    };
    if let Some(h) = source.as_string() {
        let text = helpers::string_text(heap, h);
        let chars: Vec<JsValue> = text
            .chars()
            .map(|c| JsValue::string(heap.intern_text(&c.to_string())))
            .collect();
        return Ok(new_array(heap, chars));
    }
    let Some(obj) = source.as_object() else {
        return Ok(new_array(heap, Vec::new()));
    };
    let len = i64::from(array_len(heap, obj));
    // A huge `length` property on a sparse receiver describes mostly holes;
    // materialize only the stored elements (holes read as undefined) and fix
    // the length property afterwards so the shape of the result still reads
    // `length === len`.
    let dense = dense_bound(heap, obj, len);
    let items: Vec<JsValue> = (0..dense)
        .map(|i| read_index(heap, obj, i as u32).unwrap_or(JsValue::undefined()))
        .collect();
    let arr_v = new_array(heap, items);
    if let Some(arr) = arr_v.as_object() {
        set_array_len(heap, arr, len as u32);
    }
    Ok(arr_v)
}
