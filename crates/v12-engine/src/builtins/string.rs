//! String built-ins.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3 + §8): all
//! bodies take `&mut Ctx`. The regexp-backed methods (`match`, `replace`,
//! `search`, `split`) read the per-registry compiled-pattern cache out of the
//! `Ctx` capability fed by the registry at dispatch.

use v12_heap::{Handle, Heap, JsValue, V12Str};
use v12_native::Throw;

use super::{ctx::Ctx, helpers, regexp};

/// The `this` string primitive, or a `TypeError` naming `method`.
fn this_string(ctx: &mut Ctx, this: JsValue, method: &str) -> Result<Handle<V12Str>, Throw> {
    this.as_string()
        .ok_or_else(|| ctx.type_error(format!("{method} called on non-string")))
}

/// The regexp argument as a compiled-regexp object, or `None` when the
/// argument is not an object of `Kind::RegExp` (callers fall back to plain
/// text matching).
fn as_regexp(heap: &Heap, v: Option<&JsValue>) -> Option<Handle<v12_heap::JsObject>> {
    v.and_then(|v| v.as_object())
        .filter(|&re| heap.get(re).kind == v12_heap::Kind::RegExp)
}

/// `String.prototype.charAt(index)` – returns a single-character string.
pub fn string_char_at(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let handle = this_string(ctx, this, "String.prototype.charAt")?;
    let index = args.first().and_then(to_index).unwrap_or(0);
    let units = string_units(&mut *ctx.heap, handle);
    let unit = match units.get(index as usize) {
        Some(&unit) => vec![unit],
        None => Vec::new(),
    };
    let h = ctx.heap.intern_string(v12_heap::V12Str::utf16(unit));
    Ok(JsValue::string(h))
}

/// `String.prototype.slice(start, end)` – returns a sliced view.
pub fn string_slice(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let handle = this_string(ctx, this, "String.prototype.slice")?;
    let len = ctx.heap.get(handle).len() as i64;
    let start = args.first().and_then(to_integer).unwrap_or(0);
    let end = args.get(1).and_then(to_integer).unwrap_or(len);
    let from = clamp_index(start, len) as u32;
    let to = clamp_index(end, len) as u32;
    let (from, to) = if from > to { (to, to) } else { (from, to) };
    let slice_len = to.saturating_sub(from);
    let heap = &mut *ctx.heap;
    let Some(sliced) = heap.slice_string(handle, from, slice_len) else {
        let h = heap.intern_string(v12_heap::V12Str::latin1(Vec::new()));
        return Ok(JsValue::string(h));
    };
    // Flatten lazily sliced strings when eagerly queried, otherwise keep lazy.
    // For the built-in return value, keep as heap handle.
    Ok(JsValue::string(sliced))
}

fn to_index(v: &JsValue) -> Option<i64> {
    if let Some(n) = v.as_smi() {
        return Some(i64::from(n));
    }
    if let Some(n) = v.as_f64()
        && n.is_finite()
    {
        return Some(n.trunc() as i64);
    }
    None
}

fn to_integer(v: &JsValue) -> Option<i64> {
    to_index(v)
}

fn clamp_index(index: i64, len: i64) -> i64 {
    if index < 0 {
        (len + index).max(0)
    } else {
        index.min(len)
    }
}

fn string_units(heap: &mut Heap, handle: Handle<V12Str>) -> Vec<u16> {
    heap.flatten(handle);
    match &heap.get(handle).storage {
        v12_heap::StrStorage::Latin1(bytes) => bytes.iter().map(|&b| u16::from(b)).collect(),
        v12_heap::StrStorage::Utf16(units) => units.clone(),
        _ => Vec::new(),
    }
}

/// One regexp match: `(start, end)` byte span in the subject text plus
/// capture groups 1–9 (`None` = group did not participate).
type MatchSpan = (usize, usize, Vec<Option<String>>);

/// Byte offset in `text` for a UTF-16 code-unit index. The regexp engine
/// matches over UTF-16 units (`exec`'s `index` is a unit index) while
/// splicing happens on the UTF-8 rendering of `text`; the two coincide for
/// ASCII but an astral char is 2 units vs 4 bytes. A unit index landing
/// inside a char (possible after lossy lone-surrogate replacement) rounds
/// up to the next char start, so the result is always a char boundary.
fn utf16_byte_offset(text: &str, units: usize) -> usize {
    let mut count = 0usize;
    for (off, ch) in text.char_indices() {
        if count >= units {
            return off;
        }
        count += ch.len_utf16();
    }
    text.len()
}

