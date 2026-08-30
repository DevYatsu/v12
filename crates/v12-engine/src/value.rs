//! Value marshalling traits for the embedding interface.
//!
//! Host values convert to and from engine values through `ToValue` and
//! `FromValue`. Handles never escape the crate boundary; only `JsValue`
//! crosses it.

use v12_heap::{Heap, JsValue, V12Str};

/// Converts a host value into an engine value.
pub trait ToValue {
    /// Converts `self` into a JavaScript value allocated in `heap`.
    fn to_value(&self, heap: &mut Heap) -> JsValue;
}

/// Converts an engine value into a host value.
pub trait FromValue: Sized {
    /// Attempts to convert `value` read from `heap` into `Self`.
    fn from_value(heap: &Heap, value: JsValue) -> Option<Self>;
}

impl ToValue for bool {
    fn to_value(&self, _heap: &mut Heap) -> JsValue {
        JsValue::from(*self)
    }
}

impl FromValue for bool {
    fn from_value(_heap: &Heap, value: JsValue) -> Option<Self> {
        value.try_into().ok()
    }
}

impl ToValue for i32 {
    fn to_value(&self, _heap: &mut Heap) -> JsValue {
        JsValue::from(*self)
    }
}

impl FromValue for i32 {
    fn from_value(_heap: &Heap, value: JsValue) -> Option<Self> {
        value.try_into().ok()
    }
}

impl ToValue for f64 {
    fn to_value(&self, _heap: &mut Heap) -> JsValue {
        JsValue::from(*self)
    }
}

impl FromValue for f64 {
    fn from_value(_heap: &Heap, value: JsValue) -> Option<Self> {
        value.try_into().ok()
    }
}

impl ToValue for String {
    fn to_value(&self, heap: &mut Heap) -> JsValue {
        let handle = if self.is_ascii() {
            heap.intern_string(V12Str::latin1(self.as_bytes().to_vec()))
        } else {
            heap.intern_string(V12Str::utf16(self.encode_utf16().collect()))
        };
        JsValue::string(handle)
    }
}

impl ToValue for &str {
    fn to_value(&self, heap: &mut Heap) -> JsValue {
        let handle = if self.is_ascii() {
            heap.intern_string(V12Str::latin1(self.as_bytes().to_vec()))
        } else {
            heap.intern_string(V12Str::utf16(self.encode_utf16().collect()))
        };
        JsValue::string(handle)
    }
}

impl FromValue for String {
    fn from_value(heap: &Heap, value: JsValue) -> Option<Self> {
        let handle = value.as_string()?;
        let s = heap.get(handle);
        match &s.storage {
            v12_heap::StrStorage::Latin1(bytes) => {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
            v12_heap::StrStorage::Utf16(units) => Some(String::from_utf16_lossy(units)),
            _ => {
                // Composite strings are flattened lazily; report None for the
                // minimal marshalling layer and let callers flatten first.
                None
            }
        }
    }
}

impl ToValue for () {
    fn to_value(&self, _heap: &mut Heap) -> JsValue {
        JsValue::undefined()
    }
}

impl FromValue for () {
    fn from_value(_heap: &Heap, _value: JsValue) -> Option<Self> {
        // `()` means "discard the result": with ADR-004 completion values
        // wired, scripts now complete with real values, so every value
        // decodes to unit.
        Some(())
    }
}
