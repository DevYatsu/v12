//! `JSON.parse` / `JSON.stringify`.
//!
//! Scope note: the callback-taking parts of the spec algorithms — the
//! `reviver` of `JSON.parse`, a function `replacer` of `JSON.stringify`, and
//! `toJSON` invocation — need interpreter re-entry. Registry natives run with
//! a bare `&mut Heap` and cannot call user code, so those paths are not
//! implemented here (they are ignored / not invoked); everything else is
//! spec-shaped. The pure `replacer` *array* form (a property allow-list) and
//! the `space` parameter are fully implemented.

use v12_heap::{Handle, Heap, JsObject, JsValue, StrStorage, V12Str};
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

// -- realm-linked errors ------------------------------------------------------

/// Builds a real error object carrying the realm `constructor` link.
///
/// `Ctx::make_error` skips that link when the context was built without an
/// explicit global (the `call_ctx` seam used by registry natives). Falling
/// back to the first registered realm global — exactly as `Ctx::intrinsic`
/// does — is what makes `assert.throws(SyntaxError, …)` see
/// `thrown.constructor === SyntaxError`.
fn throw_kind(ctx: &mut Ctx, kind: &str, msg: &str) -> Throw {
    let global = ctx
        .global
        .or_else(|| ctx.heap.realm_globals().first().copied());
    Throw::Value(super::registry::error_object(ctx.heap, global, kind, msg))
}

// -- parse --------------------------------------------------------------------

struct Parser<'p> {
    chars: &'p [u8],
    pos: usize,
}

impl<'p> Parser<'p> {
    fn ws(&mut self) {
        while self.pos < self.chars.len()
            && matches!(self.chars[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.chars.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("Unexpected character at position {}", self.pos))
        }
    }

    fn literal(&mut self, word: &str, value: JsValue) -> Result<JsValue, String> {
        if self.chars[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(format!("Unexpected token at position {}", self.pos))
        }
    }

    fn value(&mut self, ctx: &mut Ctx) -> Result<JsValue, String> {
        self.ws();
        match self.peek() {
            Some(b'{') => self.object(ctx),
            Some(b'[') => self.array(ctx),
            Some(b'"') => {
                let s = self.string()?;
                Ok(JsValue::string(ctx.heap.intern_text(&s)))
            }
            Some(b't') => self.literal("true", JsValue::from_bool(true)),
            Some(b'f') => self.literal("false", JsValue::from_bool(false)),
            Some(b'n') => self.literal("null", JsValue::null()),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("Unexpected token at position {}", self.pos)),
        }
    }

    fn object(&mut self, ctx: &mut Ctx) -> Result<JsValue, String> {
        self.expect(b'{')?;
        let obj = ctx.alloc_obj(JsObject::default());
        link_parsed_proto(ctx, obj, "Object");
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsValue::object(obj));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            self.expect(b':')?;
            let value = self.value(ctx)?;
            define_json_prop(ctx, obj, &key, value);
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsValue::object(obj));
                }
                _ => return Err(format!("Expected ',' or '}}' at position {}", self.pos)),
            }
        }
    }

    fn array(&mut self, ctx: &mut Ctx) -> Result<JsValue, String> {
        self.expect(b'[')?;
        let items: Vec<JsValue> = Vec::new();
        let arr = ctx.alloc_obj(JsObject::array(items));
        link_parsed_proto(ctx, arr, "Array");
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsValue::object(arr));
        }
        loop {
            let value = self.value(ctx)?;
            let len = ctx.heap.get(arr).element_len() as u32;
            ctx.heap.get_mut(arr).set_element(len, value);
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsValue::object(arr));
                }
                _ => return Err(format!("Expected ',' or ']' at position {}", self.pos)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err("Unterminated string".to_string()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let esc = self.peek().ok_or("Unterminated escape")?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self
                                .chars
                                .get(self.pos..self.pos + 4)
                                .ok_or("Truncated \\u escape")?;
                            let units = std::str::from_utf8(hex)
                                .ok()
                                .and_then(|s| u16::from_str_radix(s, 16).ok())
                                .ok_or("Invalid \\u escape")?;
                            self.pos += 4;
                            // Surrogate pair reassembly; lone surrogates are
                            // parse errors per spec.
                            if (0xD800..0xDC00).contains(&units)
                                && self.chars.get(self.pos) == Some(&b'\\')
                                && self.chars.get(self.pos + 1) == Some(&b'u')
                            {
                                let hex2 =
                                    self.chars.get(self.pos + 2..self.pos + 6).unwrap_or(&[]);
                                let low = std::str::from_utf8(hex2)
                                    .ok()
                                    .and_then(|s| u16::from_str_radix(s, 16).ok())
                                    .filter(|l| (0xDC00..0xE000).contains(l));
                                if let Some(low) = low {
                                    self.pos += 6;
                                    let cp = 0x10000
                                        + (u32::from(units) - 0xD800) * 0x400
                                        + (u32::from(low) - 0xDC00);
                                    out.push(char::from_u32(cp).ok_or("Invalid code point")?);
                                    continue;
                                }
                            }
                            char::from_u32(u32::from(units))
                                .map(|c| out.push(c))
                                .ok_or("Lone surrogate")?;
                        }
                        _ => return Err("Invalid escape".to_string()),
                    }
                }
                Some(_) => {
                    // Copy the whole UTF-8 sequence (input is a &str). Raw
                    // control code units are outside `JSONStringCharacter`
                    // and must be rejected (tests 15.12.1.1-g4-*,
                    // 15.12.2-2-*).
                    let rest = &self.chars[self.pos..];
                    let s = std::str::from_utf8(rest).map_err(|_| "Invalid UTF-8")?;
                    let c = s.chars().next().unwrap();
                    if (c as u32) < 0x20 {
                        return Err(format!(
                            "Unexpected control character at position {}",
                            self.pos
                        ));
                    }
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn number(&mut self) -> Result<JsValue, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == digits_start {
            return Err("Invalid number".to_string());
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text =
            std::str::from_utf8(&self.chars[start..self.pos]).map_err(|_| "Invalid number")?;
        let n = text
            .parse::<f64>()
            .map_err(|_| "Invalid number".to_string())?;
        // `-0` must survive parsing: `helpers::js_number` would canonicalize
        // it to +0 through the Smi fast path (test `text-negative-zero`).
        if n == 0.0 && text.starts_with('-') {
            return Ok(JsValue::from_f64(-0.0));
        }
        Ok(helpers::js_number(n))
    }
}

