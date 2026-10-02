//! Heap-only conversions and small allocation helpers for the built-in
//! natives.
//!
//! # Invariant: everything here is heap-only
//!
//! Every function in this module touches `Heap` and nothing else. That is a
//! deliberate property, not an accident of how the module grew: the operations
//! implemented here (`ToString`, string flattening, property-key text,
//! `IsStrictlyEqual`, `Array.prototype.sort`'s display key) are all
//! defined over **primitives and plain heap state**. None of them can observe
//! a realm intrinsic, the pending-job sink, or the regexp cache, so none of
//! them needs them.
//!
//! Anything that *does* need the realm lives on [`super::ctx::Ctx`], which
//! carries `global`, `pending`, and `regex_cache`. Plan §3 F4 / §4 W3.1: this
//! module used to hold shims that built a throwaway `Ctx::new(heap, None,
//! None)` and delegated to it, which silently detached every error they threw
//! from the realm (`thrown.constructor` read `undefined`). Do not reintroduce
//! one. A caller that already holds a `&mut Ctx` calls `ctx.to_number(v)` /
//! `ctx.to_string(v)` / `ctx.string_text(h)` directly; the heap-only twins
//! below exist for the few call sites that genuinely have no `Ctx` (a
//! `&mut Heap`-only helper such as `array::dense_bound`, and
//! `registry::function_construct`, which has a realm `global` but no `Ctx`).
//!
//! The `heap_` prefix on the twins says "this one is heap-only, on purpose";
//! [`super::ctx::Ctx`] delegates to them so there is one implementation of
//! each coercion.

use std::borrow::Cow;

use v12_heap::{Handle, Heap, JsObject, JsValue, V12Str};

/// The string text of a heap string, borrowing when possible.
///
/// Flattens first, then borrows the Latin-1 bytes as `&str` when they form
/// valid UTF-8 (ASCII always does) — zero allocation; invalid Latin-1 or
/// UTF-16 storage falls back to an owned lossy conversion. Prefer this over
/// [`heap_string_text`] when the text is consumed before the next heap access —
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
///
/// Heap-only by construction (see the module invariant): flattening and
/// storage decoding read no realm state. [`super::ctx::Ctx::string_text`]
/// delegates here, so a `&mut Ctx` caller and a bare-`&mut Heap` caller render
/// a string identically.
pub(crate) fn heap_string_text(heap: &mut Heap, h: Handle<V12Str>) -> String {
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
        _ => String::new(),
    }
}

/// The text of a value, borrowing the string storage when possible (see
/// [`string_text_cow`] for the borrow/allocate contract).
pub fn value_text_cow<'a>(heap: &'a mut Heap, v: JsValue) -> Cow<'a, str> {
    if let Some(h) = v.as_string() {
        return string_text_cow(heap, h);
    }
    Cow::Owned(display_text(v))
}

/// The text of a value: strings render their text, real arrays render
/// comma-joined elements, everything else renders the way `console.log`
/// observes it (Tier-0 display subset) — except a Number, which uses ES
/// `ToString` (so `-0` renders `"0"`, unlike the `display_text` form).
/// Property keys and `String(-0)` both flow through here.
///
/// Heap-only by construction (see the module invariant): real arrays are
/// joined straight out of the element store and no prototype walk or user code
/// can run, so there is nothing here for a realm to answer.
/// [`super::ctx::Ctx::to_string`] delegates here.
pub(crate) fn heap_value_text(heap: &mut Heap, v: JsValue) -> String {
    if let Some(obj) = v.as_object()
        && heap.get(obj).kind == v12_heap::Kind::Array
    {
        return array_join_text(heap, obj, 0);
    }
    if let Some(h) = v.as_string() {
        return heap_string_text(heap, h);
    }
    if let Some(number) = v.as_smi().map(f64::from).or(v.as_f64()) {
        return super::number::number_to_string(number);
    }
    display_text(v)
}

/// Comma-joined element text of a real array (`undefined`/`null`/holes render
/// empty, matching `Array.prototype.join`). Nested arrays recurse; `depth` caps
/// the recursion so cyclic arrays terminate.
fn array_join_text(heap: &mut Heap, obj: Handle<JsObject>, depth: usize) -> String {
    if depth > 8 {
        return String::new();
    }
    // Snapshot before formatting: rendering an element may allocate (and thus
    // collect), invalidating a live borrow of the element store.
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
            parts.push(heap_string_text(heap, h));
        } else {
            parts.push(display_text(v));
        }
    }
    parts.join(",")
}

/// The single UTF-16 code unit at `index` of a heap string, as an interned
/// string value (the observable form of a `String` index own property).
/// Returns the empty string when `index` is past the code-unit length.
pub fn string_index_unit(heap: &mut Heap, h: Handle<V12Str>, index: usize) -> Handle<V12Str> {
    heap.flatten(h);
    let unit = match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => bytes.get(index).map(|&b| u16::from(b)),
        v12_heap::StrStorage::Utf16(units) => units.get(index).copied(),
        // flatten just ran; composites are impossible now.
        v12_heap::StrStorage::Cons { .. } | v12_heap::StrStorage::Sliced { .. } => None,
    };
    match unit {
        Some(u) => heap.intern_string(V12Str::utf16(vec![u])),
        None => heap.intern_string(V12Str::latin1(Vec::new())),
    }
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
