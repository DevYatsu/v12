//! JavaScript abstract operations over [`JsValue`]: coercions, equality,
//! arithmetic, and the string conversions the ISA's opcodes need.
//!
//! Every function here is total over canonical values and either returns a
//! result or a [`JSException`] carrying a ready-to-throw value. Numeric
//! results are canonicalized through [`box_number`]: integral values inside
//! the Smi range become Smis, everything else stays a raw double — with the
//! deliberate exception of negative zero, whose sign a Smi cannot carry.

use v12_heap::{Handle, Heap, JsValue, V12Str};

use crate::{JSException, KIND_FUNCTION, intern_text};

/// UTF-16 code units of a heap string, materializing composites first.
pub(crate) fn string_units(heap: &mut Heap, h: Handle<V12Str>) -> Vec<u16> {
    heap_string_units(heap, h)
}

// ---------------------------------------------------------------------------
// Coercions
// ---------------------------------------------------------------------------

/// The numeric content of a value if it is one natively (Smi or double).
/// Cross-representation equality: Smi 1 and double 1.0 are the same number.
pub(crate) fn num_of(v: JsValue) -> Option<f64> {
    v.as_f64().or_else(|| v.as_smi().map(f64::from))
}

/// ES ToBoolean. Falsy: `undefined`, `null`, `false`, `+0`, `-0`, `NaN`,
/// and the empty string. Everything else — objects included — is truthy.
pub(crate) fn to_boolean(heap: &Heap, v: JsValue) -> bool {
    if let Some(n) = num_of(v) {
        return n != 0.0 && !n.is_nan();
    }
    if let Some(b) = v.as_bool() {
        return b;
    }
    if v.is_string() {
        // `as_string` just proved the tag.
        return !heap.get(v.as_string().expect("string tag")).is_empty();
    }
    // null / undefined fell to num_of? No: they are boxed specials, handled
    // explicitly here because their numeric coercion is irrelevant.
    !(v.is_null() || v.is_undefined())
}

/// ES ToNumber for the subset reachable without built-ins. Objects coerce to
/// NaN: no user-visible valueOf/toString exists yet, and the default
/// `Object.prototype.valueOf` would produce NaN anyway.
pub(crate) fn to_number(heap: &mut Heap, v: JsValue) -> f64 {
    if let Some(n) = num_of(v) {
        return n;
    }
    if let Some(b) = v.as_bool() {
        return f64::from(u8::from(b));
    }
    if v.is_null() {
        return 0.0;
    }
    if v.is_string() {
        let h = v.as_string().expect("string tag");
        let units = heap_string_units(heap, h);
        return string_to_number(&units);
    }
    // undefined, symbols, bigints, objects.
    f64::NAN
}

/// The UTF-16 code units of a heap string, materializing composites first
/// (`flatten` is idempotent on flat leaves).
fn heap_string_units(heap: &mut Heap, h: Handle<V12Str>) -> Vec<u16> {
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => bytes.iter().map(|&b| u16::from(b)).collect(),
        v12_heap::StrStorage::Utf16(units) => units.clone(),
        // flatten just ran; composites are impossible now.
        v12_heap::StrStorage::Cons { .. } | v12_heap::StrStorage::Sliced { .. } => Vec::new(),
    }
}

/// ES ToNumber applied to string text (UTF-16 units). Accepts optional
/// surrounding ASCII whitespace, decimal / hexadecimal / octal / binary
/// literals with an optional exponent, and the `Infinity` spellings;
/// everything else is NaN. Non-ASCII whitespace is not trimmed — a
/// documented subset restriction.
pub(crate) fn string_to_number(units: &[u16]) -> f64 {
    const WS: [u16; 6] = [0x9, 0xA, 0xB, 0xC, 0xD, 0x20];
    let mut start = 0;
    let mut end = units.len();
    while start < end && WS.contains(&units[start]) {
        start += 1;
    }
    while end > start && WS.contains(&units[end - 1]) {
        end -= 1;
    }
    let s = &units[start..end];
    if s.is_empty() {
        return 0.0;
    }

    let mut idx = 0;
    let mut negative = false;
    if s[0] == u16::from(b'+') {
        idx = 1;
    } else if s[0] == u16::from(b'-') {
        negative = true;
        idx = 1;
    }
    let rest = &s[idx..];
    if rest.is_empty() {
        return f64::NAN;
    }

    let ascii: Option<String> = rest
        .iter()
        .map(|&u| char::from_u32(u32::from(u)).filter(|c| c.is_ascii()))
        .collect();
    let Some(text) = ascii else { return f64::NAN };
    let lower = text.to_ascii_lowercase();

    let magnitude = if lower == "infinity" {
        f64::INFINITY
    } else if let Some(hex) = lower.strip_prefix("0x") {
        parse_radix(hex, 16)
    } else if let Some(oct) = lower.strip_prefix("0o") {
        parse_radix(oct, 8)
    } else if let Some(bin) = lower.strip_prefix("0b") {
        parse_radix(bin, 2)
    } else if valid_decimal(&lower) {
        lower.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        return f64::NAN;
    };
    if negative { -magnitude } else { magnitude }
}