/// Links a freshly parsed object/array to the realm prototype of `intrinsic`
/// (`"Object"` / `"Array"`), so `Object.getPrototypeOf(JSON.parse('{}'))`
/// is `Object.prototype` (test `S15.12.2_A1`).
fn link_parsed_proto(ctx: &mut Ctx, obj: Handle<JsObject>, intrinsic: &str) {
    let Some(ctor) = ctx.intrinsic(intrinsic).and_then(|v| v.as_object()) else {
        return;
    };
    if let Some(proto) = ctx.heap.get(ctor).prototype {
        ctx.heap.get_mut(obj).prototype = Some(proto);
    }
}

/// Defines a parsed property on a JSON object (plain shape transition).
///
/// Keeps `Attrs::DEFAULT` (enumerable, writable, configurable): parsed
/// properties are ordinary own properties, unlike `BUILTIN`-attr installs,
/// so this deliberately does not route through `Ctx::define_data_prop`.
/// A duplicate key (`{"a":1,"a":2}`, `{"__proto__":1,"__proto__":2}`)
/// overwrites the existing slot instead of pushing a value the descriptor
/// never points at.
fn define_json_prop(ctx: &mut Ctx, obj: Handle<JsObject>, name: &str, value: JsValue) {
    let heap = &mut *ctx.heap;
    let h = if name.is_ascii() {
        heap.intern_string(V12Str::latin1_slice(name.as_bytes()))
    } else {
        heap.intern_string(V12Str::utf16(name.encode_utf16().collect()))
    };
    let key = v12_heap::PropKey::from_string(h);
    let shape = heap.shape_of_mut(obj);
    if let Some(slot) = heap.lookup_property(shape, key).and_then(|d| d.slot()) {
        heap.get_mut(obj).properties[slot as usize] = value;
        return;
    }
    let child = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    heap.bind_shape(obj, child);
    heap.get_mut(obj).properties.push(value);
    heap.get_mut(obj).property_keys.push(Some(key));
}

