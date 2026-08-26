//! String built-ins.

use v12_heap::{Handle, Heap, JsValue, V12Str};

use super::intern_type_error;

/// `String.prototype.charAt(index)` – returns a single-character string.
pub fn string_char_at(
    heap: &mut Heap,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsValue> {
    let Some(handle) = this.as_string() else {
        return Err(intern_type_error(
            heap,
            "TypeError: String.prototype.charAt called on non-string",
        ));
    };
    let index = args.first().and_then(to_index).unwrap_or(0);
    let units = string_units(heap, handle);
    if let Some(&unit) = units.get(index as usize) {
        let h = heap.intern_string(V12Str::utf16(vec![unit]));
        Ok(JsValue::string(h))
    } else {
        let h = heap.intern_string(V12Str::latin1(Vec::new()));
        Ok(JsValue::string(h))
    }
}

/// `String.prototype.slice(start, end)` – returns a sliced view.
pub fn string_slice(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let Some(handle) = this.as_string() else {
        return Err(intern_type_error(
            heap,
            "TypeError: String.prototype.slice called on non-string",
        ));
    };
    let len = heap.get(handle).len() as i64;
    let start = args.first().and_then(to_integer).unwrap_or(0);
    let end = args.get(1).and_then(to_integer).unwrap_or(len);
    let from = clamp_index(start, len) as u32;
    let to = clamp_index(end, len) as u32;
    let (from, to) = if from > to { (to, to) } else { (from, to) };
    let slice_len = to.saturating_sub(from);
    let Some(sliced) = heap.slice_string(handle, from, slice_len) else {
        let h = heap.intern_string(V12Str::latin1(Vec::new()));
        return Ok(JsValue::string(h));
    };
    // Flatten lazily sliced strings when eagerly queried, otherwise keep lazy.
    // For the built-in return value, keep as heap handle.
    Ok(JsValue::string(sliced))
}

fn to_index(v: &JsValue) -> Option<i64> {
    if let Some(n) = v.as_smi() {
        return Some(i64::from(n));
    }
    if let Some(n) = v.as_f64()
        && n.is_finite()
    {
        return Some(n.trunc() as i64);
    }
    None
}

fn to_integer(v: &JsValue) -> Option<i64> {
    to_index(v)
}

fn clamp_index(index: i64, len: i64) -> i64 {
    if index < 0 {
        (len + index).max(0)
    } else {
        index.min(len)
    }
}

fn string_units(heap: &mut Heap, handle: Handle<V12Str>) -> Vec<u16> {
    heap.flatten(handle);
    match &heap.get(handle).storage {
        v12_heap::StrStorage::Latin1(bytes) => bytes.iter().map(|&b| u16::from(b)).collect(),
        v12_heap::StrStorage::Utf16(units) => units.clone(),
        _ => Vec::new(),
    }
}
