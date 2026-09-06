//! Global URI built-ins: `encodeURI`/`decodeURI` and the Component variants.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.

use v12_heap::JsValue;
use v12_native::Throw;

use super::ctx::Ctx;

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

fn encode(ctx: &mut Ctx, text: &str, keep_reserved: bool) -> Result<JsValue, Throw> {
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
    Ok(JsValue::string(ctx.heap.intern_text(&out)))
}

fn decode(
    ctx: &mut Ctx,
    text: &str,
    component_only: bool,
    what: &str,
) -> Result<JsValue, Throw> {
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
            return Err(ctx.type_error(format!("URIError: {what} malformed percent-encoding")));
        };
        if !component_only {
            // decodeURI must not decode escapes for characters that
            // encodeURI leaves unencoded (reserved `;/?:@&=+$,#`): the
            // original `%XX` passes through verbatim (original case kept),
            // per spec — it is not a URIError.
            if matches!(decoded,
                b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#')
            {
                out.extend_from_slice(&bytes[i..i + 3]);
                i += 3;
                continue;
            }
        }
        out.push(decoded);
        i += 3;
    }
    match String::from_utf8(out) {
        Ok(text) => Ok(JsValue::string(ctx.heap.intern_text(&text))),
        Err(_) => Err(ctx.type_error(format!("URIError: {what} invalid UTF-8 sequence"))),
    }
}

fn arg_string(ctx: &mut Ctx, args: &[JsValue], what: &str) -> Result<String, Throw> {
    match args.first().copied() {
        Some(v) => Ok(ctx.to_string(v)),
        None => Err(ctx.type_error(format!("TypeError: {what} requires an argument"))),
    }
}

pub fn global_encode_uri(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = arg_string(ctx, args, "encodeURI")?;
    encode(ctx, &text, true)
}

pub fn global_encode_uri_component(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = arg_string(ctx, args, "encodeURIComponent")?;
    encode(ctx, &text, false)
}

pub fn global_decode_uri(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = arg_string(ctx, args, "decodeURI")?;
    decode(ctx, &text, false, "decodeURI")
}

pub fn global_decode_uri_component(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = arg_string(ctx, args, "decodeURIComponent")?;
    decode(ctx, &text, true, "decodeURIComponent")
}