/// `JSON.parse(text [, reviver])`.
///
/// The `reviver` is not invoked (needs interpreter re-entry); the parsed
/// value is returned unfiltered.
pub fn json_parse(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(&v) => {
            // `Symbol` has no string form: `ToString` throws a TypeError.
            if v.is_symbol() {
                return Err(throw_kind(
                    ctx,
                    "TypeError",
                    "Cannot convert a Symbol value to a string",
                ));
            }
            // Numbers coerce through `Number::toString` rather than the
            // display path: `-0` must read `"0"` (test
            // `text-negative-zero`), which `format!("{n}")` gets wrong.
            if let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) {
                crate::builtins::number::number_to_string(n)
            } else {
                ctx.to_string(v)
            }
        }
        None => "undefined".to_string(),
    };
    let mut parser = Parser {
        chars: text.as_bytes(),
        pos: 0,
    };
    let result = parser
        .value(ctx)
        .map_err(|msg| throw_kind(ctx, "SyntaxError", &msg))?;
    parser.ws();
    if parser.pos != parser.chars.len() {
        return Err(throw_kind(
            ctx,
            "SyntaxError",
            &format!("Unexpected token at position {}", parser.pos),
        ));
    }
    Ok(result)
}

// -- stringify ----------------------------------------------------------------

/// The UTF-16 code units of a heap string, materializing composites first.
fn heap_units(heap: &mut Heap, h: Handle<V12Str>) -> Vec<u16> {
    heap.flatten(h);
    match &heap.get(h).storage {
        StrStorage::Latin1(bytes) => bytes.iter().map(|&b| u16::from(b)).collect(),
        StrStorage::Utf16(units) => units.clone(),
        // `flatten` just ran; composites cannot survive it.
        StrStorage::Cons { .. } | StrStorage::Sliced { .. } => Vec::new(),
    }
}

/// Appends `QuoteJSONString(value)` for `units` to `out`.
///
/// Lone surrogates are escaped as `\uXXXX` rather than replaced (the
/// well-formed `JSON.stringify` behavior, test `value-string-escape-unicode`).
fn quote_units(units: &[u16], out: &mut String) {
    out.push('"');
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        match u {
            0x0008 => {
                out.push_str("\\b");
                i += 1;
            }
            0x0009 => {
                out.push_str("\\t");
                i += 1;
            }
            0x000A => {
                out.push_str("\\n");
                i += 1;
            }
            0x000C => {
                out.push_str("\\f");
                i += 1;
            }
            0x000D => {
                out.push_str("\\r");
                i += 1;
            }
            0x0022 => {
                out.push_str("\\\"");
                i += 1;
            }
            0x005C => {
                out.push_str("\\\\");
                i += 1;
            }
            u if u < 0x20 => {
                out.push_str(&format!("\\u{u:04x}"));
                i += 1;
            }
            0xD800..=0xDBFF => {
                if let Some(&lo) = units.get(i + 1)
                    && (0xDC00..=0xDFFF).contains(&lo)
                {
                    let cp = 0x10000 + ((u32::from(u) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                    }
                    i += 2;
                } else {
                    out.push_str(&format!("\\u{u:04x}"));
                    i += 1;
                }
            }
            0xDC00..=0xDFFF => {
                out.push_str(&format!("\\u{u:04x}"));
                i += 1;
            }
            u => {
                if let Some(c) = char::from_u32(u32::from(u)) {
                    out.push(c);
                }
                i += 1;
            }
        }
    }
    out.push('"');
}

/// `QuoteJSONString(value)` for a heap string.
fn quote(heap: &mut Heap, h: Handle<V12Str>, out: &mut String) {
    let units = heap_units(heap, h);
    quote_units(&units, out);
}

/// The `gap` (indent unit) of `JSON.stringify` step 5–7.
///
/// Only a string or a number contributes; every other type (boolean, symbol,
/// `null`, `undefined`, plain object) is silently ignored. Boxed
/// `Number`/`String` values arrive here already unboxed to primitives.
fn gap(space: Option<JsValue>, heap: &mut Heap) -> String {
    let Some(v) = space else {
        return String::new();
    };
    if let Some(h) = v.as_string() {
        let units = heap_units(heap, h);
        let mut out = String::new();
        for &u in units.iter().take(10) {
            out.push(char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'));
        }
        return out;
    }
    let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) else {
        return String::new();
    };
    if n.is_nan() || n < 1.0 {
        return String::new();
    }
    // `ToIntegerOrInfinity` truncates toward zero; clamp at 10. A non-finite
    // `n` (Infinity) saturates the cast to `i64::MAX` and clamps to 10.
    let count = (n as i64).clamp(0, 10) as usize;
    " ".repeat(count)
}

