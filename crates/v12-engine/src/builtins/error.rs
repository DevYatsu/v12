//! Error built-ins.

use v12_heap::{Heap, JsObject, JsValue};
use v12_native::Throw;

use super::helpers;

/// `Error(message)` – creates an error object with a message.
pub fn error_create(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let message = args.first().copied().unwrap_or(JsValue::undefined());
    let name_h = helpers::intern_text(heap, "Error");
    let msg_h = if let Some(h) = message.as_string() {
        h
    } else if message.is_undefined() {
        helpers::intern_text(heap, "")
    } else {
        // Non-string message: render it (best-effort) as the message text.
        let text = helpers::value_text(heap, message);
        helpers::intern_text(heap, &text)
    };
    let obj = helpers::alloc_obj(heap, JsObject::error(name_h, msg_h));
    Ok(JsValue::object(obj))
}
