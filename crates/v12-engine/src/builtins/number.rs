//! Number built-ins.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.

use v12_heap::JsValue;
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

/// `Number.isNaN(value)` – true only for NaN. No coercion: `Number.isNaN("x")`
/// is `false` (only an actual number value that is NaN answers true).
pub fn number_is_nan(_ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is_nan = if let Some(n) = v.as_f64() {
        n.is_nan()
    } else {
        // A Smi or any non-number is never NaN.
        false
    };
    Ok(JsValue::from_bool(is_nan))
}

/// `Number.isFinite(value)` – no coercion: only true when the value is a
/// number that is finite (a Smi, or a finite double). `Number.isFinite("1")`
/// is `false`.
pub fn number_is_finite(
    _ctx: &mut Ctx,
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
    Ok(JsValue::from_bool(finite))
}

/// Global `isNaN(value)` – COERCES via `ToNumber`, then tests for NaN. So
/// `isNaN("x")` is `true` (the string coerces to NaN).
pub fn global_is_nan(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let n = ctx.to_number(v);
    Ok(JsValue::from_bool(n.is_nan()))
}

/// Global `isFinite(value)` – COERCES via `ToNumber`, then requires a finite
/// number (NaN and ±Infinity yield `false`).
pub fn global_is_finite(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let n = ctx.to_number(v);
    Ok(JsValue::from_bool(n.is_finite()))
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
pub fn global_parse_int(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    // Owned text via `ctx.to_string`: no heap borrow is held, so the radix
    // coercion order is unobservable (`to_number` never runs user code).
    let radix_arg = args.get(1).copied();
    let mut radix = match radix_arg {
        Some(v) => ctx.to_number(v) as i32,
        None => 0,
    };

    // ToString the first argument; default empty string stays NaN.
    let text = match args.first() {
        Some(v) => ctx.to_string(*v),
        None => String::new(),
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
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(v) => ctx.to_string(*v),
        None => String::new(),
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
pub fn number_construct(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let Some(&v) = args.first() else {
        return Ok(helpers::js_number(0.0));
    };
    let n = ctx.to_number(v);
    Ok(helpers::js_number(n))
}

/// ECMAScript `Number::toString`: shortest round-trip digits re-rendered per
/// the spec's decimal/exponential branch rules (`1e21` prints `1e+21`,
/// `1e-7` prints `1e-7`, everything between prints decimal).
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if n == 0.0 {
        return "0".to_string(); // covers -0: ToString(-0) is "0"
    }
    // `{:e}` yields the shortest round-trip form: `d.ddd…e<exp>`.
    let sci = format!("{:e}", n);
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp always emits 'e'");
    let exp: i32 = exp.parse().expect("LowerExp exponent is an integer");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa
        .trim_start_matches('-')
        .chars()
        .filter(|c| *c != '.')
        .collect();
    // Trim trailing zeros that `{:e}` may keep for integral doubles (it does
    // not, but the filter above could reassemble `10` → `10`); canonical
    // shortest form from `{:e}` has no trailing zeros past the first digit.
    let k = digits.len() as i32;
    let n10 = exp + 1; // value = 0.digits × 10^n10

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if k <= n10 && n10 <= 21 {
        // Digits followed by (n10 - k) zeros.
        out.push_str(&digits);
        for _ in 0..(n10 - k) {
            out.push('0');
        }
    } else if 0 < n10 && n10 <= 21 {
        // Dot after n10 digits.
        out.push_str(&digits[..n10 as usize]);
        out.push('.');
        out.push_str(&digits[n10 as usize..]);
    } else if -6 < n10 && n10 <= 0 {
        out.push_str("0.");
        for _ in 0..(-n10) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n10 - 1 >= 0 { '+' } else { '-' });
        out.push_str(&(n10 - 1).abs().to_string());
    }
    out
}

/// The `this` receiver as a primitive number (`ToNumber` subset: primitives
/// pass through, wrapper objects are not modeled).
fn this_number(ctx: &mut Ctx, this: JsValue, method: &str) -> Result<f64, Throw> {
    if let Some(n) = this.as_smi().map(f64::from) {
        return Ok(n);
    }
    if let Some(n) = this.as_f64() {
        return Ok(n);
    }
    Err(ctx.type_error(format!(
        "TypeError: Number.prototype.{method} requires that 'this' be a Number"
    )))
}

/// `Number.prototype.toString(radix?)` – shortest form in radix 2-36;
/// fractional/ irrational values approximate with 14 significant fraction
/// digits (spec-exact for integral values, which is what tests exercise).
pub fn number_proto_to_string(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let n = this_number(ctx, this, "toString")?;
    let radix = match args.first().copied() {
        None => 10,
        Some(v) if v.is_undefined() => 10,
        Some(v) => {
            let r = ctx.to_number(v);
            if !(2.0..=36.0).contains(&r) || r.fract() != 0.0 {
                return Err(
                    ctx.range_error("RangeError: toString() radix must be between 2 and 36")
                );
            }
            r as u32
        }
    };
    if radix == 10 || n.is_nan() || n.is_infinite() {
        let text = number_to_string(n);
        return Ok(JsValue::string(ctx.heap.intern_text(&text)));
    }
    // NaN/Infinity spell the same in every radix.
    if n.is_nan() {
        return Ok(JsValue::string(ctx.heap.intern_text("NaN")));
    }
    if n.is_infinite() {
        return Ok(JsValue::string(ctx.heap.intern_text(if n > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        })));
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let negative = n < 0.0;
    let mag = n.abs();
    let mut int_part = mag.trunc();
    let frac_part = mag.fract();
    let mut out = String::new();
    if int_part == 0.0 {
        out.push('0');
    } else {
        let mut chunks = Vec::new();
        while int_part >= 1.0 {
            chunks.push(DIGITS[(int_part % radix as f64) as usize]);
            int_part = (int_part / radix as f64).trunc();
        }
        out.extend(chunks.iter().rev().copied().map(|b| b as char));
    }
    if frac_part > 0.0 {
        out.push('.');
        let mut f = frac_part;
        for _ in 0..14 {
            f *= f64::from(radix);
            let d = f.trunc();
            out.push(DIGITS[d as usize] as char);
            f -= d;
            if f <= 0.0 {
                break;
            }
        }
    }
    if negative {
        out.insert(0, '-');
    }
    Ok(JsValue::string(ctx.heap.intern_text(&out)))
}

/// `Number.prototype.toFixed(digits?)` – fixed-point with 0-100 fraction
/// digits; |x| ≥ 1e21 falls back to normal `ToString`.
pub fn number_to_fixed(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = this_number(ctx, this, "toFixed")?;
    let digits = match args.first() {
        None => 0usize,
        Some(&v) => {
            let d = ctx.to_number(v);
            if !(0.0..=100.0).contains(&d) {
                return Err(ctx.range_error(
                    "RangeError: toFixed() digits argument must be between 0 and 100",
                ));
            }
            d as usize
        }
    };
    let text = if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if n.abs() >= 1e21 {
        number_to_string(n)
    } else {
        format!("{:.*}", digits, n)
    };
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `Number.prototype.toPrecision(precision?)` – `undefined` → `ToString`;
/// else fixed or exponential with `precision` significant digits.
pub fn number_to_precision(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let n = this_number(ctx, this, "toPrecision")?;
    let Some(&p_v) = args.first() else {
        let text = number_to_string(n);
        return Ok(JsValue::string(ctx.heap.intern_text(&text)));
    };
    if p_v.is_undefined() {
        let text = number_to_string(n);
        return Ok(JsValue::string(ctx.heap.intern_text(&text)));
    }
    let p = ctx.to_number(p_v);
    if !(1.0..=100.0).contains(&p) {
        return Err(ctx.range_error("RangeError: toPrecision() argument must be between 1 and 100"));
    }
    let p = p as usize;
    let text = if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        let sci = format!("{:.*e}", p.saturating_sub(1), n);
        let (mantissa, exp) = sci.split_once('e').expect("LowerExp always emits 'e'");
        let exp: i32 = exp.parse().expect("LowerExp exponent is an integer");
        if exp + 1 > p as i32 || exp < -6 {
            // Exponential: mantissa already carries p-1 fraction digits;
            // normalize the exponent spelling to JS (`e+5`, `e-7`).
            let sign = if exp >= 0 { '+' } else { '-' };
            format!("{mantissa}e{sign}{}", exp.abs())
        } else if exp + 1 == p as i32 {
            mantissa.to_string()
        } else {
            // Fixed with (p - 1 - exp) fraction digits.
            let frac = (p as i32 - 1 - exp).max(0) as usize;
            format!("{:.*}", frac, n)
        }
    };
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `Number.prototype.toExponential(fractionDigits?)` – exponential notation;
/// `undefined` digit count uses the shortest round-trip form.
pub fn number_to_exponential(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let n = this_number(ctx, this, "toExponential")?;
    if n.is_nan() {
        return Ok(JsValue::string(ctx.heap.intern_text("NaN")));
    }
    if n.is_infinite() {
        let text = if n > 0.0 { "Infinity" } else { "-Infinity" };
        return Ok(JsValue::string(ctx.heap.intern_text(text)));
    }
    let text = match args.first() {
        Some(&v) if !v.is_undefined() => {
            let f = ctx.to_number(v);
            if !(0.0..=100.0).contains(&f) {
                return Err(ctx.range_error(
                    "RangeError: toExponential() argument must be between 0 and 100",
                ));
            }
            format_scientific(n, Some(f as usize))
        }
        _ => format_scientific(n, None),
    };
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// Renders `n` as JS scientific notation (`1.5e+21`, `1e-7`), with
/// `fraction_digits` fraction digits or the shortest round-trip form.
fn format_scientific(n: f64, fraction_digits: Option<usize>) -> String {
    let sci = match fraction_digits {
        Some(d) => format!("{:.*e}", d, n),
        None => format!("{:e}", n),
    };
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp always emits 'e'");
    let exp: i32 = exp.parse().expect("LowerExp exponent is an integer");
    let sign = if exp >= 0 { '+' } else { '-' };
    format!("{mantissa}e{sign}{}", exp.abs())
}

/// `Number.prototype.valueOf` – the primitive receiver itself (wrapper
/// objects are not modeled). A non-Number receiver throws (no unchecked
/// `this` passthrough).
pub fn number_proto_value_of(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    this_number(ctx, this, "valueOf")?;
    Ok(this)
}

/// `Number.isInteger(value)` – no coercion; integral numbers only.
pub fn number_is_integer(
    _ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is = if let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) {
        n.is_finite() && n.fract() == 0.0
    } else {
        false
    };
    Ok(JsValue::from_bool(is))
}

/// `Number.isSafeInteger(value)` – integral, finite, and within ±(2^53−1).
pub fn number_is_safe_integer(
    _ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is = if let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) {
        n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0
    } else {
        false
    };
    Ok(JsValue::from_bool(is))
}

macro_rules! number_const {
    ( $( $name:ident => $method:ident => $value:expr ),* $(,)? ) => {
        $(
            #[doc = concat!("`Number.", stringify!($method), "` – the constant, installed as a data property via `install_value`.")]
            pub fn $name(_ctx: &mut Ctx, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
                Ok($value)
            }
        )*
    };
}

number_const! {
    number_const_max_safe_integer => MAX_SAFE_INTEGER => helpers::js_number(9_007_199_254_740_991.0),
    number_const_min_safe_integer => MIN_SAFE_INTEGER => helpers::js_number(-9_007_199_254_740_991.0),
    number_const_epsilon => EPSILON => JsValue::from_f64(f64::EPSILON),
    number_const_max_value => MAX_VALUE => JsValue::from_f64(f64::MAX),
    number_const_min_value => MIN_VALUE => JsValue::from_f64(5e-324),
    number_const_positive_infinity => POSITIVE_INFINITY => JsValue::from_f64(f64::INFINITY),
    number_const_negative_infinity => NEGATIVE_INFINITY => JsValue::from_f64(f64::NEG_INFINITY),
    number_const_nan => NaN => JsValue::from_f64(f64::NAN),
}
