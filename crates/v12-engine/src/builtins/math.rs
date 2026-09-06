//! Math built-ins.
//!
//! Phase 3 step 2 migration (`docs/builtins-arch-plan.md` §5.3): pure
//! builtins that ignore `this`. Bodies take `&mut Ctx`; the legacy
//! `&mut Heap` dispatch site reaches them through `ctx::call_ctx`, so
//! dispatch IDs and install paths are unchanged.

use std::sync::atomic::{AtomicU64, Ordering};

use v12_heap::JsValue;
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

/// Fast-forward the first argument's `f64` (defaulting absent to `undefined`,
/// i.e. NaN via `to_number`), feeding every math built-in a single input.
fn one_arg(ctx: &mut Ctx, args: &[JsValue]) -> f64 {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    ctx.to_number(v)
}

/// `Math.abs(x)` – absolute value; `Math.abs(NaN)` is NaN.
pub fn math_abs(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(ctx, args);
    Ok(helpers::js_number(n.abs()))
}

/// `Math.floor(x)` – greatest integer ≤ x.
pub fn math_floor(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(ctx, args);
    Ok(helpers::js_number(n.floor()))
}

/// `Math.ceil(x)` – smallest integer ≥ x.
pub fn math_ceil(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(ctx, args);
    Ok(helpers::js_number(n.ceil()))
}

/// `Math.trunc(x)` – integral part, toward zero.
pub fn math_trunc(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(ctx, args);
    Ok(helpers::js_number(n.trunc()))
}