/// The `PropertyList` of `JSON.stringify` step 4 (array `replacer`).
///
/// `None` when `replacer` is absent or not an array (no allow-list), `Some`
/// with the deduplicated names otherwise (an empty array means "serialize no
/// properties"). A callable replacer is left to the unsupported callback path
/// and reported as `None` here.
fn property_list(ctx: &mut Ctx, replacer: Option<JsValue>) -> Option<Vec<String>> {
    let v = replacer?;
    let obj = v.as_object()?;
    if ctx.heap.get(obj).kind != v12_heap::Kind::Array {
        return None;
    }
    let len = array_length(ctx, obj);
    let mut list: Vec<String> = Vec::new();
    for i in 0..len {
        let Some(item) = ctx.heap.get(obj).get_element(i) else {
            continue; // hole / absent: treated as `undefined` and skipped
        };
        let name = if let Some(h) = item.as_string() {
            ctx.string_text(h)
        } else if let Some(n) = item.as_smi().map(f64::from).or(item.as_f64()) {
            crate::builtins::number::number_to_string(n)
        } else {
            // Boxed String/Number objects would be ToString-coerced here;
            // the engine has no wrapper objects, so everything else is
            // ignored per `JSON.stringify` step 4.f.
            continue;
        };
        if !list.contains(&name) {
            list.push(name);
        }
    }
    Some(list)
}

/// `LengthOfArrayLike(array)`: the `length` slot, falling back to the element
/// store (sparse arrays keep `length` in `properties[0]`).
fn array_length(ctx: &Ctx, arr: Handle<JsObject>) -> u32 {
    if let Some(&v) = ctx.heap.get(arr).properties.first() {
        if let Some(n) = v.as_smi() {
            return n.max(0) as u32;
        }
        if let Some(n) = v.as_f64()
            && n.is_finite()
            && n >= 0.0
        {
            return n as u32;
        }
    }
    ctx.heap.get(arr).element_len() as u32
}

pub fn json_stringify(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or(JsValue::undefined());
    let replacer = args.get(1).copied();
    let unit = gap(args.get(2).copied(), ctx.heap);
    let list = property_list(ctx, replacer);
    let mut out = String::new();
    let mut seen: Vec<Handle<JsObject>> = Vec::new();
    match write_value(ctx, value, &unit, 0, &mut out, &mut seen, list.as_deref()) {
        Ok(true) => Ok(JsValue::string(ctx.heap.intern_text(&out))),
        Ok(false) => Ok(JsValue::undefined()),
        Err(WriteErr::Cyclic) => Err(throw_kind(
            ctx,
            "TypeError",
            "Converting circular structure to JSON",
        )),
        Err(WriteErr::BigInt) => Err(throw_kind(
            ctx,
            "TypeError",
            "Do not know how to serialize a BigInt",
        )),
    }
}

/// Failure markers for the stringify walk, distinct from "not representable"
/// (`Ok(false)`). Both must `Throw` a TypeError (the spec's
/// `SerializeJSONProperty`), not stringify as `undefined` / `null` or skip.
enum WriteErr {
    /// A cycle: `value` is already on the ancestor stack.
    Cyclic,
    /// A BigInt reached the serializer (no `toJSON`/replacer handled it).
    BigInt,
}