/// Drives `RegExp.prototype.exec` over `text_h` until exhaustion (or the
/// first match when `!global`), collecting each match's span as byte offsets
/// in `text` plus capture groups 1–9. Guards against infinite loops on
/// zero-width matches by forcing `lastIndex` forward.
fn collect_match_spans(
    ctx: &mut Ctx,
    re: Handle<v12_heap::JsObject>,
    text_h: Handle<V12Str>,
    text: &str,
    global: bool,
) -> Result<Vec<MatchSpan>, Throw> {
    let units_len: usize = text.chars().map(char::len_utf16).sum();
    let mut spans = Vec::new();
    let mut start = 0.0;
    loop {
        let m = regexp::regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)])?;
        if m.is_null() {
            break;
        }
        let Some(arr) = m.as_object() else { break };
        let match_start = ctx
            .heap
            .get(arr)
            .properties
            .get(1)
            .and_then(|v| v.as_smi())
            .map(i64::from)
            .unwrap_or(0) as usize;
        // Snapshot the element handles first: resolving their text below
        // needs `&mut ctx`, which cannot coexist with a live heap borrow.
        let (m0_h, group_handles): (Option<Handle<V12Str>>, Vec<Option<Handle<V12Str>>>) = {
            let arr_ref = ctx.heap.get(arr);
            (
                arr_ref.elements_array.get(0).and_then(|v| v.as_string()),
                (1..=9)
                    .map(|i| arr_ref.elements_array.get(i).and_then(|v| v.as_string()))
                    .collect(),
            )
        };
        let m0_len = m0_h.map(|h| ctx.string_text(h).len()).unwrap_or(0);
        let groups = group_handles
            .into_iter()
            .map(|g| g.map(|h| ctx.string_text(h)))
            .collect();
        // `index` is a UTF-16 unit index; convert span ends to byte offsets
        // (`m0_len` is the whole-match text's byte length, so `e` tracks the
        // same conversion `text` splicing needs).
        let s = utf16_byte_offset(text, match_start);
        let mut e = (s + m0_len).min(text.len());
        while !text.is_char_boundary(e) {
            e += 1;
        }
        spans.push((s, e, groups));
        // Zero-width guard (`lastIndex` counts UTF-16 units).
        let li = regexp::last_index(ctx.heap, re);
        if li <= start {
            regexp::set_last_index(ctx.heap, re, start + 1.0);
        }
        start = li;
        if start > units_len as f64 || !global {
            break;
        }
    }
    Ok(spans)
}

/// `String.prototype.match(regexp)` — the regexp `match` method.
///
/// ES 22.2.6.10 subset: with a global regexp, repeatedly `exec` until
/// exhaustion, returning the array of matched substrings (no groups, no
/// `index`/`input`). With a non-global regexp, delegates to
/// `RegExp.prototype.exec` and returns that result directly (`null` or a
/// match array).
pub fn string_match(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let handle = this_string(ctx, this, "String.prototype.match")?;
    let text = ctx.string_text(handle);
    let Some(re) = as_regexp(ctx.heap, args.first()) else {
        // Non-regexp argument: ToString and return a single-match array.
        let arg = args.first().map(|v| ctx.to_string(*v)).unwrap_or_default();
        return Ok(match_text_to_array(
            ctx,
            &text,
            text.find(&arg).map(|i| (i, i + arg.len())),
        ));
    };
    let (_, flags) = regexp::regexp_source_flags(ctx.heap, re);
    let text_h = ctx.heap.intern_text(&text);
    if flags.contains('g') {
        // Global: collect every match's whole text.
        let spans = collect_match_spans(ctx, re, text_h, &text, true)?;
        if spans.is_empty() {
            return Ok(JsValue::null());
        }
        let matches = spans
            .iter()
            .map(|&(s, e, _)| JsValue::string(ctx.heap.intern_text(&text[s..e])))
            .collect();
        let arr = ctx.alloc_obj(v12_heap::JsObject::array(matches));
        Ok(JsValue::object(arr))
    } else {
        regexp::regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)])
    }
}

