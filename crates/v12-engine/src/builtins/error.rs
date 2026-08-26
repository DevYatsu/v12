//! Error built-ins.

use v12_heap::{Heap, JsObject, JsValue, V12Str};

/// `Error(message)` – creates an error object with a message property.
pub fn error_create(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let message = args.first().copied().unwrap_or(JsValue::undefined());
    let obj = heap.alloc(JsObject::default());
    // Store message as property "message" if provided.
    if !message.is_undefined() {
        let key = {
            let h = heap.intern_string(V12Str::latin1(b"message".to_vec()));
            v12_heap::PropKey::from_string(h)
        };
        let shape = heap.root_shape();
        let child = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
        let _ = child;
        heap.get_mut(obj).properties.push(message);
    }
    Ok(JsValue::object(obj))
}
