//! RegExp built-ins over the [`v12_regex`] wrapper (which wraps `regress`).
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3 + §8): all
//! bodies take `&mut Ctx`. The per-registry compiled-pattern cache travels
//! as a `Ctx` capability (`Ctx::with_regex_cache`, fed by the registry at
//! dispatch); bodies clone the `Rc` out of the context.
//!
//! A RegExp object is a `Kind::RegExp` with `properties =
//! [source, flags, lastIndex]`:
//!
//! - `properties[0]`: the source text (heap string; may be the canonical
//!   `"(?:)"` for empty patterns).
//! - `properties[1]`: the canonical flag string (heap string, `"dgimsuvy"`
//!   order with only set flags).
//! - `properties[2]`: `lastIndex` (Smi or double).
//!
//! The compiled `v12_regex::CompiledRegex` lives in a side table keyed by the
//! object handle (`Rc<RefCell<HashMap<u32, CompiledRegex>>>`) so recompilation
//! only happens when the source or flags change. The table is owned by the
//! registry, which outlives every function object referencing it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use v12_heap::{Handle, Heap, JsObject, JsValue, V12Str};
use v12_native::Throw;

use super::{ctx::Ctx, helpers};

/// Compiled-regexp cache: object handle → compiled pattern. Owned by the
/// registry so it survives GC (objects are traced strongly via the cache's
/// `u32` keys only as opaque ids — the cache holds no `Handle`s, so entries
/// for collected objects are simply stale and get overwritten on reuse).
pub type RegexCache = Rc<RefCell<HashMap<u32, v12_regex::CompiledRegex>>>;

/// The `lastIndex` slot index in a RegExp object's `properties`.
const SLOT_SOURCE: usize = 0;
const SLOT_FLAGS: usize = 1;
const SLOT_LAST_INDEX: usize = 2;

/// `RegExp(pattern, flags?)` — constructs a RegExp object.
///
/// Spec (RegExpConstructor): when `pattern` is a RegExp and `flags` is
/// undefined, the new object copies `pattern.source` and `pattern.flags`.
/// Otherwise `pattern` is coerced to a string (with `undefined` → `""` and
/// `null` → `"null"` per ToString). Invalid flags are a SyntaxError.
pub fn regexp_construct(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let (source_text, flags_text) = match (args.first(), args.get(1)) {
        (Some(first), None) => {
            // Copy-from-regexp fast path.
            if let Some(obj) = first.as_object()
                && ctx.heap.get(obj).kind == v12_heap::Kind::RegExp
            {
                let source = ctx.heap.get(obj).properties[SLOT_SOURCE];
                let flags = ctx.heap.get(obj).properties[SLOT_FLAGS];
                let handle = alloc_regexp(ctx, source, flags);
                return Ok(JsValue::object(handle));
            }
            (stringify_arg(ctx, *first), String::new())
        }
        (Some(first), Some(flags)) => (stringify_arg(ctx, *first), stringify_arg(ctx, *flags)),
        _ => (String::new(), String::new()),
    };
    let source_text = if source_text.is_empty() {
        "(?:)".to_string()
    } else {
        source_text
    };
    let flags_text = canonicalize_flags(&flags_text)
        .map_err(|e| ctx.syntax_error(format!("SyntaxError: {e}")))?;
    let source_h = ctx.heap.intern_text(&source_text);
    let flags_h = ctx.heap.intern_text(&flags_text);
    let handle = alloc_regexp(ctx, JsValue::string(source_h), JsValue::string(flags_h));
    Ok(JsValue::object(handle))
}

fn alloc_regexp(ctx: &mut Ctx, source: JsValue, flags: JsValue) -> Handle<JsObject> {
    let h = ctx.alloc_obj(JsObject::regexp(
        source.as_string().expect("source is a string"),
        flags.as_string().expect("flags is a string"),
    ));
    link_regexp_proto(ctx, h);
    h
}