/// Cap on the byte length of a `String.prototype.replace` result.
///
/// The spec allows strings up to 2^53−1 units, but this engine cannot
/// realize multi-gigabyte strings: building them means unbounded `String`
/// growth plus reallocation plus an interned duplicate. The conformance
/// suite (`staging/sm/String/replace-math.js`) deliberately expands 1 MiB
/// × a 64 KiB `"$1"`-repeat template toward 2^36 chars and accepts an
/// OOM-style catch, so abort with a `RangeError` (catchable from JS)
/// instead of stalling past the runner's hard-kill. 256 MiB sits far above
/// any legitimate conformance string (~1 MiB via `puff`) and far below the
/// test's 2^36 target.
const MAX_REPLACE_LEN: u64 = 1 << 28;

/// Byte length `expand_replacement` would produce, computed without
/// building the string (saturating, so huge templates cannot overflow).
fn expanded_len(template: &str, whole: &str, groups: &[&str]) -> u64 {
    let whole_len = whole.len() as u64;
    let mut len = 0u64;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            len = len.saturating_add(c.len_utf8() as u64);
            continue;
        }
        match chars.next() {
            Some('$') => len = len.saturating_add(1),
            Some('&') => len = len.saturating_add(whole_len),
            Some(d @ '0'..='9') => {
                let idx = d.to_digit(10).unwrap() as usize;
                let sub = if idx == 0 {
                    whole
                } else if idx <= groups.len() {
                    groups[idx - 1]
                } else {
                    ""
                };
                len = len.saturating_add(sub.len() as u64);
            }
            // `$` followed by anything else (or nothing) is literal.
            Some(other) => len = len.saturating_add(1 + other.len_utf8() as u64),
            None => len = len.saturating_add(1),
        }
    }
    len
}
///
/// ES 22.2.6.11 subset: global regexps replace every match; otherwise only
/// the first. The replacement is a string; `$&`, `$1`–`$9`, and `$$` are
/// expanded (no function replacements).
pub fn string_replace(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let handle = this_string(ctx, this, "String.prototype.replace")?;
    let text = ctx.string_text(handle);
    let Some(search) = args.first().copied() else {
        return Ok(JsValue::string(handle));
    };
    let replacement = args.get(1).map(|v| ctx.to_string(*v)).unwrap_or_default();
    // Non-regexp search: replace the first occurrence.
    let Some(re) = as_regexp(ctx.heap, Some(&search)) else {
        let needle = ctx.to_string(search);
        return replace_first_occurrence(ctx, &text, &needle, &replacement);
    };
    let (_, flags) = regexp::regexp_source_flags(ctx.heap, re);
    let global = flags.contains('g');
    let text_h = ctx.heap.intern_text(&text);
    let spans = collect_match_spans(ctx, re, text_h, &text, global)?;
    if spans.is_empty() {
        return Ok(JsValue::string(handle));
    }
    let mut out = String::new();
    let mut cursor = 0;
    for (s, e, groups) in spans {
        let gap = (s - cursor) as u64;
        let whole = &text[s..e];
        let group_refs = groups
            .iter()
            .map(|g| g.as_deref().unwrap_or(""))
            .collect::<Vec<_>>();
        // Project the post-splice length before allocating: a template of
        // N `$1`s over a 1 MiB capture reaches tens of GiB, which must
        // throw RangeError (catchable) rather than build + realloc + intern.
        let expansion = expanded_len(&replacement, whole, &group_refs);
        if (out.len() as u64)
            .saturating_add(gap)
            .saturating_add(expansion)
            > MAX_REPLACE_LEN
        {
            return Err(ctx.range_error("RangeError: replace result exceeds maximum string length"));
        }
        out.push_str(&text[cursor..s]);
        out.push_str(&expand_replacement(&replacement, whole, &group_refs));
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    Ok(JsValue::string(ctx.heap.intern_text(&out)))
}

/// The non-regexp `replace` fallback: substitute `replacement` (with `$&`/
/// `$1`–`$9`/`$$` expansion) for the first occurrence of `needle` in `text`.
fn replace_first_occurrence(
    ctx: &mut Ctx,
    text: &str,
    needle: &str,
    replacement: &str,
) -> Result<JsValue, Throw> {
    let out = if needle.is_empty() {
        if (replacement.len() as u64).saturating_add(text.len() as u64) > MAX_REPLACE_LEN {
            return Err(ctx.range_error("RangeError: replace result exceeds maximum string length"));
        }
        replacement.to_string() + text
    } else {
        match text.find(needle) {
            Some(i) => {
                let expansion = expanded_len(replacement, needle, &[]);
                if (text.len() as u64)
                    .saturating_sub(needle.len() as u64)
                    .saturating_add(expansion)
                    > MAX_REPLACE_LEN
                {
                    return Err(
                        ctx.range_error("RangeError: replace result exceeds maximum string length")
                    );
                }
                format!(
                    "{}{}{}",
                    &text[..i],
                    expand_replacement(replacement, needle, &[]),
                    &text[i + needle.len()..]
                )
            }
            None => text.to_string(),
        }
    };
    Ok(JsValue::string(ctx.heap.intern_text(&out)))
}

/// `String.prototype.search(regexp)` — the index of the first match, or -1.
pub fn string_search(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let handle = this_string(ctx, this, "String.prototype.search")?;
    let text = ctx.string_text(handle);
    let Some(search) = args.first().copied() else {
        return Ok(helpers::smi_or_f64(0));
    };
    let Some(re) = as_regexp(ctx.heap, Some(&search)) else {
        let needle = ctx.to_string(search);
        return Ok(helpers::smi_or_f64(
            text.find(&needle).map(|i| i as i64).unwrap_or(-1),
        ));
    };
    let text_h = ctx.heap.intern_text(&text);
    let m = regexp::regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)])?;
    if m.is_null() {
        return Ok(helpers::smi_or_f64(-1));
    }
    let idx = m
        .as_object()
        .and_then(|arr| ctx.heap.get(arr).properties.get(1))
        .and_then(|v| v.as_smi())
        .unwrap_or(0);
    Ok(helpers::smi_or_f64(i64::from(idx)))
}

