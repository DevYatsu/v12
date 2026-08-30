//! Math built-ins.

use v12_heap::{Heap, JsValue};
use v12_native::Throw;

/// `Math.abs(x)` – absolute value.
pub fn math_abs(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let n = to_number(heap, v);
    if n.is_nan() {
        return Ok(JsValue::from_f64(f64::NAN));
    }
    let abs = n.abs();
    // Canonicalize through Smi when possible.
    if abs.fract() == 0.0
        && abs >= f64::from(JsValue::SMI_MIN)
        && abs <= f64::from(JsValue::SMI_MAX)
        && let Some(smi) = JsValue::from_i32_smi(abs as i32)
    {
        return Ok(smi);
    }
    Ok(JsValue::from_f64(abs))
}

fn to_number(heap: &mut Heap, v: JsValue) -> f64 {
    if let Some(n) = v.as_smi().map(f64::from) {
        return n;
    }
    if let Some(n) = v.as_f64() {
        return n;
    }
    if v.is_true() {
        return 1.0;
    }
    if v.is_false() {
        return 0.0;
    }
    if v.is_null() {
        return 0.0;
    }
    if v.is_string() {
        let h = v.as_string().expect("string");
        let units = {
            heap.flatten(h);
            match &heap.get(h).storage {
                v12_heap::StrStorage::Latin1(b) => {
                    b.iter().map(|&c| u16::from(c)).collect::<Vec<_>>()
                }
                v12_heap::StrStorage::Utf16(u) => u.clone(),
                _ => Vec::new(),
            }
        };
        let s: String = String::from_utf16_lossy(&units);
        return s.trim().parse::<f64>().unwrap_or(f64::NAN);
    }
    f64::NAN
}
