//! Number built-ins.

use std::borrow::Cow;

use v12_heap::{Heap, JsValue};
use v12_native::Throw;

use super::helpers;

/// `Number.isNaN(value)` – true only for NaN. No coercion: `Number.isNaN("x")`
/// is `false` (only an actual number value that is NaN answers true).
pub fn number_is_nan(_heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is_nan = if let Some(n) = v.as_f64() {
        n.is_nan()
    } else {
        // A Smi or any non-number is never NaN.
        false
    };
    Ok(if is_nan {
        JsValue::true_()
    } else {
        JsValue::false_()
    })
}

/// `Number.isFinite(value)` – no coercion: only true when the value is a
/// number that is finite (a Smi, or a finite double). `Number.isFinite("1")`
/// is `false`.
pub fn number_is_finite(
    _heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let finite = if v.as_smi().is_some() {
        true
    } else if let Some(n) = v.as_f64() {
        n.is_finite()
    } else {
        false
    };
    Ok(if finite {
        JsValue::true_()
    } else {
        JsValue::false_()
    })
}

/// Global `isNaN(value)` – COERCES via `ToNumber`, then tests for NaN. So
/// `isNaN("x")` is `true` (the string coerces to NaN).
pub fn global_is_nan(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let n = helpers::to_number(heap, v);
    Ok(if n.is_nan() {
        JsValue::true_()
    } else {
        JsValue::false_()
    })
}

/// Global `isFinite(value)` – COERCES via `ToNumber`, then requires a finite
/// number (NaN and ±Infinity yield `false`).
pub fn global_is_finite(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let n = helpers::to_number(heap, v);
    Ok(if n.is_finite() {
        JsValue::true_()
    } else {
        JsValue::false_()
    })
}

/// Accumulates the maximal integer prefix of the sign-less digit string `digits`
/// valid in `radix`, applying `sign`. Returns NaN when no valid digit is scanned.
fn scan_int_digits(digits: &str, radix: u32, sign: f64) -> f64 {
    let mut value = 0.0f64;
    let mut any = false;
    for c in digits.chars() {
        let Some(d) = c.to_digit(radix) else {
            break;
        };
        any = true;
        value = value * f64::from(radix) + f64::from(d);
    }
    if !any {
        return f64::NAN;
    }
    sign * value
}

/// Global `parseInt(string, radix?)` (also `Number.parseInt`).
pub fn global_parse_int(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    // Resolve the radix before extracting the text: the borrowed text holds
    // the heap until it is dropped, so the radix coercion must run first
    // (unobservable here — `to_number` never runs user code).
    let radix_arg = args.get(1).copied();
    let mut radix = match radix_arg {
        Some(v) => helpers::to_number(heap, v) as i32,
        None => 0,
    };

    // ToString the first argument; default empty string stays NaN.
    let text = match args.first() {
        Some(v) => helpers::value_text_cow(heap, *v),
        None => Cow::Borrowed(""),
    };
    let mut s = text.trim();

    // Strip an optional leading sign; the `0x` prefix is only honored on the
    // sign-less remainder (spec: parseInt("-0x10") === -16).
    let mut sign = 1.0f64;
    if let Some(first) = s.as_bytes().first() {
        if *first == b'-' {
            sign = -1.0;
            s = &s[1..];
        } else if *first == b'+' {
            s = &s[1..];
        }
    }

    // NaN radix → 0; 0/absent → default 10, honoring a `0x`/`0X` hex prefix
    // (→ 16).
    if radix == 0 {
        if s.len() >= 2 && s.as_bytes().starts_with(b"0x") {
            return Ok(helpers::js_number(scan_int_digits(&s[2..], 16, sign)));
        }
        if s.len() >= 2 && s.as_bytes().starts_with(b"0X") {
            return Ok(helpers::js_number(scan_int_digits(&s[2..], 16, sign)));
        }
        radix = 10;
    } else if !(2..=36).contains(&radix) {
        return Ok(JsValue::from_f64(f64::NAN));
    }
    Ok(helpers::js_number(scan_int_digits(s, radix as u32, sign)))
}

/// Global `parseFloat(string)` (also `Number.parseFloat`). Returns a double
/// per spec (never a Smi).
pub fn global_parse_float(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(v) => helpers::value_text_cow(heap, *v),
        None => Cow::Borrowed(""),
    };
    let trimmed = text.trim_start();

    // Scan a StrDecimalLiteral prefix: sign?, digits?, '.', digits?, e/E
    // exponent?. No digit before the exponent → NaN.
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut has_frac = false;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        has_frac = i > frac_start;
    }
    let has_int = i > digits_start;
    if !has_int && !has_frac {
        return Ok(JsValue::from_f64(f64::NAN));
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let exp_start = i;
        i += 1;
        if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
            i += 1;
        }
        let exp_digits = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_digits {
            // Exponent with no digits: rewind past the 'e' so only the mantissa
            // is consumed (a bare `1e` parses `1`).
            i = exp_start;
        }
    }
    // The consumed prefix includes the sign char, so `.parse` yields the exact
    // value (including a -0 sign) without further adjustment.
    let consumed = &trimmed[..i];
    Ok(JsValue::from_f64(consumed.parse().unwrap_or(f64::NAN)))
}

/// `Number(value)` – the callable/constructible `Number` intrinsic.
/// `Number()` → 0; `Number(undefined)` → NaN; `Number(null)` → 0;
/// `Number(true)` → 1; strings are parsed; objects → NaN (subset).
pub fn number_construct(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(&v) = args.first() else {
        return Ok(helpers::js_number(0.0));
    };
    let n = helpers::to_number(heap, v);
    Ok(helpers::js_number(n))
}