/// Writes `value`; `Ok(true)` when written, `Ok(false)` when the value is
/// not representable (functions, `undefined` — top-level or as an object
/// property), `Err` for cycles and BigInt (see [`WriteErr`]).
fn write_value(
    ctx: &mut Ctx,
    value: JsValue,
    unit: &str,
    depth: usize,
    out: &mut String,
    seen: &mut Vec<Handle<JsObject>>,
    list: Option<&[String]>,
) -> Result<bool, WriteErr> {
    if let Some(h) = value.as_string() {
        let mut buf = String::new();
        quote(ctx.heap, h, &mut buf);
        out.push_str(&buf);
        return Ok(true);
    }
    if let Some(n) = value.as_smi().map(f64::from).or(value.as_f64()) {
        if n.is_finite() {
            out.push_str(&crate::builtins::number::number_to_string(n));
        } else {
            out.push_str("null");
        }
        return Ok(true);
    }
    if value.is_true() {
        out.push_str("true");
        return Ok(true);
    }
    if value.is_false() {
        out.push_str("false");
        return Ok(true);
    }
    if value.is_null() {
        out.push_str("null");
        return Ok(true);
    }
    if value.is_bigint() {
        return Err(WriteErr::BigInt);
    }
    let Some(obj) = value.as_object() else {
        // undefined and symbols: not representable.
        return Ok(false);
    };
    // Callable objects (functions) are not representable: top level yields
    // `undefined`, array elements become `null`, object properties are
    // skipped (`value-function`).
    if ctx.heap.get(obj).kind == v12_heap::Kind::Function {
        return Ok(false);
    }
    // Cycle detection: an object already on the ancestor stack re-entering
    // is a circular structure (a `Throw`), not a skip.
    if seen.contains(&obj) {
        return Err(WriteErr::Cyclic);
    }
    seen.push(obj);
    let is_array = ctx.heap.get(obj).kind == v12_heap::Kind::Array;
    let result = if is_array {
        // The PropertyList (array replacer) still governs the *objects*
        // nested inside an array element, so it propagates here.
        write_array(ctx, obj, unit, depth, out, seen, list)
    } else {
        write_object(ctx, obj, unit, depth, out, seen, list)
    };
    seen.pop();
    result
}

fn newline_indent(unit: &str, depth: usize, out: &mut String) {
    if !unit.is_empty() {
        out.push('\n');
        out.push_str(&unit.repeat(depth));
    }
}

fn write_array(
    ctx: &mut Ctx,
    arr: Handle<JsObject>,
    unit: &str,
    depth: usize,
    out: &mut String,
    seen: &mut Vec<Handle<JsObject>>,
    list: Option<&[String]>,
) -> Result<bool, WriteErr> {
    let elems = ctx.heap.get(arr).elements_snapshot();
    if elems.is_empty() {
        out.push_str("[]");
        return Ok(true);
    }
    out.push('[');
    for (i, &v) in elems.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        newline_indent(unit, depth + 1, out);
        let v = if v.is_hole() { JsValue::null() } else { v };
        // Arrays represent holes/undefined/functions as null; a cycle or a
        // BigInt still throws (propagated, not nulled). The PropertyList
        // propagates into object elements.
        let mut buf = String::new();
        match write_value(ctx, v, unit, depth + 1, &mut buf, seen, list) {
            Ok(true) => out.push_str(&buf),
            Ok(false) => out.push_str("null"),
            Err(e) => return Err(e),
        }
    }
    newline_indent(unit, depth, out);
    out.push(']');
    Ok(true)
}

/// Own enumerable string-keyed properties of `obj` in specification
/// enumeration order: integer-indexed keys ascending, then the remaining
/// string keys in creation order. Accessor properties are included (the spec
/// reaches them through `Get`, which this walk cannot perform without
/// interpreter re-entry, so they render as `undefined`).
fn enumerable_own_keys(ctx: &mut Ctx, obj: Handle<JsObject>) -> Vec<(Handle<V12Str>, JsValue)> {
    let shape = ctx.heap.shape_of(obj);
    let mut raw: Vec<(Handle<V12Str>, JsValue)> = Vec::new();
    let descriptors: Vec<v12_heap::Descriptor> =
        ctx.heap.get(shape).descriptors.as_slice().to_vec();
    for desc in &descriptors {
        let Some(h) = desc.key().string() else {
            continue;
        };
        if !desc.attrs().enumerable() {
            continue;
        }
        let value = match ctx
            .heap
            .lookup_property(shape, desc.key())
            .and_then(|d| d.slot())
        {
            Some(slot) => ctx
                .heap
                .get(obj)
                .properties
                .get(slot as usize)
                .copied()
                .unwrap_or(JsValue::undefined()),
            None => JsValue::undefined(), // accessor: cannot invoke the getter
        };
        if value.is_hole() {
            continue; // deleted data property: descriptor kept, slot holed
        }
        raw.push((h, value));
    }
    // Dictionary-rung overflow (past the shape spill threshold), in insertion
    // sequence order.
    let mut overflow: Vec<(u32, v12_heap::PropKey, Option<u32>, bool)> = ctx
        .heap
        .get(obj)
        .dictionary
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|(k, e)| (e.seq, *k, Some(e.slot), e.is_accessor))
                .collect()
        })
        .unwrap_or_default();
    overflow.sort_by_key(|&(seq, _, _, _)| seq);
    for (_, key, slot, is_accessor) in overflow {
        let Some(h) = key.string() else {
            continue;
        };
        let value = match (slot, is_accessor) {
            (Some(slot), false) => ctx
                .heap
                .get(obj)
                .properties
                .get(slot as usize)
                .copied()
                .unwrap_or(JsValue::undefined()),
            _ => JsValue::undefined(),
        };
        if value.is_hole() {
            continue;
        }
        raw.push((h, value));
    }
    // Integer-first ordering (EnumerableOwnPropertyNames).
    let mut indexed: Vec<(u32, Handle<V12Str>, JsValue)> = Vec::new();
    let mut named: Vec<(Handle<V12Str>, JsValue)> = Vec::new();
    for (h, v) in raw {
        let text = ctx.string_text(h);
        if let Some(i) = array_index(&text) {
            indexed.push((i, h, v));
        } else {
            named.push((h, v));
        }
    }
    indexed.sort_by_key(|(i, _, _)| *i);
    indexed
        .into_iter()
        .map(|(_, h, v)| (h, v))
        .chain(named)
        .collect()
}

