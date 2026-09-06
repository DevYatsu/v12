//! The [`Throw`] error type for native handlers.

use v12_heap::{Heap, JsObject, JsValue};

/// A value to throw.
///
/// Distinct from the success `JsValue` so a handler signature reads "produce
/// a value or throw one" instead of two naked `JsValue`s. An `Err(Throw)`
/// returned by a native is thrown inside JS.
///
/// A `Throw` is either a ready-to-throw [`JsValue`] (a real `Kind::Error`
/// object built with the heap) or a not-yet-interned message (produced by
/// the heap-free std [`TryFrom`] conversions). The dispatch boundary resolves
/// [`Throw::Message`] into a real error object via [`Throw::into_js`].
#[derive(Clone, PartialEq, Eq)]
pub enum Throw {
    /// A ready-to-throw value (a real `Kind::Error` object).
    Value(JsValue),
    /// A message not yet interned; the throw boundary turns it into a real
    /// error object via [`Throw::into_js`].
    Message(String),
}

/// Error kinds recognized in a `"Kind: message"` prefix. Kept in sync with
/// the realm intrinsics (`Error`, `TypeError`, `RangeError`,
/// `ReferenceError`, `SyntaxError` in `GLOBAL_INTRINSICS`) plus the
/// conventional `URIError`/`InternalError` spellings.
const KNOWN_KINDS: &[&str] = &[
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "URIError",
    "InternalError",
];

/// Splits a `"Kind: message"` prefix, honoring the embedded kind when it is
/// a known error kind and falling back to `default_kind` otherwise.
///
/// Lets long-standing `"TypeError: …"`-spelled call sites keep their text
/// while the thrown value becomes a real object: the stored `message` never
/// duplicates the `name` (display renders `"Name: message"`).
pub fn parse_error_text<'a>(text: &'a str, default_kind: &'a str) -> (&'a str, &'a str) {
    if let Some((kind, rest)) = text.split_once(": ")
        && KNOWN_KINDS.contains(&kind)
    {
        return (kind, rest);
    }
    (default_kind, text)
}

/// Builds a real `Kind::Error` object with `properties = [name, message]`
/// (the layout the display paths read directly). Heap-only: no realm is
/// available here, so no `constructor` own-prop is wired — realm-backed
/// callers (e.g. `Ctx`) wire it from the intrinsic slot instead.
pub fn error_object(heap: &mut Heap, kind: &str, message: &str) -> JsValue {
    let name_h = heap.intern_text(kind);
    let msg_h = heap.intern_text(message);
    let obj = heap.alloc(JsObject::error(name_h, msg_h));
    heap.add_root(JsValue::object(obj));
    JsValue::object(obj)
}

impl Throw {
    /// Builds a ready-to-throw real error object (`name` from the message's
    /// `"Kind: …"` prefix when known, else `"TypeError"`).
    pub fn type_error(heap: &mut Heap, msg: impl AsRef<str>) -> Self {
        let text = msg.as_ref();
        let (kind, message) = parse_error_text(text, "TypeError");
        Throw::Value(error_object(heap, kind, message))
    }

    /// Builds a ready-to-throw real `RangeError` object.
    pub fn range_error(heap: &mut Heap, msg: impl AsRef<str>) -> Self {
        let text = msg.as_ref();
        let (kind, message) = parse_error_text(text, "RangeError");
        Throw::Value(error_object(heap, kind, message))
    }

    /// Builds a ready-to-throw real `SyntaxError` object.
    pub fn syntax_error(heap: &mut Heap, msg: impl AsRef<str>) -> Self {
        let text = msg.as_ref();
        let (kind, message) = parse_error_text(text, "SyntaxError");
        Throw::Value(error_object(heap, kind, message))
    }

    /// Builds a ready-to-throw real `ReferenceError` object.
    pub fn reference_error(heap: &mut Heap, msg: impl AsRef<str>) -> Self {
        let text = msg.as_ref();
        let (kind, message) = parse_error_text(text, "ReferenceError");
        Throw::Value(error_object(heap, kind, message))
    }

    /// Builds a not-yet-interned `TypeError: <msg>`.
    ///
    /// For conversions with no heap in hand ([`TryFrom<JsValue>`]); the
    /// dispatch boundary resolves it into a real error object via
    /// [`Throw::into_js`].
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

    /// Resolves the throw into a concrete `JsValue`, building a real error
    /// object for any pending message against `heap`.
    pub fn into_js(self, heap: &mut Heap) -> JsValue {
        match self {
            Throw::Value(v) => v,
            Throw::Message(msg) => {
                let (kind, message) = parse_error_text(&msg, "TypeError");
                error_object(heap, kind, message)
            }
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