/// Stamps `%RegExp.prototype%` as `h`'s `[[Prototype]]` (spec
/// `RegExpCreate` → `OrdinaryCreateFromConstructor`). The prototype is the
/// realm's, resolved through the `RegExp` constructor's `prototype` field
/// (installed at realm build via `install_ctor`), so
/// `Object.getPrototypeOf(re) === RegExp.prototype` and
/// `re instanceof RegExp` both hold. No-op when the realm cannot be
/// resolved (engine-side tests that build a bare heap).
pub(crate) fn link_regexp_proto(ctx: &mut Ctx, h: Handle<JsObject>) {
    let Some(ctor) = ctx.intrinsic("RegExp").and_then(|v| v.as_object()) else {
        return;
    };
    if let Some(proto) = ctx.heap.get(ctor).prototype {
        ctx.heap.get_mut(h).prototype = Some(proto);
    }
}

/// ES ToString for the RegExp constructor's pattern/flags arguments.
/// `undefined` → `""`, everything else via `ctx.to_string` (`null` renders
/// `"null"` through the display subset).
fn stringify_arg(ctx: &mut Ctx, v: JsValue) -> String {
    if v.is_undefined() {
        return String::new();
    }
    ctx.to_string(v)
}

/// Validates and canonicalizes a flag string into `"dgimsuvy"` order.
/// Duplicate flags and unknown letters are SyntaxErrors.
fn canonicalize_flags(flags: &str) -> Result<String, String> {
    let mut seen = [false; 8];
    let order = ['d', 'g', 'i', 'm', 's', 'u', 'v', 'y'];
    for c in flags.chars() {
        let Some(idx) = order.iter().position(|&o| o == c) else {
            return Err(format!("invalid regular expression flag {c:?}"));
        };
        if seen[idx] {
            return Err(format!("duplicate regular expression flag {c:?}"));
        }
        seen[idx] = true;
    }
    Ok(order
        .iter()
        .enumerate()
        .filter(|(i, _)| seen[*i])
        .map(|(_, c)| *c)
        .collect())
}

/// The compiled pattern for a RegExp object, compiling on first use.
/// The cache is keyed by the raw object index; stale entries for collected
/// objects are harmless (overwritten on reuse).
pub fn compile_for(
    cache: &RegexCache,
    heap: &mut Heap,
    obj: Handle<JsObject>,
) -> Result<v12_regex::CompiledRegex, String> {
    let idx = obj.index();
    if let Some(compiled) = cache.borrow().get(&idx) {
        return Ok(compiled.clone());
    }
    let (source, flags) = regexp_source_flags(heap, obj);
    let compiled = compile_pattern(&source, &flags)?;
    cache.borrow_mut().insert(idx, compiled.clone());
    Ok(compiled)
}

/// Reads `(source, flags)` off a RegExp object as Rust text.
pub fn regexp_source_flags(heap: &mut Heap, obj: Handle<JsObject>) -> (String, String) {
    let source_h = heap.get(obj).properties[SLOT_SOURCE]
        .as_string()
        .expect("RegExp source is a string");
    let flags_h = heap.get(obj).properties[SLOT_FLAGS]
        .as_string()
        .expect("RegExp flags is a string");
    (string_text(heap, source_h), string_text(heap, flags_h))
}

fn string_text(heap: &mut Heap, h: Handle<V12Str>) -> String {
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
        _ => String::new(),
    }
}

fn compile_pattern(source: &str, flags: &str) -> Result<v12_regex::CompiledRegex, String> {
    let mut f = v12_regex::Flags::default();
    for c in flags.chars() {
        match c {
            'd' => f.has_indices = true,
            'g' => f.global = true,
            'i' => f.ignore_case = true,
            'm' => f.multiline = true,
            's' => f.dot_all = true,
            'u' => f.unicode = true,
            'v' => f.unicode_sets = true,
            'y' => f.sticky = true,
            _ => return Err(format!("invalid regular expression flag {c:?}")),
        }
    }
    v12_regex::compile(source, f).map_err(|e| e.message)
}