/// `String.prototype.split(regexp, limit)` — split on regexp separators.
///
/// ES 22.2.6.17 subset: non-global regexps split on the first match (the
/// captured groups are omitted from the output); global regexps split on
/// every match. Empty segments are preserved.
pub fn string_split(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let handle = this_string(ctx, this, "String.prototype.split")?;
    let text = ctx.string_text(handle);
    let limit = args
        .get(1)
        .and_then(|v| v.as_smi())
        .map(i64::from)
        .unwrap_or(i64::MAX);
    let mut pieces: Vec<&str> = Vec::new();
    let Some(search) = args.first().copied() else {
        pieces.push(&text);
        return Ok(array_of_strings(ctx, pieces, limit));
    };
    // Non-regexp separator.
    let Some(re) = as_regexp(ctx.heap, Some(&search)) else {
        let sep = ctx.to_string(search);
        if sep.is_empty() {
            // Split into UTF-16 code units (no surrogate pairing).
            let chars: Vec<&str> = text.split("").filter(|s| !s.is_empty()).collect();
            return Ok(array_of_strings(ctx, chars, limit));
        }
        pieces = text.split(&sep).collect();
        return Ok(array_of_strings(ctx, pieces, limit));
    };
    let (source, flags) = regexp::regexp_source_flags(ctx.heap, re);
    // Spec (22.2.6.17): `split` treats the separator as global — when the
    // separator regexp lacks `g`, the spec creates a clone with `g` added
    // (the "Splitter"). Do the same so `exec` advances `lastIndex` across
    // occurrences; the original regexp is left untouched.
    let splitter = if flags.contains('g') {
        re
    } else {
        let source_h = ctx.heap.intern_text(&source);
        let flags_h = ctx.heap.intern_text(&format!("{flags}g"));
        let h = ctx.alloc_obj(v12_heap::JsObject::regexp(source_h, flags_h));
        regexp::link_regexp_proto(ctx, h);
        h
    };
    let text_h = ctx.heap.intern_text(&text);
    let spans = collect_match_spans(ctx, splitter, text_h, &text, true)?;
    let mut cursor = 0;
    for (s, e, _) in spans {
        pieces.push(&text[cursor..s]);
        cursor = e;
    }
    pieces.push(&text[cursor..]);
    Ok(array_of_strings(ctx, pieces, limit))
}

