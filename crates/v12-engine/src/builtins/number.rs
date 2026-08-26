//! Number built-ins.

use v12_heap::{Heap, JsValue};

/// `Number.isNaN(value)` – true only for NaN.
pub fn number_is_nan(
    _heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsValue> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is_nan = if let Some(n) = v.as_f64() {
        n.is_nan()
    } else {
        false
    };
    Ok(if is_nan {
        JsValue::true_()
    } else {
        JsValue::false_()
    })
}