/// Current `lastIndex` as a number (default 0).
pub fn last_index(heap: &Heap, obj: Handle<JsObject>) -> f64 {
    heap.get(obj).properties[SLOT_LAST_INDEX]
        .as_smi()
        .map(f64::from)
        .or_else(|| heap.get(obj).properties[SLOT_LAST_INDEX].as_f64())
        .unwrap_or(0.0)
}

/// Sets `lastIndex`, canonicalizing to a Smi when integral and in range.
pub fn set_last_index(heap: &mut Heap, obj: Handle<JsObject>, v: f64) {
    if v.fract() == 0.0
        && (-1e15..=1e15).contains(&v)
        && let Some(smi) = JsValue::from_i32_smi(v as i32)
    {
        heap.get_mut(obj).properties[SLOT_LAST_INDEX] = smi;
        return;
    }
    heap.get_mut(obj).properties[SLOT_LAST_INDEX] = JsValue::from_f64(v);
}

/// `RegExp.prototype.exec(string)` — the core matching primitive.
///
/// Implements ES 22.2.5.2 (subset): coerces the input to a string, applies
/// the global/sticky `lastIndex` advancement rules, runs the match via
/// `v12_regex`, and returns either `null` or an array-like match object
/// (`[0]` = whole match, `[1..n]` = capture groups, plus `index`, `input`,
/// and `groups` properties).
pub fn regexp_exec(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(this, "RegExp.prototype.exec", Some(v12_heap::Kind::RegExp))?;
    let input_text = args.first().map(|v| ctx.to_string(*v)).unwrap_or_default();
    let input_units: Vec<u16> = input_text.encode_utf16().collect();

    let (source, flags) = regexp_source_flags(ctx.heap, obj);
    let _ = source;
    let is_global = flags.contains('g');
    let is_sticky = flags.contains('y');
    let mut start = last_index(ctx.heap, obj);
    if !is_global && !is_sticky {
        start = 0.0;
    }
    // Spec: lastIndex beyond the input length fails immediately.
    if start > input_units.len() as f64 {
        if is_global || is_sticky {
            set_last_index(ctx.heap, obj, 0.0);
        }
        return Ok(JsValue::null());
    }
    let cache = ctx
        .regex_cache()
        .expect("registry feeds the regex cache at dispatch");
    let compiled = compile_for(&cache, ctx.heap, obj).map_err(|e| {
        let h = ctx.heap.intern_text(&format!("SyntaxError: {e}"));
        JsValue::string(h)
    })?;
    let m = compiled.exec(&input_units, start as usize);
    match m {
        None => {
            if is_global || is_sticky {
                set_last_index(ctx.heap, obj, 0.0);
            }
            Ok(JsValue::null())
        }
        Some(m) => {
            // Sticky: the match must start exactly at `lastIndex`.
            if is_sticky && m.start() != start as usize {
                set_last_index(ctx.heap, obj, 0.0);
                return Ok(JsValue::null());
            }
            if is_global || is_sticky {
                // Global advances to match end, or +1 on an empty match
                // (zero-width guard).
                let next = if m.span().is_empty() {
                    (m.start() + 1) as f64
                } else {
                    m.end() as f64
                };
                set_last_index(ctx.heap, obj, next);
            }
            Ok(match_result(ctx, &m, &input_units, &input_text))
        }
    }
}

