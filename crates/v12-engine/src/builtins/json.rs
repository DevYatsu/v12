//! `JSON.parse` / `JSON.stringify`.

use v12_heap::{Handle, JsObject, JsValue};
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

// -- parse --------------------------------------------------------------------

struct Parser<'p> {
    chars: &'p [u8],
    pos: usize,
}

impl<'p> Parser<'p> {
    fn ws(&mut self) {
        while self.pos < self.chars.len() && matches!(self.chars[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
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
                                let hex2 = self.chars.get(self.pos + 2..self.pos + 6).unwrap_or(&[]);
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
                            char::from_u32(u32::from(units)).map(|c| out.push(c)).ok_or("Lone surrogate")?;
                        }
                        _ => return Err("Invalid escape".to_string()),
                    }
                }
                Some(_) => {
                    // Copy the whole UTF-8 sequence (input is a &str).
                    let rest = &self.chars[self.pos..];
                    let s = std::str::from_utf8(rest).map_err(|_| "Invalid UTF-8")?;
                    let c = s.chars().next().unwrap();
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
        let text = std::str::from_utf8(&self.chars[start..self.pos]).map_err(|_| "Invalid number")?;
        text.parse::<f64>()
            .map(helpers::js_number)
            .map_err(|_| "Invalid number".to_string())
    }
}

/// Defines a parsed property on a JSON object (plain shape transition).
///
/// Keeps `Attrs::DEFAULT` (enumerable, writable, configurable): parsed
/// properties are ordinary own properties, unlike `BUILTIN`-attr installs,
/// so this deliberately does not route through `Ctx::define_data_prop`.
fn define_json_prop(ctx: &mut Ctx, obj: Handle<JsObject>, name: &str, value: JsValue) {
    let heap = &mut *ctx.heap;
    let h = if name.is_ascii() {
        heap.intern_string(v12_heap::V12Str::latin1_slice(name.as_bytes()))
    } else {
        heap.intern_string(v12_heap::V12Str::utf16(name.encode_utf16().collect()))
    };
    let key = v12_heap::PropKey::from_string(h);
    let shape = heap.shape_of_mut(obj);
    let child = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    heap.bind_shape(obj, child);
    heap.get_mut(obj).properties.push(value);
    heap.get_mut(obj).property_keys.push(Some(key));
}

/// `JSON.parse(text)` – recursive-descent JSON to heap values. The `reviver`
/// argument is not supported (it needs callback re-entry).
pub fn json_parse(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(&v) => ctx.to_string(v),
        None => "undefined".to_string(),
    };
    let mut parser = Parser {
        chars: text.as_bytes(),
        pos: 0,
    };
    let result = parser
        .value(ctx)
        .map_err(|msg| ctx.syntax_error(format!("SyntaxError: {msg}")))?;
    parser.ws();
    if parser.pos != parser.chars.len() {
        return Err(ctx.syntax_error(format!(
            "SyntaxError: Unexpected token at position {}",
            parser.pos
        )));
    }
    Ok(result)
}

// -- stringify ----------------------------------------------------------------

/// Quotes a string per JSON (escapes `"`, `\`, control characters).
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Indent unit: `space` is either a string (truncated to 10) or a number of
/// spaces (clamped to 10).
fn indent_unit(space: Option<JsValue>, ctx: &mut Ctx) -> String {
    match space {
        Some(v) if v.as_string().is_some() => {
            let h = v.as_string().expect("checked");
            ctx.string_text(h).chars().take(10).collect()
        }
        Some(v) => {
            let n = ctx.to_number(v);
            if n.is_finite() && n > 0.0 {
                " ".repeat((n as usize).min(10))
            } else {
                String::new()
            }
        }
        None => String::new(),
    }
}

pub fn json_stringify(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or(JsValue::undefined());
    let unit = indent_unit(args.get(2).copied(), ctx);
    let mut out = String::new();
    let mut seen: Vec<Handle<JsObject>> = Vec::new();
    match write_value(ctx, value, &unit, 0, &mut out, &mut seen) {
        Ok(true) => Ok(JsValue::string(ctx.heap.intern_text(&out))),
        Ok(false) => Ok(JsValue::undefined()),
        Err(Cyclic) => Err(ctx.type_error("TypeError: Converting circular structure to JSON")),
    }
}