fn parse_radix(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return f64::NAN;
    }
    let mut acc = 0.0f64;
    for c in digits.chars() {
        acc = acc * f64::from(radix) + f64::from(c.to_digit(radix).expect("validated above"));
    }
    acc
}

/// Decimal-literal grammar check: `digits [. digits] | . digits`, optional
/// `[eE][+-]?digits`. At least one digit must appear in the mantissa.
fn valid_decimal(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut int_digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            frac_digits += 1;
        }
    }
    if int_digits + frac_digits == 0 {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let mut exp_digits = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            exp_digits += 1;
        }
        if exp_digits == 0 {
            return false;
        }
    }
    i == bytes.len()
}

/// ES Number::toString(10) for the common cases. Formatting follows Rust's
/// shortest-round-trip `Display`, which matches JS for every value with an
/// exponent in `[−6, 21)`; beyond that range JS switches to exponential
/// notation and this formatter does not — a documented divergence that
/// Tier-1 programs do not observe.
pub(crate) fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n == f64::INFINITY {
        "Infinity".into()
    } else if n == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        format!("{n}")
    }
}

/// ES ToString. Strings return themselves; numbers/booleans/specials intern
/// their spelling (idempotent — the interner deduplicates). Plain objects
/// render as `[object Object]` and functions as `function`, matching the
/// reference behavior for the current object model. Symbols throw TypeError.
pub(crate) fn to_js_string(heap: &mut Heap, v: JsValue) -> Result<Handle<V12Str>, JSException> {
    if let Some(h) = v.as_string() {
        return Ok(h);
    }
    if let Some(n) = num_of(v) {
        return Ok(intern_text(heap, &number_to_string(n)));
    }
    if let Some(b) = v.as_bool() {
        return Ok(intern_text(heap, if b { "true" } else { "false" }));
    }
    if v.is_undefined() {
        return Ok(intern_text(heap, "undefined"));
    }
    if v.is_null() {
        return Ok(intern_text(heap, "null"));
    }
    if v.is_object() {
        let o = v.as_object().expect("object tag");
        let text = if heap.get(o).kind == KIND_FUNCTION {
            "function"
        } else {
            "[object Object]"
        };
        return Ok(intern_text(heap, text));
    }
    if v.is_symbol() {
        return Err(JSException(JsValue::string(intern_text(
            heap,
            "TypeError: Cannot convert a Symbol value to a string",
        ))));
    }
    Err(JSException(JsValue::string(intern_text(
        heap,
        "InternalError: BigInt ToString is not supported yet",
    ))))
}

// ---------------------------------------------------------------------------
// Canonical numeric boxing
// ---------------------------------------------------------------------------

/// Boxes a computed double into its canonical representation: Smi when the
/// value is integral and fits the i31 payload, raw double otherwise.
/// Negative zero deliberately stays a double — a Smi cannot preserve its
/// sign, and `-0 === 0` must still hold while `String(-0)` differs.
pub(crate) fn box_number(n: f64) -> JsValue {
    if n.is_finite() && n.fract() == 0.0 && !(n == 0.0 && n.is_sign_negative()) {
        let lo = f64::from(JsValue::SMI_MIN);
        let hi = f64::from(JsValue::SMI_MAX);
        if (lo..=hi).contains(&n)
            && let Some(smi) = JsValue::from_i32_smi(n as i32)
        {
            return smi;
        }
    }
    JsValue::from_f64(n)
}

