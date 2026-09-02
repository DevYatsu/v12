//! Shared conversions and validation helpers for the built-in natives.
//!
//! Every built-in has the same prologue — check `this` is the right kind of
//! object, extract string text, build an smi-or-double, allocate + root — and
//! each file used to hand-roll its own copy. These helpers are the single
//! implementation; the native handlers call them and stay focused on their
//! own semantics.

use v12_heap::{Handle, Heap, JsObject, JsValue, V12Str};
use v12_native::Throw;

/// The receiver for a built-in method, checked against `this`.
///
/// Returns a `TypeError` naming `method` when `this` is not an object or not
/// of `kind` (when given). This is the one-line replacement for the old
/// `let Some(obj) = this.as_object() else { return Err(…non-object…) }` plus
/// the separate `kind` re-check.
pub fn as_object(
    heap: &mut Heap,
    this: JsValue,
    method: &str,
    kind: Option<v12_heap::Kind>,
) -> Result<Handle<JsObject>, Throw> {
    let Some(obj) = this.as_object() else {
        return Err(Throw::type_error(
            heap,
            format!("TypeError: {method} called on non-object"),
        ));
    };
    if let Some(kind) = kind
        && heap.get(obj).kind != kind
    {
        return Err(Throw::type_error(
            heap,
            format!("TypeError: {method} called on non-{kind:?}"),
        ));
    }
    Ok(obj)
}

/// The string text of a heap string, flattened and lossy-converted.
pub fn string_text(heap: &mut Heap, h: Handle<V12Str>) -> String {
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
        _ => String::new(),
    }
}

/// The text of a value: strings render their text, everything else renders
/// the way `console.log` observes it (Tier-0 display subset).
pub fn value_text(heap: &mut Heap, v: JsValue) -> String {
    if let Some(h) = v.as_string() {
        return string_text(heap, h);
    }
    display_text(v)
}

/// A number value: a Smi when integral and in Smi range, a double otherwise.
pub fn smi_or_f64(n: i64) -> JsValue {
    JsValue::from_i32_smi(n as i32).unwrap_or_else(|| JsValue::from_f64(n as f64))
}

/// Allocates an object and roots it (every engine-created object that can
/// outlive the current stack frame must be rooted; the natives all do).
pub fn alloc_obj(heap: &mut Heap, obj: JsObject) -> Handle<JsObject> {
    let h = heap.alloc(obj);
    heap.add_root(JsValue::object(h));
    h
}

/// `console.log`-style display text for a non-string value (Tier-0 subset).
/// Strings route through [`value_text`]; this covers everything else.
fn display_text(v: JsValue) -> String {
    if let Some(number) = v.as_smi().map(f64::from).or(v.as_f64()) {
        if number.is_nan() {
            return "NaN".to_string();
        }
        if number == f64::INFINITY {
            return "Infinity".to_string();
        }
        if number == f64::NEG_INFINITY {
            return "-Infinity".to_string();
        }
        return format!("{number}");
    }
    if v.is_true() {
        return "true".to_string();
    }
    if v.is_false() {
        return "false".to_string();
    }
    if v.is_undefined() {
        return "undefined".to_string();
    }
    if v.is_null() {
        return "null".to_string();
    }
    if v.is_object() {
        return "[object Object]".to_string();
    }
    "<unprintable>".to_string()
}

/// Interns a string value in the heap (Latin1 when ASCII, UTF-16 otherwise).
pub fn intern_text(heap: &mut Heap, text: &str) -> v12_heap::Handle<V12Str> {
    if text.is_ascii() {
        heap.intern_string(V12Str::latin1(text.as_bytes().to_vec()))
    } else {
        heap.intern_string(V12Str::utf16(text.encode_utf16().collect()))
    }
}

/// ES `ToNumber` subset: Smi/double pass through; `true`→1.0, `false`/`null`→0.0,
/// `undefined`→NaN; a string is trimmed (empty→0.0, else parsed as f64, failure→NaN);
/// objects → NaN. Reused by all numeric built-ins (DRY).
pub fn to_number(heap: &mut Heap, v: JsValue) -> f64 {
    if let Some(n) = v.as_smi().map(f64::from) {
        return n;
    }
    if let Some(n) = v.as_f64() {
        return n;
    }
    if v.is_true() {
        return 1.0;
    }
    if v.is_false() || v.is_null() {
        return 0.0;
    }
    if let Some(h) = v.as_string() {
        let trimmed = string_text(heap, h);
        let trimmed = trimmed.trim();
        if trimmed.is_empty() {
            return 0.0;
        }
        return trimmed.parse::<f64>().unwrap_or(f64::NAN);
    }
    f64::NAN
}

/// Canonicalizes an f64 to a JavaScript number value: an integral value within
/// Smi range becomes a Smi, anything else stays a double.
pub fn js_number(n: f64) -> JsValue {
    if n.fract() == 0.0
        && n >= f64::from(JsValue::SMI_MIN)
        && n <= f64::from(JsValue::SMI_MAX)
        && let Some(smi) = JsValue::from_i32_smi(n as i32)
    {
        return smi;
    }
    JsValue::from_f64(n)
}