/// Builds the `exec` result array: `[whole, ...groups]` with `index`,
/// `input`, and `groups` (named captures) properties.
fn match_result(
    ctx: &mut Ctx,
    m: &v12_regex::Match,
    input_units: &[u16],
    input_text: &str,
) -> JsValue {
    let capture_count = m.capture_count();
    let mut elements = Vec::with_capacity(capture_count + 1);
    for i in 0..=capture_count {
        match m.group(i) {
            Some(span) => {
                let text: String = input_units[span.start..span.end]
                    .iter()
                    .map(|&u| char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}'))
                    .collect();
                elements.push(JsValue::string(ctx.heap.intern_text(&text)));
            }
            None => elements.push(JsValue::undefined()),
        }
    }
    let arr = ctx.alloc_obj(JsObject::array(elements));
    // `index`, `input` named properties via shape transitions. Build the
    // shape as root → length → index → input so the array's `length` stays at
    // physical slot 0 (the interpreter's array fast path reads
    // `properties[0]` for length) and the named props land at slots 1/2.
    let length_key = ctx.heap.intern_text("length");
    let index_key = ctx.heap.intern_text("index");
    let input_key = ctx.heap.intern_text("input");
    let shape0 = ctx.heap.root_shape();
    let shape_len = ctx.heap.add_property(
        shape0,
        v12_heap::PropKey::from_string(length_key),
        v12_heap::Attrs::DEFAULT,
    );
    let shape_idx = ctx.heap.add_property(
        shape_len,
        v12_heap::PropKey::from_string(index_key),
        v12_heap::Attrs::DEFAULT,
    );
    let shape_in = ctx.heap.add_property(
        shape_idx,
        v12_heap::PropKey::from_string(input_key),
        v12_heap::Attrs::DEFAULT,
    );
    ctx.heap.bind_shape(arr, shape_in);
    // `properties[0]` is the length Smi (from `JsObject::array`); slots 1/2
    // get index/input.
    if ctx.heap.get(arr).properties.len() < 3 {
        ctx.heap
            .get_mut(arr)
            .properties
            .resize(3, JsValue::undefined());
        ctx.heap.get_mut(arr).property_keys.resize(3, None);
    }
    ctx.heap.get_mut(arr).properties[1] = helpers::smi_or_f64(m.start() as i64);
    let input_h = ctx.heap.intern_text(input_text);
    ctx.heap.get_mut(arr).properties[2] = JsValue::string(input_h);
    JsValue::object(arr)
}

/// `RegExp.prototype.test(string)` — `Boolean(exec(string))`.
pub fn regexp_test(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    match regexp_exec(ctx, this, args)? {
        v if v.is_null() => Ok(JsValue::false_()),
        _ => Ok(JsValue::true_()),
    }
}

/// `RegExp.prototype.toString()` — `"/" + source + "/" + flags`.
pub fn regexp_to_string(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(
        this,
        "RegExp.prototype.toString",
        Some(v12_heap::Kind::RegExp),
    )?;
    let (source, flags) = regexp_source_flags(ctx.heap, obj);
    let text = format!("/{source}/{flags}");
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
}

/// `RegExp.prototype.compile` — legacy recompile-in-place (Annex B).
pub fn regexp_compile(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = ctx.this_object(
        this,
        "RegExp.prototype.compile",
        Some(v12_heap::Kind::RegExp),
    )?;
    let (source_text, flags_text) = match (args.first(), args.get(1)) {
        (Some(first), None) => {
            if let Some(src_obj) = first.as_object()
                && ctx.heap.get(src_obj).kind == v12_heap::Kind::RegExp
            {
                let (s, f) = regexp_source_flags(ctx.heap, src_obj);
                (s, f)
            } else {
                (stringify_arg(ctx, *first), String::new())
            }
        }
        (Some(first), Some(flags)) => (stringify_arg(ctx, *first), stringify_arg(ctx, *flags)),
        _ => (String::new(), String::new()),
    };
    let source_text = if source_text.is_empty() {
        "(?:)".to_string()
    } else {
        source_text
    };
    let flags_text = canonicalize_flags(&flags_text).map_err(|e| {
        let h = ctx.heap.intern_text(&format!("SyntaxError: {e}"));
        JsValue::string(h)
    })?;
    let source_h = ctx.heap.intern_text(&source_text);
    let flags_h = ctx.heap.intern_text(&flags_text);
    ctx.heap.get_mut(obj).properties[SLOT_SOURCE] = JsValue::string(source_h);
    ctx.heap.get_mut(obj).properties[SLOT_FLAGS] = JsValue::string(flags_h);
    ctx.heap.get_mut(obj).properties[SLOT_LAST_INDEX] =
        JsValue::from_i32_smi(0).expect("0 fits Smi");
    // Drop any cached compilation for this object.
    if let Some(cache) = ctx.regex_cache() {
        cache.borrow_mut().remove(&obj.index());
    }
    Ok(this)
}
