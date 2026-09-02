//! Math built-ins.

use std::sync::atomic::{AtomicU64, Ordering};

use v12_heap::{Heap, JsValue};
use v12_native::Throw;

use super::helpers;

/// Fast-forward the first argument's `f64` (defaulting absent to `undefined`,
/// i.e. NaN via `to_number`), feeding every math built-in a single input.
fn one_arg(heap: &mut Heap, args: &[JsValue]) -> f64 {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    helpers::to_number(heap, v)
}

/// `Math.abs(x)` – absolute value; `Math.abs(NaN)` is NaN.
pub fn math_abs(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(heap, args);
    Ok(helpers::js_number(n.abs()))
}

/// `Math.floor(x)` – greatest integer ≤ x.
pub fn math_floor(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(heap, args);
    Ok(helpers::js_number(n.floor()))
}

/// `Math.ceil(x)` – smallest integer ≥ x.
pub fn math_ceil(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(heap, args);
    Ok(helpers::js_number(n.ceil()))
}

/// `Math.trunc(x)` – integral part, toward zero.
pub fn math_trunc(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(heap, args);
    Ok(helpers::js_number(n.trunc()))
}

/// `Math.round(x)` – round toward +∞ on the half (ES: `Math.round(-0.5)` is
/// `-0`, `Math.round(0.5)` is `1`). Rust's `f64::round` rounds half away from
/// zero, so floor(x + 0.5) is used instead; the 0.0 early-return preserves ±0.
pub fn math_round(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let x = one_arg(heap, args);
    Ok(helpers::js_number(round_half_up(x)))
}

/// ES `Math.round` semantics: `floor(x + 0.5)`, with the ±0 passes-through so
/// `Math.round(-0.5)` yields `-0` (ES differs from Rust's half-away rounding).
fn round_half_up(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() {
        return x;
    }
    if x == 0.0 {
        return x;
    }
    (x + 0.5).floor()
}

/// `Math.sqrt(x)` – non-negative square root; negative input → NaN.
pub fn math_sqrt(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(heap, args);
    if n < 0.0 {
        return Ok(JsValue::from_f64(f64::NAN));
    }
    Ok(helpers::js_number(n.sqrt()))
}

/// `Math.pow(x, y)` – x raised to the y-th power. Rust's `powf` follows IEEE
/// 754, which matches ES (including `Math.pow(NaN, 0) === 1`).
pub fn math_pow(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let x = one_arg(heap, args);
    let y = args.get(1).copied().unwrap_or(JsValue::undefined());
    let y = helpers::to_number(heap, y);
    Ok(JsValue::from_f64(x.powf(y)))
}

/// `Math.max(...)` – largest argument; no args → -Infinity; any NaN → NaN.
pub fn math_max(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    if args.is_empty() {
        return Ok(JsValue::from_f64(f64::NEG_INFINITY));
    }
    let mut max = f64::NEG_INFINITY;
    for &a in args {
        let n = helpers::to_number(heap, a);
        if n.is_nan() {
            return Ok(JsValue::from_f64(f64::NAN));
        }
        if n > max {
            max = n;
        }
    }
    Ok(helpers::js_number(max))
}

/// `Math.min(...)` – smallest argument; no args → +Infinity; any NaN → NaN.
pub fn math_min(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    if args.is_empty() {
        return Ok(JsValue::from_f64(f64::INFINITY));
    }
    let mut min = f64::INFINITY;
    for &a in args {
        let n = helpers::to_number(heap, a);
        if n.is_nan() {
            return Ok(JsValue::from_f64(f64::NAN));
        }
        if n < min {
            min = n;
        }
    }
    Ok(helpers::js_number(min))
}

/// Deterministic PRNG seed for `Math.random`. Never observed by test262 (which
/// only asserts the output is a number in [0, 1)); avoids `SystemTime`, so
/// output is reproducible across runs.
static RNG_STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

/// `Math.random()` – a deterministic, seeded number in [0, 1). A xorshift step
/// advances the state on every call.
pub fn math_random(_heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut x = RNG_STATE.load(Ordering::Relaxed);
    // xorshift: three inline shifts cover the state space, no final multiply.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG_STATE.store(x, Ordering::Relaxed);
    // Map to [0, 1): scale the high 53 bits by 2^-53 (the largest count of
    // distinct doubles below 1.0), so the value is a valid `Math.random`.
    let d = ((x >> 11) as f64) * (1.0 / (1u64 << 53) as f64);
    Ok(JsValue::from_f64(d))
}