fn array_of_strings(ctx: &mut Ctx, strs: Vec<&str>, limit: i64) -> JsValue {
    let mut out = Vec::new();
    for (i, s) in strs.into_iter().enumerate() {
        if (i as i64) >= limit {
            break;
        }
        out.push(JsValue::string(ctx.heap.intern_text(s)));
    }
    let arr = ctx.alloc_obj(v12_heap::JsObject::array(out));
    JsValue::object(arr)
}

fn match_text_to_array(ctx: &mut Ctx, text: &str, found: Option<(usize, usize)>) -> JsValue {
    match found {
        Some((s, e)) => {
            let matched_h = ctx.heap.intern_text(&text[s..e]);
            let arr = ctx.alloc_obj(v12_heap::JsObject::array(vec![JsValue::string(matched_h)]));
            JsValue::object(arr)
        }
        None => JsValue::null(),
    }
}

/// Expands `$&`, `$1`–`$9`, and `$$` in a replacement string.
fn expand_replacement(template: &str, whole: &str, groups: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + whole.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('$') => out.push('$'),
            Some('&') => out.push_str(whole),
            Some(d @ '0'..='9') => {
                let idx = d.to_digit(10).unwrap() as usize;
                if idx == 0 {
                    out.push_str(whole);
                } else if idx <= groups.len() {
                    out.push_str(groups[idx - 1]);
                } else {
                    out.push_str("");
                }
            }
            Some(other) => {
                out.push('$');
                out.push(other);
            }
            None => out.push('$'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Plain string methods (string receivers are primitives routed through the
// `StringPrim` const method table; `this` is the primitive itself).
// ---------------------------------------------------------------------------

/// The `this` receiver's text (string primitives only; wrapper objects are
/// not modeled).
fn this_text(ctx: &mut Ctx, this: JsValue, method: &str) -> Result<String, Throw> {
    match this.as_string() {
        Some(h) => Ok(ctx.string_text(h)),
        None => Err(ctx.type_error(format!(
            "TypeError: String.prototype.{method} requires that 'this' be a String"
        ))),
    }
}

/// Integer coercion shared by the index arguments (NaN → 0, truncation).
fn to_int(ctx: &mut Ctx, v: Option<JsValue>) -> i64 {
    let Some(v) = v else { return 0 };
    if v.is_undefined() {
        return 0;
    }
    let n = ctx.to_number(v);
    if n.is_nan() { 0 } else { n.trunc() as i64 }
}

/// UTF-16 code units of the text — JS string indices are UTF-16 offsets.
fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// Finds `needle` in `haystack` at or after `from` (unit indices).
fn utf16_find(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(haystack.len()));
    }
    if from >= haystack.len() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .skip(from)
        .position(|w| w == needle)
}

fn arg_text(ctx: &mut Ctx, v: JsValue) -> String {
    ctx.to_string(v)
}

pub fn string_char_code_at(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "charCodeAt")?;
    let units = utf16(&text);
    let i = to_int(ctx, args.first().copied());
    match units.get(i as usize) {
        Some(&u) => Ok(helpers::js_number(f64::from(u))),
        None => Ok(JsValue::from_f64(f64::NAN)),
    }
}

pub fn string_code_point_at(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "codePointAt")?;
    let units = utf16(&text);
    let i = to_int(ctx, args.first().copied());
    if i < 0 || i as usize >= units.len() {
        return Ok(JsValue::undefined());
    }
    let u = units[i as usize];
    let cp = if (0xD800..0xDC00).contains(&u)
        && let Some(&low) = units.get(i as usize + 1)
        && (0xDC00..0xE000).contains(&low)
    {
        0x10000u32 + (u32::from(u) - 0xD800) * 0x400 + (u32::from(low) - 0xDC00)
    } else {
        u32::from(u)
    };
    Ok(helpers::js_number(f64::from(cp)))
}

pub fn string_at(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "at")?;
    let units = utf16(&text);
    let len = units.len() as i64;
    let i = match args.first().copied() {
        None => 0,
        Some(v) if v.is_undefined() => 0,
        Some(v) => {
            let n = ctx.to_number(v).trunc() as i64;
            if n < 0 { len + n } else { n }
        }
    };
    if i < 0 || i >= len {
        return Ok(JsValue::undefined());
    }
    let unit = units[i as usize];
    Ok(JsValue::string(ctx.heap.intern_text(
        &(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}')).to_string(),
    )))
}

