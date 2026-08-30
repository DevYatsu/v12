//! String built-ins.

use v12_heap::{Heap, JsValue};
use v12_native::Throw;

use super::{helpers, regexp};

/// `String.prototype.charAt(index)` – returns a single-character string.
pub fn string_char_at(
    heap: &mut Heap,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let handle = this
        .as_string()
        .ok_or_else(|| Throw::type_error(heap, "String.prototype.charAt called on non-string"))?;
    let index = args.first().and_then(to_index).unwrap_or(0);
    let units = string_units(heap, handle);
    if let Some(&unit) = units.get(index as usize) {
        let h = heap.intern_string(v12_heap::V12Str::utf16(vec![unit]));
        Ok(JsValue::string(h))
    } else {
        let h = heap.intern_string(v12_heap::V12Str::latin1(Vec::new()));
        Ok(JsValue::string(h))
    }
}

/// `String.prototype.slice(start, end)` – returns a sliced view.
pub fn string_slice(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let handle = this
        .as_string()
        .ok_or_else(|| Throw::type_error(heap, "String.prototype.slice called on non-string"))?;
    let len = heap.get(handle).len() as i64;
    let start = args.first().and_then(to_integer).unwrap_or(0);
    let end = args.get(1).and_then(to_integer).unwrap_or(len);
    let from = clamp_index(start, len) as u32;
    let to = clamp_index(end, len) as u32;
    let (from, to) = if from > to { (to, to) } else { (from, to) };
    let slice_len = to.saturating_sub(from);
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

fn string_units(heap: &mut Heap, handle: v12_heap::Handle<v12_heap::V12Str>) -> Vec<u16> {
    heap.flatten(handle);
    match &heap.get(handle).storage {
        v12_heap::StrStorage::Latin1(bytes) => bytes.iter().map(|&b| u16::from(b)).collect(),
        v12_heap::StrStorage::Utf16(units) => units.clone(),
        _ => Vec::new(),
    }
}

/// `String.prototype.match(regexp)` — the regexp `match` method.
///
/// ES 22.2.6.10 subset: with a global regexp, repeatedly `exec` until
/// exhaustion, returning the array of matched substrings (no groups, no
/// `index`/`input`). With a non-global regexp, delegates to
/// `RegExp.prototype.exec` and returns that result directly (`null` or a
/// match array).
pub fn string_match(
    heap: &mut Heap,
    cache: &regexp::RegexCache,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let handle = this
        .as_string()
        .ok_or_else(|| Throw::type_error(heap, "String.prototype.match called on non-string"))?;
    let text = helpers::string_text(heap, handle);
    let Some(re) = args.first().and_then(|v| v.as_object()) else {
        // Non-regexp argument: ToString and return a single-match array.
        let arg = args.first().map(|v| helpers::value_text(heap, *v)).unwrap_or_default();
        return Ok(match_text_to_array(heap, &text, text.find(&arg).map(|i| (i, i + arg.len()))));
    };
    if heap.get(re).kind != v12_heap::Kind::RegExp {
        let arg = helpers::value_text(heap, *args.first().unwrap());
        return Ok(match_text_to_array(heap, &text, text.find(&arg).map(|i| (i, i + arg.len()))));
    }
    let (_, flags) = regexp::regexp_source_flags(heap, re);
    let text_h = heap.intern_text(&text);
    if flags.contains('g') {
        // Global: collect every match's whole text.
        let mut matches = Vec::new();
        let mut start = 0.0;
        loop {
            let m = regexp::regexp_exec(heap, cache, JsValue::object(re), &[JsValue::string(text_h)])?;
            if m.is_null() {
                break;
            }
            // Extract `m[0]`.
            if let Some(arr) = m.as_object() {
                if let Some(v) = heap.get(arr).elements_array.get(0) {
                    matches.push(v);
                }
            }
            // Guard against infinite loop on zero-width matches.
            let li = regexp::last_index(heap, re);
            if li <= start {
                regexp::set_last_index(heap, re, start + 1.0);
            }
            start = li;
            if start > text.len() as f64 {
                break;
            }
        }
        if matches.is_empty() {
            return Ok(JsValue::null());
        }
        let arr = helpers::alloc_obj(heap, v12_heap::JsObject::array(matches));
        Ok(JsValue::object(arr))
    } else {
        regexp::regexp_exec(heap, cache, JsValue::object(re), &[JsValue::string(text_h)])
    }
}

/// `String.prototype.replace(regexp, replacement)` — regexp replace.
///
/// ES 22.2.6.11 subset: global regexps replace every match; otherwise only
/// the first. The replacement is a string; `$&`, `$1`–`$9`, and `$$` are
/// expanded (no function replacements).
pub fn string_replace(
    heap: &mut Heap,
    cache: &regexp::RegexCache,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let handle = this
        .as_string()
        .ok_or_else(|| Throw::type_error(heap, "String.prototype.replace called on non-string"))?;
    let text = helpers::string_text(heap, handle);
    let Some(search) = args.first().copied() else {
        return Ok(JsValue::string(handle));
    };
    let replacement = args
        .get(1)
        .map(|v| helpers::value_text(heap, *v))
        .unwrap_or_default();
    // Non-regexp search: replace the first occurrence.
    let Some(re) = search.as_object() else {
        let needle = helpers::value_text(heap, search);
        if needle.is_empty() {
            let out = replacement.clone() + &text;
            return Ok(JsValue::string(heap.intern_text(&out)));
        }
        let out = match text.find(&needle) {
            Some(i) => format!(
                "{}{}{}",
                &text[..i],
                expand_replacement(&replacement, &needle, &[]),
                &text[i + needle.len()..]
            ),
            None => text.clone(),
        };
        return Ok(JsValue::string(heap.intern_text(&out)));
    };
    if heap.get(re).kind != v12_heap::Kind::RegExp {
        let needle = helpers::value_text(heap, search);
        if needle.is_empty() {
            let out = replacement.clone() + &text;
            return Ok(JsValue::string(heap.intern_text(&out)));
        }
        let out = match text.find(&needle) {
            Some(i) => format!(
                "{}{}{}",
                &text[..i],
                expand_replacement(&replacement, &needle, &[]),
                &text[i + needle.len()..]
            ),
            None => text.clone(),
        };
        return Ok(JsValue::string(heap.intern_text(&out)));
    }
    let (_, flags) = regexp::regexp_source_flags(heap, re);
    let global = flags.contains('g');
    let text_h = heap.intern_text(&text);
    // Collect all match spans (byte offsets in `text`).
    let mut spans: Vec<(usize, usize, Vec<Option<String>>)> = Vec::new();
    let mut start = 0.0;
    loop {
        let m = regexp::regexp_exec(heap, cache, JsValue::object(re), &[JsValue::string(text_h)])?;
        if m.is_null() {
            break;
        }
        let Some(arr) = m.as_object() else { break };
        let match_start = heap.get(arr).properties.get(1).and_then(|v| v.as_smi()).map(i64::from).unwrap_or(0) as usize;
        let m0_len = heap.get(arr).elements_array.get(0).and_then(|v| v.as_string()).map(|h| helpers::string_text(heap, h).len()).unwrap_or(0);
        let mut groups = Vec::new();
        for i in 1..=9 {
            let g = heap.get(arr).elements_array.get(i).and_then(|v| v.as_string()).map(|h| helpers::string_text(heap, h));
            groups.push(g);
        }
        spans.push((match_start, match_start + m0_len, groups));
        // Zero-width guard.
        let li = regexp::last_index(heap, re);
        if li <= start {
            regexp::set_last_index(heap, re, start + 1.0);
        }
        start = li;
        if start > text.len() as f64 {
            break;
        }
        if !global {
            break;
        }
    }
    if spans.is_empty() {
        return Ok(JsValue::string(handle));
    }
    let mut out = String::new();
    let mut cursor = 0;
    for (s, e, groups) in spans {
        out.push_str(&text[cursor..s]);
        let whole = &text[s..e];
        out.push_str(&expand_replacement(&replacement, whole, &groups.iter().map(|g| g.as_deref().unwrap_or("")).collect::<Vec<_>>()));
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    Ok(JsValue::string(heap.intern_text(&out)))
}

/// `String.prototype.search(regexp)` — the index of the first match, or -1.
pub fn string_search(
    heap: &mut Heap,
    cache: &regexp::RegexCache,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let handle = this
        .as_string()
        .ok_or_else(|| Throw::type_error(heap, "String.prototype.search called on non-string"))?;
    let text = helpers::string_text(heap, handle);
    let Some(search) = args.first().copied() else {
        return Ok(helpers::smi_or_f64(0));
    };
    let Some(re) = search.as_object() else {
        let needle = helpers::value_text(heap, search);
        return Ok(helpers::smi_or_f64(text.find(&needle).map(|i| i as i64).unwrap_or(-1)));
    };
    if heap.get(re).kind != v12_heap::Kind::RegExp {
        let needle = helpers::value_text(heap, search);
        return Ok(helpers::smi_or_f64(text.find(&needle).map(|i| i as i64).unwrap_or(-1)));
    }
    let text_h = heap.intern_text(&text);
    let m = regexp::regexp_exec(heap, cache, JsValue::object(re), &[JsValue::string(text_h)])?;
    if m.is_null() {
        return Ok(helpers::smi_or_f64(-1));
    }
    let idx = m
        .as_object()
        .and_then(|arr| heap.get(arr).properties.get(1))
        .and_then(|v| v.as_smi())
        .unwrap_or(0);
    Ok(helpers::smi_or_f64(i64::from(idx)))
}

/// `String.prototype.split(regexp, limit)` — split on regexp separators.
///
/// ES 22.2.6.17 subset: non-global regexps split on the first match (the
/// captured groups are omitted from the output); global regexps split on
/// every match. Empty segments are preserved.
pub fn string_split(
    heap: &mut Heap,
    cache: &regexp::RegexCache,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let handle = this
        .as_string()
        .ok_or_else(|| Throw::type_error(heap, "String.prototype.split called on non-string"))?;
    let text = helpers::string_text(heap, handle);
    let limit = args
        .get(1)
        .and_then(|v| v.as_smi())
        .map(i64::from)
        .unwrap_or(i64::MAX);
    let mut pieces: Vec<&str> = Vec::new();
    let Some(search) = args.first().copied() else {
        pieces.push(&text);
        return Ok(array_of_strings(heap, pieces, limit));
    };
    // Non-regexp separator.
    let is_regexp = search
        .as_object()
        .is_some_and(|re| heap.get(re).kind == v12_heap::Kind::RegExp);
    if !is_regexp {
        let sep = helpers::value_text(heap, search);
        if sep.is_empty() {
            // Split into UTF-16 code units (no surrogate pairing).
            let chars: Vec<&str> = text.split("").filter(|s| !s.is_empty()).collect();
            return Ok(array_of_strings(heap, chars, limit));
        }
        pieces = text.split(&sep).collect();
        return Ok(array_of_strings(heap, pieces, limit));
    }
    let re = search.as_object().unwrap();
    let (source, flags) = regexp::regexp_source_flags(heap, re);
    // Spec (22.2.6.17): `split` treats the separator as global — when the
    // separator regexp lacks `g`, the spec creates a clone with `g` added
    // (the "Splitter"). Do the same so `exec` advances `lastIndex` across
    // occurrences; the original regexp is left untouched.
    let splitter = if flags.contains('g') {
        re
    } else {
        let source_h = heap.intern_text(&source);
        let flags_h = heap.intern_text(&format!("{flags}g"));
        helpers::alloc_obj(heap, v12_heap::JsObject::regexp(source_h, flags_h))
    };
    let text_h = heap.intern_text(&text);
    // Collect separator spans.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0.0;
    loop {
        let m = regexp::regexp_exec(heap, cache, JsValue::object(splitter), &[JsValue::string(text_h)])?;
        if m.is_null() {
            break;
        }
        let Some(arr) = m.as_object() else { break };
        let s = heap.get(arr).properties.get(1).and_then(|v| v.as_smi()).map(i64::from).unwrap_or(0) as usize;
        let len = heap.get(arr).elements_array.get(0).and_then(|v| v.as_string()).map(|h| helpers::string_text(heap, h).len()).unwrap_or(0);
        spans.push((s, s + len));
        let li = regexp::last_index(heap, splitter);
        if li <= start {
            regexp::set_last_index(heap, splitter, start + 1.0);
        }
        start = li;
        if start > text.len() as f64 {
            break;
        }
    }
    let mut cursor = 0;
    for (s, e) in spans {
        pieces.push(&text[cursor..s]);
        cursor = e;
    }
    pieces.push(&text[cursor..]);
    Ok(array_of_strings(heap, pieces, limit))
}

fn array_of_strings<'a>(heap: &mut Heap, strs: Vec<&'a str>, limit: i64) -> JsValue {
    let mut out = Vec::new();
    for (i, s) in strs.into_iter().enumerate() {
        if (i as i64) >= limit {
            break;
        }
        out.push(JsValue::string(heap.intern_text(s)));
    }
    let arr = helpers::alloc_obj(heap, v12_heap::JsObject::array(out));
    JsValue::object(arr)
}

fn match_text_to_array(heap: &mut Heap, text: &str, found: Option<(usize, usize)>) -> JsValue {
    match found {
        Some((s, e)) => {
            let matched_h = heap.intern_text(&text[s..e]);
            let arr = helpers::alloc_obj(heap, v12_heap::JsObject::array(vec![JsValue::string(matched_h)]));
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

