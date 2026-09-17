//! JavaScript abstract operations over [`JsValue`]: coercions, equality,
//! arithmetic, and the string conversions the ISA's opcodes need.
//!
//! Every function here is total over canonical values and either returns a
//! result or a [`JSException`] carrying a ready-to-throw value. Numeric
//! results are canonicalized through [`box_number`]: integral values inside
//! the Smi range become Smis, everything else stays a raw double — with the
//! deliberate exception of negative zero, whose sign a Smi cannot carry.

use v12_heap::{Handle, Heap, JsObject, JsValue, Kind, V12Str};

use crate::{Interp, JSException};

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

/// ES Number::toString(10). Uses Rust's shortest-round-trip `{:e}`
/// formatting for the digit string, then applies the spec's decimal vs
/// exponential selection (`k ≤ n ≤ 21` decimal, `-6 < n ≤ 0` small decimal,
/// otherwise exponential with an explicit `+`/`-` exponent sign).
pub(crate) fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n == f64::INFINITY {
        return "Infinity".into();
    }
    if n == f64::NEG_INFINITY {
        return "-Infinity".into();
    }
    if n == 0.0 {
        return if n.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    // `{:e}` yields shortest digits: `d[.ddd]e±X` (no leading zeros, no
    // trailing zeros in the fraction).
    let sci = format!("{:e}", n);
    let (mantissa, exp_text) = sci.split_once('e').expect("scientific form");
    let exp: i32 = exp_text.parse().expect("decimal exponent");
    let neg = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let k = digits.len() as i32; // digit count (spec's `k`)
    let ni = exp + 1; // spec's `n`: decimal point position
    let sign = if neg { "-" } else { "" };
    let body = if k <= ni && ni <= 21 {
        // digits followed by ni-k zeros
        let mut t = digits.clone();
        for _ in 0..(ni - k) {
            t.push('0');
        }
        t
    } else if 0 < ni && ni <= 21 {
        // dot after ni digits
        let (head, tail) = digits.split_at(ni as usize);
        format!("{head}.{tail}")
    } else if -6 < ni && ni <= 0 {
        // 0.000ddd
        let mut t = String::from("0.");
        for _ in 0..(-ni) {
            t.push('0');
        }
        t.push_str(&digits);
        t
    } else {
        // exponential: d[.ddd]e±(n-1)
        let e = ni - 1;
        let (esign, emag) = if e < 0 { ('-', -e) } else { ('+', e) };
        if k == 1 {
            format!("{digits}e{esign}{emag}")
        } else {
            let (head, tail) = digits.split_at(1);
            format!("{head}.{tail}e{esign}{emag}")
        }
    };
    format!("{sign}{body}")
}

/// ES ToPrimitive hint. `Default` and `Number` share the `valueOf`-first
/// order; `String` uses `toString`-first (`OrdinaryToPrimitive`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrimitiveHint {
    Default,
    String,
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
        return Ok(heap.intern_text(&number_to_string(n)));
    }
    if let Some(b) = v.as_bool() {
        return Ok(heap.intern_text(if b { "true" } else { "false" }));
    }
    if v.is_undefined() {
        return Ok(heap.intern_text("undefined"));
    }
    if v.is_null() {
        return Ok(heap.intern_text("null"));
    }
    if v.is_object() {
        let o = v.as_object().expect("object tag");
        if heap.get(o).kind == Kind::Function {
            return Ok(heap.intern_text("function"));
        }
        // Real arrays render as their comma-joined elements (so `map`
        // results don't display as `[object Object]`); every other object
        // keeps the reference behavior.
        if heap.get(o).kind == Kind::Array {
            let text = array_join_text(heap, o, 0);
            return Ok(heap.intern_text(&text));
        }
        return Ok(heap.intern_text("[object Object]"));
    }
    if v.is_symbol() {
        let (kind, msg) = v12_native::parse_error_text(
            "TypeError: Cannot convert a Symbol value to a string",
            "TypeError",
        );
        return Err(JSException(v12_native::error_object(heap, kind, msg)));
    }
    let (kind, msg) = v12_native::parse_error_text(
        "InternalError: BigInt ToString is not supported yet",
        "TypeError",
    );
    Err(JSException(v12_native::error_object(heap, kind, msg)))
}