// ---------------------------------------------------------------------------
// Equality
// ---------------------------------------------------------------------------

/// ES IsStrictlyEqual. Numbers compare numerically (so a Smi equals the same
/// double, `+0 === -0`, NaN equals nothing), strings compare by text across
/// representations, references compare by identity.
pub(crate) fn strict_equals(heap: &Heap, a: JsValue, b: JsValue) -> bool {
    if let (Some(x), Some(y)) = (num_of(a), num_of(b)) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_string(), b.as_string()) {
        return heap.strings_equal(x, y);
    }
    if let (Some(x), Some(y)) = (a.as_bool(), b.as_bool()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_bigint(), b.as_bigint()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_symbol(), b.as_symbol()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_object(), b.as_object()) {
        return x == y;
    }
    // Same-special comparisons (`undefined === undefined`, `null === null`)
    // and cross-type pairs both reduce to bit identity.
    a.bits() == b.bits() && (a.is_undefined() || a.is_null())
}

/// ES IsLooseEqual restricted to the cases reachable without ToPrimitive:
/// the null/undefined pair, number↔string numeric comparison, booleans
/// coerced to numbers first, and same-type strict comparison otherwise.
/// Object↔non-object is false — no user-defined conversion exists yet.
pub(crate) fn loose_equals(heap: &mut Heap, a: JsValue, b: JsValue) -> bool {
    if a.is_null() || a.is_undefined() {
        return b.is_null() || b.is_undefined();
    }
    if b.is_null() || b.is_undefined() {
        return false;
    }
    if let (Some(x), Some(y)) = (num_of(a), num_of(b)) {
        return x == y;
    }
    let a_str = a.is_string();
    let b_str = b.is_string();
    if a_str != b_str {
        // One side is a string, the other a non-string non-number reference:
        // only the boolean arm can proceed (booleans were not caught above).
        let (num_side, other) = if a_str { (b, a) } else { (a, b) };
        if let Some(bool_val) = other.as_bool() {
            return to_number(heap, JsValue::from_f64(if bool_val { 1.0 } else { 0.0 }))
                == to_number(heap, num_side);
        }
        return false;
    }
    if a_str {
        // Both strings: textual comparison.
        let (x, y) = (
            a.as_string().expect("string"),
            b.as_string().expect("string"),
        );
        return heap.strings_equal(x, y);
    }
    if a.as_bool().is_some() || b.as_bool().is_some() {
        let na = f64::from(u8::from(a.as_bool().expect("bool arm")));
        let nb = f64::from(u8::from(b.as_bool().expect("bool arm")));
        return na == nb;
    }
    if a.is_object() && b.is_object() {
        return strict_equals(heap, a, b);
    }
    false
}

// ---------------------------------------------------------------------------
// Comparisons
// ---------------------------------------------------------------------------

/// UTF-16 code-unit lexicographic ordering over two heap strings. Composites
/// are flattened in place first; flattening preserves text and hash.
pub(crate) fn compare_strings(
    heap: &mut Heap,
    a: Handle<V12Str>,
    b: Handle<V12Str>,
) -> std::cmp::Ordering {
    use v12_heap::StrStorage;
    heap.flatten(a);
    heap.flatten(b);
    fn units<'h>(heap: &'h Heap, h: Handle<V12Str>) -> Box<dyn Iterator<Item = u16> + 'h> {
        match &heap.get(h).storage {
            StrStorage::Latin1(bytes) => Box::new(bytes.iter().copied().map(u16::from)),
            StrStorage::Utf16(units) => Box::new(units.iter().copied()),
            _ => unreachable!("flattened above"),
        }
    }
    let ha = units(heap, a);
    let hb = units(heap, b);
    // Iterator::cmp is exactly lexicographic code-unit order.
    ha.cmp(hb)
}