pub fn string_index_of(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "indexOf")?;
    let search = match args.first().copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    let from = to_int(ctx, args.get(1).copied()).max(0) as usize;
    let result = utf16_find(&utf16(&text), &utf16(&search), from);
    Ok(helpers::js_number(
        result.map_or(-1.0, |i| f64::from(i as u32)),
    ))
}

pub fn string_last_index_of(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "lastIndexOf")?;
    let search = match args.first().copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    let units = utf16(&text);
    let needle = utf16(&search);
    // NaN fromIndex becomes +∞ per spec; we treat absent/NaN as end.
    let end = match args.get(1).copied() {
        None => units.len(),
        Some(v) if v.is_undefined() => units.len(),
        Some(v) => {
            let n = ctx.to_number(v);
            if n.is_nan() {
                units.len()
            } else {
                (n.trunc().max(0.0) as usize).min(units.len())
            }
        }
    };
    let result = if needle.is_empty() {
        Some(end)
    } else {
        units[..end]
            .windows(needle.len())
            .rposition(|w| w == needle)
    };
    Ok(helpers::js_number(
        result.map_or(-1.0, |i| f64::from(i as u32)),
    ))
}

pub fn string_includes(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "includes")?;
    let search = match args.first().copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    let from = to_int(ctx, args.get(1).copied()).max(0) as usize;
    Ok(JsValue::from_bool(
        utf16_find(&utf16(&text), &utf16(&search), from).is_some(),
    ))
}

pub fn string_starts_with(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "startsWith")?;
    let search = match args.first().copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    let from = to_int(ctx, args.get(1).copied()).max(0) as usize;
    let units = utf16(&text);
    let needle = utf16(&search);
    Ok(JsValue::from_bool(
        from <= units.len()
            && units.len() - from >= needle.len()
            && units[from..from + needle.len()] == needle[..],
    ))
}

pub fn string_ends_with(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "endsWith")?;
    let search = match args.first().copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    let units = utf16(&text);
    let needle = utf16(&search);
    let end = match args.get(1).copied() {
        None => units.len(),
        Some(v) if v.is_undefined() => units.len(),
        Some(v) => (ctx.to_number(v).trunc().max(0.0) as usize).min(units.len()),
    };
    Ok(JsValue::from_bool(
        end >= needle.len() && units[end - needle.len()..end] == needle[..],
    ))
}

pub fn string_concat(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut text = this_text(ctx, this, "concat")?;
    for &v in args {
        text.push_str(&arg_text(ctx, v));
    }
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

pub fn string_repeat(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "repeat")?;
    let n = match args.first().copied() {
        None => 0.0,
        Some(v) => ctx.to_number(v),
    };
    if n.is_nan() {
        return Ok(JsValue::string(ctx.heap.intern_text("")));
    }
    if n < 0.0 || n.is_infinite() {
        return Err(ctx.type_error("RangeError: Invalid count value"));
    }
    let n = n.trunc() as usize;
    Ok(JsValue::string(ctx.heap.intern_text(&text.repeat(n))))
}

pub fn string_pad_start(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "padStart")?;
    pad(ctx, text, args, true)
}

pub fn string_pad_end(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "padEnd")?;
    pad(ctx, text, args, false)
}

fn pad(ctx: &mut Ctx, text: String, args: &[JsValue], start: bool) -> Result<JsValue, Throw> {
    let target = match args.first().copied() {
        None => 0,
        Some(v) => ctx.to_number(v).clamp(0.0, f64::from(u32::MAX)) as usize,
    };
    let fill = match args.get(1).copied() {
        None => " ".to_string(),
        Some(v) if v.is_undefined() => " ".to_string(),
        Some(v) => arg_text(ctx, v),
    };
    let units = utf16(&text);
    if target <= units.len() {
        return Ok(JsValue::string(ctx.heap.intern_text(&text)));
    }
    let fill_units = utf16(&fill);
    let mut pad_units: Vec<u16> = Vec::new();
    if fill_units.is_empty() {
        return Ok(JsValue::string(ctx.heap.intern_text(&text)));
    }
    while pad_units.len() < target - units.len() {
        pad_units.extend_from_slice(&fill_units);
    }
    pad_units.truncate(target - units.len());
    let padded = if start {
        let mut p = pad_units;
        p.extend_from_slice(&units);
        p
    } else {
        let mut p = units;
        p.extend_from_slice(&pad_units);
        p
    };
    let result: String = String::from_utf16_lossy(&padded);
    Ok(JsValue::string(ctx.heap.intern_text(&result)))
}