/// Comma-joined element text of a real array (`undefined`/`null`/holes
/// render empty, matching `Array.prototype.join`). Nested arrays recurse;
/// anything unrenderable renders empty so display never throws. `depth`
/// caps the recursion so cyclic arrays terminate.
fn array_join_text(heap: &mut Heap, obj: Handle<JsObject>, depth: usize) -> String {
    if depth > 8 {
        return String::new();
    }
    // Snapshot before formatting: rendering an element may allocate (and
    // thus collect), invalidating a live borrow of the element store.
    let elements: Vec<JsValue> = heap.get(obj).elements_snapshot();
    let mut parts = Vec::with_capacity(elements.len());
    for v in elements {
        if v.is_undefined() || v.is_null() || v.is_hole() {
            parts.push(String::new());
        } else if let Some(nested) = v.as_object().filter(|h| heap.get(*h).kind == Kind::Array) {
            parts.push(array_join_text(heap, nested, depth + 1));
        } else {
            match to_js_string(heap, v) {
                Ok(h) => parts.push(String::from_utf16_lossy(&string_units(heap, h))),
                Err(_) => parts.push(String::new()),
            }
        }
    }
    parts.join(",")
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

/// ES IsLooseEqual (7.2.14) restricted to the cases reachable without
/// ToPrimitive: the null/undefined pair, boolean operands coerced to numbers
/// first, number↔string numeric comparison, and same-type comparison
/// otherwise. Object↔non-object is false — no user-defined conversion exists.
pub(crate) fn loose_equals(heap: &mut Heap, a: JsValue, b: JsValue) -> bool {
    if a.is_null() || a.is_undefined() {
        return b.is_null() || b.is_undefined();
    }
    if b.is_null() || b.is_undefined() {
        return false;
    }
    // ES 7.2.14 step 3: a boolean operand becomes a number, then the
    // comparison re-dispatches through the remaining arms.
    if a.as_bool().is_some() || b.as_bool().is_some() {
        let na = to_number(heap, a);
        let nb = to_number(heap, b);
        return loose_equals(heap, JsValue::from_f64(na), JsValue::from_f64(nb));
    }
    let (a_num, b_num) = (num_of(a), num_of(b));
    if let (Some(x), Some(y)) = (a_num, b_num) {
        return x == y;
    }
    // ES 7.2.14 step 4: number vs string compares ToNumber(string) with the
    // number (a string that does not parse yields NaN, which equals nothing).
    if let Some(x) = a_num
        && b.is_string()
    {
        return x == to_number(heap, b);
    }
    if let Some(y) = b_num
        && a.is_string()
    {
        return y == to_number(heap, a);
    }
    if a.is_string() && b.is_string() {
        // Both strings: textual comparison.
        let (x, y) = (
            a.as_string().expect("string"),
            b.as_string().expect("string"),
        );
        return heap.strings_equal(x, y);
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

/// ES `**`. `f64::powf` agrees with JS except when `|base| == 1` and the
/// exponent is infinite: IEEE says ±1, the spec says NaN. Patch that case.
pub(crate) fn js_pow(ln: f64, rn: f64) -> JsValue {
    let result = if ln.abs() == 1.0 && rn.is_infinite() {
        f64::NAN
    } else {
        ln.powf(rn)
    };
    box_number(result)
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

impl Interp<'_> {
    /// ES IsLooseEqual with object operands converted through
    /// [`Self::to_primitive_default`] first. Object↔object compares the
    /// original objects by identity (no conversion).
    pub(crate) fn loose_equals(&mut self, a: JsValue, b: JsValue) -> Result<bool, JSException> {
        if a.is_object() && !b.is_object() {
            let prim = self.to_primitive_default(a)?;
            return Ok(loose_equals(self.heap, prim, b));
        }
        if b.is_object() && !a.is_object() {
            let prim = self.to_primitive_default(b)?;
            return Ok(loose_equals(self.heap, a, prim));
        }
        Ok(loose_equals(self.heap, a, b))
    }

    /// ES abstract relational comparison with object operands converted
    /// through [`Self::to_primitive_default`] first.
    pub(crate) fn compare(
        &mut self,
        op: crate::Opcode,
        l: JsValue,
        r: JsValue,
    ) -> Result<bool, JSException> {
        if l.is_object() {
            let prim = self.to_primitive_default(l)?;
            return self.compare(op, prim, r);
        }
        if r.is_object() {
            let prim = self.to_primitive_default(r)?;
            return self.compare(op, l, prim);
        }
        Ok(compare(op, self.heap, l, r))
    }

    /// ES ToPrimitive with the default (no) hint: `valueOf` first, then
    /// `toString`. Primitives pass through unchanged; an object whose
    /// methods yield no primitive is a TypeError.
    pub(crate) fn to_primitive_default(&mut self, v: JsValue) -> Result<JsValue, JSException> {
        self.to_primitive_with_hint(v, PrimitiveHint::Default)
    }

    /// ES ToPrimitive (7.1.1) honoring the hint. `Default` and `Number` try
    /// `valueOf` first; `String` tries `toString` first (`OrdinaryToPrimitive`
    /// order). Primitives pass through unchanged; an object whose methods
    /// yield no primitive is a TypeError.
    pub(crate) fn to_primitive_with_hint(
        &mut self,
        v: JsValue,
        hint: PrimitiveHint,
    ) -> Result<JsValue, JSException> {
        if !v.is_object() {
            return Ok(v);
        }
        let order: [&str; 2] = if hint == PrimitiveHint::String {
            ["toString", "valueOf"]
        } else {
            ["valueOf", "toString"]
        };
        for name in order {
            let key = self.new_temp_key(name);
            let m = self.get_property(0, 0, v, key)?;
            if let Some(f) = m.as_object() {
                self.stack.push(JsValue::object(f));
                self.gc_protect();
                let r = self.call_inline(f, v, &[]);
                self.stack.pop();
                let r = r?;
                if !r.is_object() {
                    return Ok(r);
                }
            }
        }
        Err(JSException(self.error_value(
            "TypeError: Cannot convert object to primitive value",
        )))
    }

    /// ES ToNumber, routing objects through [`Self::to_primitive_default`]
    /// first (user `valueOf`/`toString` conversions).
    pub(crate) fn to_number_value(&mut self, v: JsValue) -> Result<f64, JSException> {
        if v.is_object() {
            let prim = self.to_primitive_default(v)?;
            return Ok(to_number(self.heap, prim));
        }
        Ok(to_number(self.heap, v))
    }

    /// ES ToString (7.1.17) with the string hint: objects run
    /// `OrdinaryToPrimitive(hint "string")` (user `toString` first), the
    /// resulting primitive is stringified, and a Symbol result throws
    /// TypeError. Primitives stringify directly.
    pub(crate) fn to_string_value(&mut self, v: JsValue) -> Result<JsValue, JSException> {
        if v.is_object() {
            let prim = self.to_primitive_with_hint(v, PrimitiveHint::String)?;
            if prim.is_symbol() {
                return Err(self.symbol_to_string_type_error());
            }
            let h = to_js_string(self.heap, prim)?;
            return Ok(JsValue::string(h));
        }
        let h = to_js_string(self.heap, v)?;
        Ok(JsValue::string(h))
    }

    /// The ES TypeError for `ToString(symbol)`.
    fn symbol_to_string_type_error(&mut self) -> JSException {
        JSException(self.error_value("TypeError: Cannot convert a Symbol value to a string"))
    }

    /// ES ToPropertyKey (7.1.19): strings and symbols pass through; any other
    /// primitive is `ToString`ed; an object is converted via
    /// [`Self::to_primitive_default`] first, so a key object's
    /// `valueOf`/`toString` runs at a defined point rather than silently
    /// rendering as `[object Object]`.
    ///
    /// v1 deviation: the spec's hint is `string` (toString first), while
    /// `to_primitive_default` is the default hint (valueOf first). Shared with
    /// every coercion site in the interpreter; the two orders differ only for
    /// an object that defines both methods to yield primitives.
    pub(crate) fn to_property_key_value(&mut self, v: JsValue) -> Result<JsValue, JSException> {
        if v.is_string() || v.is_symbol() {
            return Ok(v);
        }
        let prim = if v.is_object() {
            self.to_primitive_default(v)?
        } else {
            v
        };
        // A symbol can only appear here via `to_primitive_default`, so it is
        // checked before the string conversion (which throws for symbols).
        if prim.is_symbol() {
            return Ok(prim);
        }
        let h = to_js_string(self.heap, prim)?;
        Ok(JsValue::string(h))
    }
}