/// ES abstract relational comparison (`<` `<=` `>` `>`): strings compare as
/// text, anything else numerically, with any NaN operand making every
/// relation false.
pub(crate) fn compare(op: crate::Opcode, heap: &mut Heap, l: JsValue, r: JsValue) -> bool {
    use std::cmp::Ordering;
    if let (Some(lh), Some(rh)) = (l.as_string(), r.as_string()) {
        return match op {
            crate::Opcode::Lt => compare_strings(heap, lh, rh) == Ordering::Less,
            crate::Opcode::Le => compare_strings(heap, lh, rh) != Ordering::Greater,
            crate::Opcode::Gt => compare_strings(heap, lh, rh) == Ordering::Greater,
            crate::Opcode::Ge => compare_strings(heap, lh, rh) != Ordering::Less,
            _ => unreachable!("compare() only sees relational opcodes"),
        };
    }
    let (ln, rn) = (to_number(heap, l), to_number(heap, r));
    if ln.is_nan() || rn.is_nan() {
        return false;
    }
    match op {
        crate::Opcode::Lt => ln < rn,
        crate::Opcode::Le => ln <= rn,
        crate::Opcode::Gt => ln > rn,
        crate::Opcode::Ge => ln >= rn,
        _ => unreachable!("compare() only sees relational opcodes"),
    }
}

// ---------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------

/// ES `+`: string concatenation when either operand is a string, numeric
/// addition otherwise.
///
/// Untraced: `powf` diverges from IEEE `**` for a few edge inputs; see
/// [`js_pow`], which patches the known ones.
pub(crate) fn add(heap: &mut Heap, l: JsValue, r: JsValue) -> Result<JsValue, JSException> {
    if l.is_string() || r.is_string() {
        let ls = to_js_string(heap, l)?;
        let rs = to_js_string(heap, r)?;
        return Ok(JsValue::string(heap.concat(ls, rs)));
    }
    Ok(box_number(to_number(heap, l) + to_number(heap, r)))
}

pub(crate) fn sub(heap: &mut Heap, l: JsValue, r: JsValue) -> JsValue {
    smi_fast(l, r, |a, b| a.checked_sub(b))
        .unwrap_or_else(|| box_number(to_number(heap, l) - to_number(heap, r)))
}

pub(crate) fn mul(heap: &mut Heap, l: JsValue, r: JsValue) -> JsValue {
    smi_fast(l, r, |a, b| a.checked_mul(b))
        .unwrap_or_else(|| box_number(to_number(heap, l) * to_number(heap, r)))
}

pub(crate) fn div(heap: &mut Heap, l: JsValue, r: JsValue) -> JsValue {
    box_number(to_number(heap, l) / to_number(heap, r))
}

/// ES `%` on doubles. Rust's `%` is IEEE truncated remainder — identical to
/// JS semantics, including the sign following the dividend.
pub(crate) fn modulo(heap: &mut Heap, l: JsValue, r: JsValue) -> JsValue {
    box_number(to_number(heap, l) % to_number(heap, r))
}

/// ES `**`. `f64::powf` agrees with JS except when `|base| == 1` and the
/// exponent is infinite: IEEE says ±1, the spec says NaN. Patch that case.
pub(crate) fn js_pow(heap: &mut Heap, l: JsValue, r: JsValue) -> JsValue {
    let (ln, rn) = (to_number(heap, l), to_number(heap, r));
    let result = if ln.abs() == 1.0 && rn.is_infinite() {
        f64::NAN
    } else {
        ln.powf(rn)
    };
    box_number(result)
}

/// Smi×Smi fast path: applies `op` in `i64` space and boxes back only when
/// the exact result fits the Smi range. Overflow falls through to the double
/// path via `None`.
fn smi_fast(l: JsValue, r: JsValue, op: impl Fn(i64, i64) -> Option<i64>) -> Option<JsValue> {
    let (a, b) = (i64::from(l.as_smi()?), i64::from(r.as_smi()?));
    let n = op(a, b)?;
    let lo = i64::from(JsValue::SMI_MIN);
    let hi = i64::from(JsValue::SMI_MAX);
    if !(lo..=hi).contains(&n) {
        return None;
    }
    // Range-checked immediately above.
    Some(JsValue::from_i32_smi(n as i32).expect("result fits the Smi range"))
}

/// ES ToUint32: NaN and the infinities map to 0; finite values truncate
/// toward zero and wrap modulo 2³².
pub(crate) fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() {
        return 0;
    }
    // rem_euclid keeps the result in [0, 2^32), which casts exactly.
    n.trunc().rem_euclid(4_294_967_296.0) as u32
}

/// ES ToInt32: the signed view of [`to_uint32`].
pub(crate) fn to_int32(n: f64) -> i32 {
    to_uint32(n) as i32
}
