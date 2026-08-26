//! Boolean built-in.

use v12_heap::{Heap, JsValue};

/// `Boolean(value)` – converts value to boolean following ToBoolean.
pub fn boolean_construct(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsValue> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let truthy = to_boolean(heap, v);
    Ok(if truthy {
        JsValue::true_()
    } else {
        JsValue::false_()
    })
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