/// Cycle marker, distinct from "not representable" (`Ok(false)`).
/// A cyclic input must `Throw` a TypeError (the spec's
/// `SerializeJSONProperty` stack check), not stringify as `undefined` /
/// `null` or skip the property. `write_*` propagate `Err(Cyclic)` outward;
/// only the top-level `json_stringify` converts it to a `Throw` (the
/// partial `out` buffer is discarded).
struct Cyclic;

/// Writes `value`; `Ok(true)` when written, `Ok(false)` when the value is
/// not representable (functions, undefined — top-level or as an object
/// property), `Err(Cyclic)` when `value` is already on the ancestor stack.
fn write_value(
    ctx: &mut Ctx,
    value: JsValue,
    unit: &str,
    depth: usize,
    out: &mut String,
    seen: &mut Vec<Handle<JsObject>>,
) -> Result<bool, Cyclic> {
    if let Some(h) = value.as_string() {
        let text = ctx.string_text(h);
        out.push_str(&quote(&text));
        return Ok(true);
    }
    if let Some(n) = value.as_smi().map(f64::from).or(value.as_f64()) {
        if n.is_finite() {
            out.push_str(&crate::builtins::number::number_to_string(n));
        } else {
            out.push('n');
            out.push_str("ull");
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
    let Some(obj) = value.as_object() else {
        // undefined, symbols, functions: not representable.
        return Ok(false);
    };
    // Cycle detection: an object already on the ancestor stack re-entering
    // is a circular structure (a `Throw`), not a skip.
    if seen.contains(&obj) {
        return Err(Cyclic);
    }
    seen.push(obj);
    let is_array = ctx.heap.get(obj).kind == v12_heap::Kind::Array;
    let result = if is_array {
        write_array(ctx, obj, unit, depth, out, seen)
    } else {
        write_object(ctx, obj, unit, depth, out, seen)
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
) -> Result<bool, Cyclic> {
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
        // Arrays represent holes/undefined/functions as null; a cycle
        // still throws (propagated, not nulled).
        let mut buf = String::new();
        match write_value(ctx, v, unit, depth + 1, &mut buf, seen) {
            Ok(true) => out.push_str(&buf),
            Ok(false) => out.push_str("null"),
            Err(cyclic) => return Err(cyclic),
        }
    }
    newline_indent(unit, depth, out);
    out.push(']');
    Ok(true)
}

fn write_object(
    ctx: &mut Ctx,
    obj: Handle<JsObject>,
    unit: &str,
    depth: usize,
    out: &mut String,
    seen: &mut Vec<Handle<JsObject>>,
) -> Result<bool, Cyclic> {
    // Snapshot enumerable own string-keyed properties (handles first — the
    // conversion to text needs the heap mutably).
    let shape = ctx.heap.shape_of(obj);
    let mut pairs: Vec<(Handle<v12_heap::V12Str>, JsValue)> = Vec::new();
    for desc in ctx.heap.get(shape).descriptors.as_slice() {
        if let Some(h) = desc.key().string()
            && desc.attrs().enumerable()
            && let Some(slot) = desc.slot()
            && let Some(&v) = ctx.heap.get(obj).properties.get(slot as usize)
        {
            pairs.push((h, v));
        }
    }
    let mut entries: Vec<(String, JsValue)> = Vec::with_capacity(pairs.len());
    for (h, v) in pairs {
        entries.push((ctx.string_text(h), v));
    }
    if entries.is_empty() {
        out.push_str("{}");
        return Ok(true);
    }
    out.push('{');
    let mut first = true;
    for (name, v) in entries {
        let mut buf = String::new();
        // undefined/function properties are skipped; a cycle throws
        // (propagated, not skipped).
        match write_value(ctx, v, unit, depth + 1, &mut buf, seen) {
            Err(cyclic) => return Err(cyclic),
            Ok(false) => continue,
            Ok(true) => {}
        }
        if !first {
            out.push(',');
        }
        first = false;
        newline_indent(unit, depth + 1, out);
        out.push_str(&quote(&name));
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
