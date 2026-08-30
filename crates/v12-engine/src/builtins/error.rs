//! Error built-ins.

use v12_heap::{Heap, JsObject, JsValue, V12Str};

/// `Error(message)` – creates an error object with a message.
pub fn error_create(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let message = args.first().copied().unwrap_or(JsValue::undefined());
    let name_h = heap.intern_string(V12Str::latin1(b"Error".to_vec()));
    let msg_h = if let Some(h) = message.as_string() {
        h
    } else if message.is_undefined() {
        heap.intern_string(V12Str::latin1(b"".to_vec()))
    } else {
        // Non-string message: render it (best-effort) as the message text.
        let text = super::value_display_text(heap, message);
        if text.is_ascii() {
            heap.intern_string(V12Str::latin1(text.into_bytes()))
        } else {
            heap.intern_string(V12Str::utf16(text.encode_utf16().collect()))
        }
    };
    let obj = heap.alloc(JsObject::error(name_h, msg_h));
    heap.add_root(JsValue::object(obj));
    Ok(JsValue::object(obj))
}