/// Canonical array-index string: a non-negative integer below `2^32-1` with
/// no leading zero (`"0"`, `"10"` qualify; `"01"`, `"1.0"`, `"-1"` do not).
fn array_index(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    let n: u32 = text.parse().ok()?;
    if n == u32::MAX { None } else { Some(n) }
}

fn write_object(
    ctx: &mut Ctx,
    obj: Handle<JsObject>,
    unit: &str,
    depth: usize,
    out: &mut String,
    seen: &mut Vec<Handle<JsObject>>,
    list: Option<&[String]>,
) -> Result<bool, WriteErr> {
    // `PropertyList` (array replacer) fixes both the key set and its order;
    // without one, use spec enumeration order.
    let entries: Vec<(Handle<V12Str>, JsValue)> = match list {
        Some(names) => {
            let mut entries = Vec::new();
            for name in names {
                let key = intern_key(ctx.heap, name);
                let shape = ctx.heap.shape_of(obj);
                let value = ctx
                    .heap
                    .lookup_property(shape, key)
                    .and_then(|d| d.slot())
                    .and_then(|slot| ctx.heap.get(obj).properties.get(slot as usize).copied());
                if let Some(v) = value
                    && !v.is_hole()
                {
                    entries.push((key.string().expect("string key"), v));
                }
            }
            entries
        }
        None => enumerable_own_keys(ctx, obj),
    };
    if entries.is_empty() {
        out.push_str("{}");
        return Ok(true);
    }
    out.push('{');
    let mut first = true;
    for (h, v) in entries {
        let mut buf = String::new();
        // undefined/function properties are skipped; a cycle or BigInt
        // throws (propagated, not skipped). The PropertyList applies at
        // every depth, so it propagates.
        match write_value(ctx, v, unit, depth + 1, &mut buf, seen, list) {
            Err(e) => return Err(e),
            Ok(false) => continue,
            Ok(true) => {}
        }
        if !first {
            out.push(',');
        }
        first = false;
        newline_indent(unit, depth + 1, out);
        let mut name = String::new();
        quote(ctx.heap, h, &mut name);
        out.push_str(&name);
        out.push(':');
        if !unit.is_empty() {
            out.push(' ');
        }
        out.push_str(&buf);
    }
    if first {
        out.clear();
        out.push_str("{}");
        return Ok(true);
    }
    newline_indent(unit, depth, out);
    out.push('}');
    Ok(true)
}

/// Interns a `PropertyList` name as a property key.
fn intern_key(heap: &mut Heap, name: &str) -> v12_heap::PropKey {
    let h = if name.is_ascii() {
        heap.intern_string(V12Str::latin1_slice(name.as_bytes()))
    } else {
        heap.intern_string(V12Str::utf16(name.encode_utf16().collect()))
    };
    v12_heap::PropKey::from_string(h)
}
