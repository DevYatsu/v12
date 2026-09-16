//! Shared conversions and validation helpers for the built-in natives.
//!
//! Thin shims over [`super::ctx::Ctx`] conversions: the canonical logic
//! lives on `Ctx` (per `docs/builtins-arch-plan.md` §4.1) and these free
//! functions delegate through an ephemeral `Ctx` so existing
//! `fn(&mut Heap, …)` bodies keep compiling until their file migrates.
//! Leaf string-flatten utilities (`string_text_cow`, `value_text_cow`)
//! stay here because they borrow heap storage via `Cow`.

use std::borrow::Cow;

use v12_heap::{Handle, Heap, JsObject, JsValue, V12Str};
use v12_native::Throw;

use super::ctx::Ctx;

/// The receiver for a built-in method, checked against `this`.
///
/// Shim over [`Ctx::this_object`].
pub fn as_object(
    heap: &mut Heap,
    this: JsValue,
    method: &str,
    kind: Option<v12_heap::Kind>,
) -> Result<Handle<JsObject>, Throw> {
    Ctx::new(heap, None, None).this_object(this, method, kind)
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
/// Shim over [`Ctx::string_text`].
pub fn string_text(heap: &mut Heap, h: Handle<V12Str>) -> String {
    Ctx::new(heap, None, None).string_text(h)
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
/// Shim over [`Ctx::to_string`].
pub fn value_text(heap: &mut Heap, v: JsValue) -> String {
    Ctx::new(heap, None, None).to_string(v)
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
/// objects → NaN. Shim over [`Ctx::to_number`].
pub fn to_number(heap: &mut Heap, v: JsValue) -> f64 {
    Ctx::new(heap, None, None).to_number(v)
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
    if let (Some(x), Some(y)) = (
        a.as_smi().map(f64::from).or(a.as_f64()),
        b.as_smi().map(f64::from).or(b.as_f64()),
    ) {
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
