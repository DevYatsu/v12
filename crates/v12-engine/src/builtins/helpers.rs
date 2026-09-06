//! Shared conversions and validation helpers for the built-in natives.
//!
//! Every built-in has the same prologue — check `this` is the right kind of
//! object, extract string text, build an smi-or-double, allocate + root — and
//! each file used to hand-roll its own copy. These helpers are the single
//! implementation; the native handlers call them and stay focused on their
//! own semantics.

use std::borrow::Cow;

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

/// The string text of a heap string, borrowing when possible.
///
/// Flattens first, then borrows the Latin-1 bytes as `&str` when they form
/// valid UTF-8 (ASCII always does) — zero allocation; invalid Latin-1 or
/// UTF-16 storage falls back to an owned lossy conversion. Prefer this over
/// [`string_text`] when the text is consumed before the next heap access —
/// the returned `Cow` holds the heap until dropped.
pub fn string_text_cow<'a>(heap: &'a mut Heap, h: Handle<V12Str>) -> Cow<'a, str> {
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes),
        v12_heap::StrStorage::Utf16(units) => Cow::Owned(String::from_utf16_lossy(units)),
        _ => Cow::Borrowed(""),
    }
}

/// The string text of a heap string, flattened and lossy-converted.
pub fn string_text(heap: &mut Heap, h: Handle<V12Str>) -> String {
    string_text_cow(heap, h).into_owned()
}

/// The text of a value, borrowing the string storage when possible (see
/// [`string_text_cow`] for the borrow/allocate contract).
pub fn value_text_cow<'a>(heap: &'a mut Heap, v: JsValue) -> Cow<'a, str> {
    if let Some(h) = v.as_string() {
        return string_text_cow(heap, h);
    }
    Cow::Owned(display_text(v))
}

/// The text of a value: strings render their text, everything else renders
/// the way `console.log` observes it (Tier-0 display subset).
pub fn value_text(heap: &mut Heap, v: JsValue) -> String {
    // Real arrays render as their comma-joined elements (so `map` results
    // don't display as `[object Object]` in `console.log`).
    if let Some(obj) = v.as_object()
        && heap.get(obj).kind == v12_heap::Kind::Array
    {
        return array_join_text(heap, obj, 0);
    }
    value_text_cow(heap, v).into_owned()
}

/// Comma-joined element text of a real array (`undefined`/`null`/holes
/// render empty, matching `Array.prototype.join`). Nested arrays recurse;
/// `depth` caps the recursion so cyclic arrays terminate.
fn array_join_text(heap: &mut Heap, obj: Handle<JsObject>, depth: usize) -> String {
    if depth > 8 {
        return String::new();
    }
    // Snapshot before formatting: rendering an element may allocate (and
    // thus collect), invalidating a live borrow of the element store.
    let elements: Vec<JsValue> = heap.get(obj).elements_snapshot();
    let mut parts = Vec::with_capacity(elements.len());
    for v in elements {
        if v.is_undefined() || v.is_null() || v.is_hole() {
            parts.push(String::new());
        } else if let Some(nested) = v
            .as_object()
            .filter(|h| heap.get(*h).kind == v12_heap::Kind::Array)
        {
            parts.push(array_join_text(heap, nested, depth + 1));
        } else if let Some(h) = v.as_string() {
            parts.push(string_text(heap, h));
        } else {
            parts.push(display_text(v));
        }
    }
    parts.join(",")
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
        let text = string_text_cow(heap, h);
        let trimmed = text.trim();
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

/// ES `IsStrictlyEqual` subset (no user code): same-type numeric, string
/// (interned-identity/textual), boolean, bigint, symbol, and object-identity
/// comparison; special values compare by bit identity.
pub fn strict_equals(heap: &Heap, a: JsValue, b: JsValue) -> bool {
    if let (Some(x), Some(y)) = (a.as_smi().map(f64::from).or(a.as_f64()), b.as_smi().map(f64::from).or(b.as_f64())) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_string(), b.as_string()) {
        return heap.strings_equal(x, y);
    }
    if let (Some(x), Some(y)) = (a.as_bool(), b.as_bool()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_bigint(), b.as_bigint()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_symbol(), b.as_symbol()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_object(), b.as_object()) {
        return x == y;
    }
    a.bits() == b.bits() && (a.is_undefined() || a.is_null())
}

/// ES `SameValueZero`: like [`strict_equals`] but `NaN` equals `NaN`.
pub fn same_value_zero(heap: &Heap, a: JsValue, b: JsValue) -> bool {
    if a.as_smi().is_none()
        && a.as_f64().is_some()
        && b.as_f64().is_some()
        && a.as_f64().map(f64::is_nan).unwrap_or(false)
        && b.as_f64().map(f64::is_nan).unwrap_or(false)
    {
        return true;
    }
    strict_equals(heap, a, b)
}
