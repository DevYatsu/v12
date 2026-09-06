//! Boolean built-in.

use v12_heap::{Heap, JsValue};
use v12_native::Throw;

/// `Boolean(value)` – converts value to boolean following ToBoolean.
pub fn boolean_construct(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let truthy = to_boolean(heap, v);
    Ok(JsValue::from_bool(truthy))
}

fn to_boolean(heap: &Heap, v: JsValue) -> bool {
    let _ = heap;
    if v.is_true() {
        return true;
    }
    if v.is_false() || v.is_undefined() || v.is_null() {
        return false;
    }
    if let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) {
        return n != 0.0 && !n.is_nan();
    }
    if let Some(h) = v.as_string() {
        return !heap.get(h).is_empty();
    }
    // Objects are truthy.
    if v.is_object() {
        return true;
    }
    false
}

/// `Boolean.prototype.toString` – `"true"`/`"false"` for the primitive
/// receiver (wrapper objects are not modeled).
pub fn boolean_proto_to_string(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = if this.is_true() {
        "true"
    } else if this.is_false() {
        "false"
    } else {
        return Err(Throw::type_error(
            heap,
            "TypeError: Boolean.prototype.toString requires that 'this' be a Boolean",
        ));
    };
    Ok(JsValue::string(heap.intern_text(text)))
}

/// `Boolean.prototype.valueOf` – the primitive receiver itself. A
/// non-Boolean receiver throws (no unchecked `this` passthrough).
pub fn boolean_proto_value_of(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.is_true() || this.is_false() {
        Ok(this)
    } else {
        Err(Throw::type_error(
            heap,
            "TypeError: Boolean.prototype.valueOf requires that 'this' be a Boolean",
        ))
    }
}
