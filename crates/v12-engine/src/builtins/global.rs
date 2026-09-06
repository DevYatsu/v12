//! Global URI built-ins: `encodeURI`/`decodeURI` and the Component variants.

use v12_heap::{Heap, JsValue};
use v12_native::Throw;

use super::helpers;

/// Unreserved characters (RFC 2396 §2.3) plus the mark set — never encoded
/// by `encodeURIComponent`.
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
}

/// Characters `encodeURI` leaves unencoded (reserved URI syntax) on top of
/// the unreserved set.
fn is_uri_reserved(b: u8) -> bool {
    matches!(b, b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#')
}

fn encode(heap: &mut Heap, text: &str, keep_reserved: bool, what: &str) -> Result<JsValue, Throw> {
    // JS URIs are byte sequences over UTF-8; astral characters split into
    // percent-encoded UTF-8 octets.
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if is_unreserved(b) || (keep_reserved && is_uri_reserved(b)) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    let _ = what;
    Ok(JsValue::string(heap.intern_text(&out)))
}

fn decode(heap: &mut Heap, text: &str, component_only: bool, what: &str) -> Result<JsValue, Throw> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'%' {
            out.push(b);
            i += 1;
            continue;
        }
        // `%XY` — two hex digits required (URIError otherwise).
        let hex = bytes
            .get(i + 1)
            .copied()
            .zip(bytes.get(i + 2).copied())
            .and_then(|(hi, lo)| {
                let hi = (hi as char).to_digit(16);
                let lo = (lo as char).to_digit(16);
                hi.zip(lo).map(|(h, l)| (h * 16 + l) as u8)
            });
        let Some(decoded) = hex else {
            return Err(Throw::type_error(heap, format!("URIError: {what} malformed percent-encoding")));
        };
        if !component_only {
            // decodeURI must not decode characters that encodeURI keeps:
            // reserved (';/?:@&=+$,#') and '#' (already in the reserved set).
            if matches!(decoded,
                b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#')
            {
                return Err(Throw::type_error(heap, format!("URIError: {what} reserved character")));
            }
        }
        out.push(decoded);
        i += 3;
    }
    match String::from_utf8(out) {
        Ok(text) => Ok(JsValue::string(heap.intern_text(&text))),
        Err(_) => Err(Throw::type_error(heap, format!("URIError: {what} invalid UTF-8 sequence"))),
    }
}

fn arg_string(heap: &mut Heap, args: &[JsValue], what: &str) -> Result<String, Throw> {
    match args.first().copied() {
        Some(v) => Ok(helpers::value_text(heap, v)),
        None => Err(Throw::type_error(heap, format!("TypeError: {what} requires an argument"))),
    }
}

pub fn global_encode_uri(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = arg_string(heap, args, "encodeURI")?;
    encode(heap, &text, true, "encodeURI")
}

pub fn global_encode_uri_component(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = arg_string(heap, args, "encodeURIComponent")?;
    encode(heap, &text, false, "encodeURIComponent")
}

pub fn global_decode_uri(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = arg_string(heap, args, "decodeURI")?;
    decode(heap, &text, false, "decodeURI")
}

pub fn global_decode_uri_component(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = arg_string(heap, args, "decodeURIComponent")?;
    decode(heap, &text, true, "decodeURIComponent")
}