/// ES `TrimString` whitespace set (includes NBSP, line separators, BOM).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub fn string_trim(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "trim")?;
    Ok(JsValue::string(
        ctx.heap.intern_text(text.trim_matches(is_js_whitespace)),
    ))
}

pub fn string_trim_start(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "trimStart")?;
    Ok(JsValue::string(
        ctx.heap
            .intern_text(text.trim_start_matches(is_js_whitespace)),
    ))
}

pub fn string_trim_end(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "trimEnd")?;
    Ok(JsValue::string(
        ctx.heap
            .intern_text(text.trim_end_matches(is_js_whitespace)),
    ))
}

pub fn string_to_lower_case(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "toLowerCase")?;
    Ok(JsValue::string(ctx.heap.intern_text(&text.to_lowercase())))
}

pub fn string_to_upper_case(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "toUpperCase")?;
    Ok(JsValue::string(ctx.heap.intern_text(&text.to_uppercase())))
}

pub fn string_substring(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "substring")?;
    let units = utf16(&text);
    let len = units.len() as i64;
    let mut start = to_int(ctx, args.first().copied()).clamp(0, len);
    let mut end = match args.get(1).copied() {
        None => len,
        Some(v) if v.is_undefined() => len,
        Some(v) => to_int(ctx, Some(v)).clamp(0, len),
    };
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    let result = String::from_utf16_lossy(&units[start as usize..end as usize]);
    Ok(JsValue::string(ctx.heap.intern_text(&result)))
}

pub fn string_substr(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "substr")?;
    let units = utf16(&text);
    let len = units.len() as i64;
    let start = match args.first().copied() {
        None => 0,
        Some(v) => {
            let n = to_int(ctx, Some(v));
            if n < 0 { (len + n).max(0) } else { n.min(len) }
        }
    };
    let length = match args.get(1).copied() {
        None => len - start,
        Some(v) if v.is_undefined() => len - start,
        Some(v) => to_int(ctx, Some(v)).max(0).min(len - start),
    };
    let result = String::from_utf16_lossy(&units[start as usize..(start + length) as usize]);
    Ok(JsValue::string(ctx.heap.intern_text(&result)))
}

pub fn string_to_string(
    _ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(this)
}

pub fn string_value_of(_ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(this)
}

/// Approximate `localeCompare`: UTF-16 code-unit lexicographic order.
pub fn string_locale_compare(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let a = this_text(ctx, this, "localeCompare")?;
    let b = match args.first().copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    let (au, bu) = (utf16(&a), utf16(&b));
    let ord = match au.cmp(&bu) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(helpers::js_number(f64::from(ord)))
}

pub fn string_from_char_code(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let mut units: Vec<u16> = Vec::with_capacity(args.len());
    for &v in args {
        let n = ctx.to_number(v);
        units.push(if n.is_nan() { 0 } else { n as u16 });
    }
    Ok(JsValue::string(
        ctx.heap.intern_text(&String::from_utf16_lossy(&units)),
    ))
}

pub fn string_from_code_point(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let mut text = String::new();
    for &v in args {
        let n = ctx.to_number(v);
        if !n.is_finite() || n < 0.0 || n > 0x10FFFFu32 as f64 || n.fract() != 0.0 {
            return Err(ctx.type_error("RangeError: Invalid code point"));
        }
        let Some(cp) = char::from_u32(n as u32) else {
            return Err(ctx.type_error("RangeError: Invalid code point"));
        };
        text.push(cp);
    }
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `String.prototype.replaceAll` for string search values (regex search
/// values remain on the registry's regex path).
pub fn string_replace_all(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = this_text(ctx, this, "replaceAll")?;
    let search = match args.first().copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    if search.is_empty() {
        let replacement = match args.get(1).copied() {
            Some(v) => arg_text(ctx, v),
            None => "undefined".to_string(),
        };
        let mut out = replacement.clone();
        out.push_str(&text);
        return Ok(JsValue::string(ctx.heap.intern_text(&out)));
    }
    let replacement = match args.get(1).copied() {
        Some(v) => arg_text(ctx, v),
        None => "undefined".to_string(),
    };
    Ok(JsValue::string(
        ctx.heap.intern_text(&text.replace(&search, &replacement)),
    ))
}