/// `Math.round(x)` – round toward +∞ on the half (ES: `Math.round(-0.5)` is
/// `-0`, `Math.round(0.5)` is `1`). Rust's `f64::round` rounds half away from
/// zero, so floor(x + 0.5) is used instead; the 0.0 early-return preserves ±0.
pub fn math_round(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let x = one_arg(ctx, args);
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
pub fn math_sqrt(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(ctx, args);
    if n < 0.0 {
        return Ok(JsValue::from_f64(f64::NAN));
    }
    Ok(helpers::js_number(n.sqrt()))
}

/// `Math.pow(x, y)` – x raised to the y-th power. Rust's `powf` follows IEEE
/// 754, which matches ES (including `Math.pow(NaN, 0) === 1`).
pub fn math_pow(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let x = one_arg(ctx, args);
    let y = args.get(1).copied().unwrap_or(JsValue::undefined());
    let y = ctx.to_number(y);
    Ok(JsValue::from_f64(x.powf(y)))
}

/// `Math.max(...)` – largest argument; no args → -Infinity; any NaN → NaN.
pub fn math_max(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    if args.is_empty() {
        return Ok(JsValue::from_f64(f64::NEG_INFINITY));
    }
    let mut max = f64::NEG_INFINITY;
    for &a in args {
        let n = ctx.to_number(a);
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
pub fn math_min(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    if args.is_empty() {
        return Ok(JsValue::from_f64(f64::INFINITY));
    }
    let mut min = f64::INFINITY;
    for &a in args {
        let n = ctx.to_number(a);
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
pub fn math_random(
    _ctx: &mut Ctx,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
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

/// Defines the direct `f64 → f64` Math functions. Each handler is the same
/// shape: coerce one argument via `ToNumber`, apply the Rust intrinsic,
/// canonicalize with `js_number` (NaN/±∞/non-Smi results stay doubles).
macro_rules! unary_math {
    ( $( $name:ident => $method:ident => $op:expr ),* $(,)? ) => {
        $(
            #[doc = concat!("`Math.", stringify!($method), "(x)` – the Rust `f64::", stringify!($op), "` subset of ES `Math.", stringify!($method), "`.")]
            pub fn $name(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
                let n = one_arg(ctx, args);
                let f: fn(f64) -> f64 = $op;
                Ok(helpers::js_number(f(n)))
            }
        )*
    };
}

unary_math! {
    math_sign => sign => |x: f64| if x.is_nan() || x == 0.0 { x } else if x > 0.0 { 1.0 } else { -1.0 },
    math_cbrt => cbrt => f64::cbrt,
    math_exp => exp => f64::exp,
    math_expm1 => expm1 => f64::exp_m1,
    math_log => log => f64::ln,
    math_log1p => log1p => f64::ln_1p,
    math_log2 => log2 => f64::log2,
    math_log10 => log10 => f64::log10,
    math_sin => sin => f64::sin,
    math_cos => cos => f64::cos,
    math_tan => tan => f64::tan,
    math_asin => asin => f64::asin,
    math_acos => acos => f64::acos,
    math_atan => atan => f64::atan,
    math_sinh => sinh => f64::sinh,
    math_cosh => cosh => f64::cosh,
    math_tanh => tanh => f64::tanh,
    math_asinh => asinh => f64::asinh,
    math_acosh => acosh => f64::acosh,
    math_atanh => atanh => f64::atanh,
}

/// `Math.atan2(y, x)` – two-argument arc tangent, NaN-propagating.
pub fn math_atan2(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let y = one_arg(ctx, args);
    let x = args
        .get(1)
        .copied()
        .map(|v| ctx.to_number(v))
        .unwrap_or(f64::NAN);
    Ok(helpers::js_number(y.atan2(x)))
}

/// `Math.hypot(...values)` – `ToNumber` each argument, then the Euclidean
/// norm; `Math.hypot()` is `+0` and any NaN/±∞ dominates per IEEE.
pub fn math_hypot(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut sum = 0.0f64;
    for &v in args {
        let n = ctx.to_number(v);
        if n.is_infinite() {
            return Ok(JsValue::from_f64(f64::INFINITY));
        }
        sum += n * n;
    }
    Ok(helpers::js_number(sum.sqrt()))
}

/// `Math.clz32(x)` – count leading zero bits of `ToUint32(x)`.
pub fn math_clz32(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(ctx, args);
    Ok(helpers::js_number(f64::from((n as u32).leading_zeros())))
}

/// `Math.imul(x, y)` – 32-bit integer multiply (wrapping).
pub fn math_imul(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let a = one_arg(ctx, args);
    let b = args
        .get(1)
        .copied()
        .map(|v| ctx.to_number(v))
        .unwrap_or(f64::NAN);
    let product = (a as i32).wrapping_mul(b as i32);
    Ok(helpers::js_number(f64::from(product)))
}

/// `Math.fround(x)` – round to the nearest IEEE binary32 value.
pub fn math_fround(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let n = one_arg(ctx, args);
    Ok(helpers::js_number(f64::from(n as f32)))
}

// -- Constants ----------------------------------------------------------------
//
// Each is a zero-argument handler evaluated once at install time by
// `install_value`; the same function also serves the (never observable)
// dispatch arm so the constant stays single-sourced in `define_builtins!`.

macro_rules! math_const {
    ( $( $name:ident => $method:ident => $value:expr ),* $(,)? ) => {
        $(
            #[doc = concat!("`Math.", stringify!($method), "` – the constant, installed as a data property via `install_value`.")]
            pub fn $name(_ctx: &mut Ctx, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
                Ok(JsValue::from_f64($value))
            }
        )*
    };
}

math_const! {
    math_const_e => E => std::f64::consts::E,
    math_const_ln2 => LN2 => std::f64::consts::LN_2,
    math_const_ln10 => LN10 => std::f64::consts::LN_10,
    math_const_log2e => LOG2E => std::f64::consts::LOG2_E,
    math_const_log10e => LOG10E => std::f64::consts::LOG10_E,
    math_const_pi => PI => std::f64::consts::PI,
    math_const_sqrt1_2 => SQRT1_2 => std::f64::consts::FRAC_1_SQRT_2,
    math_const_sqrt2 => SQRT2 => std::f64::consts::SQRT_2,
}
