//! The [`Throw`] error type for native handlers.

use v12_heap::{Heap, JsValue, V12Str};

/// A value to throw.
///
/// Distinct from the success `JsValue` so a handler signature reads "produce
/// a value or throw one" instead of two naked `JsValue`s. An `Err(Throw)`
/// returned by a native is thrown inside JS.
///
/// A `Throw` is either a ready-to-throw [`JsValue`] (built by a handler that
/// holds the heap) or a not-yet-interned message (produced by the heap-free
/// std [`TryFrom`] conversions). The dispatch boundary resolves
/// [`Throw::Message`] into a real string value via [`Throw::into_js`].
#[derive(Clone, PartialEq, Eq)]
pub enum Throw {
    /// A ready-to-throw value (e.g. a `TypeError` string built with the heap).
    Value(JsValue),
    /// A message not yet interned; the throw boundary turns it into a string.
    Message(String),
}

impl Throw {
    /// Builds a ready-to-throw `TypeError: <msg>` value.
    pub fn type_error(heap: &mut Heap, msg: impl AsRef<str>) -> Self {
        Throw::Value(intern_error_string(heap, "TypeError", msg.as_ref()))
    }

    /// Builds a not-yet-interned `TypeError: <msg>`.
    ///
    /// For conversions with no heap in hand ([`TryFrom<JsValue>`]); the
    /// dispatch boundary resolves it via [`Throw::into_js`].
    pub fn type_error_msg(msg: impl Into<String>) -> Self {
        Throw::Message(format!("TypeError: {}", msg.into()))
    }

    /// The `typeof`-style name of a value's tag (heap-free).
    pub fn typeof_name(v: JsValue) -> &'static str {
        match () {
            _ if v.is_f64() || v.is_smi() => "number",
            _ if v.is_string() => "string",
            _ if v.is_boolean() => "boolean",
            _ if v.is_object() => "object",
            _ if v.is_undefined() => "undefined",
            _ if v.is_null() => "null",
            _ if v.is_symbol() => "symbol",
            _ if v.is_bigint() => "bigint",
            _ => "value",
        }
    }

    /// Resolves the throw into a concrete `JsValue`, interning any pending
    /// message against `heap`.
    pub fn into_js(self, heap: &mut Heap) -> JsValue {
        match self {
            Throw::Value(v) => v,
            Throw::Message(msg) => intern_error_string(heap, "TypeError", &msg),
        }
    }
}

impl From<JsValue> for Throw {
    #[inline]
    fn from(v: JsValue) -> Self {
        Throw::Value(v)
    }
}

impl From<String> for Throw {
    #[inline]
    fn from(msg: String) -> Self {
        Throw::Message(msg)
    }
}

impl From<&str> for Throw {
    #[inline]
    fn from(msg: &str) -> Self {
        Throw::Message(msg.to_owned())
    }
}

impl std::fmt::Debug for Throw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Throw::Value(v) => f.debug_tuple("Throw::Value").field(&v.bits()).finish(),
            Throw::Message(m) => f.debug_tuple("Throw::Message").field(m).finish(),
        }
    }
}

/// Interns `kind: msg` as a heap string value.
fn intern_error_string(heap: &mut Heap, kind: &str, msg: &str) -> JsValue {
    let full = format!("{kind}: {msg}");
    let h = if full.is_ascii() {
        heap.intern_string(V12Str::latin1(full.as_bytes().to_vec()))
    } else {
        heap.intern_string(V12Str::utf16(full.encode_utf16().collect()))
    };
    JsValue::string(h)
}
