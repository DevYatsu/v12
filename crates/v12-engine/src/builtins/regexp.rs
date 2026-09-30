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

/// The `this` value of a `%RegExp.prototype%` accessor, resolved per spec
/// 22.2.6.x steps 2-3.
///
/// Every one of those accessors has the same two-step guard: a `this` that is
/// not an object is a `TypeError`, and so is an object that does not carry the
/// RegExp internal slots — *except* `%RegExp.prototype%` itself, which the spec
/// admits by identity and which each getter then answers differently (`"(?:)"`
/// for `source`, `""` for `flags`, `undefined` for a single-flag getter).
/// Collapsing that shared guard here keeps the ten getters one-liners and keeps
/// the one place that has to change if the brand check ever widens.
enum RegExpThis {
    /// A real RegExp exotic object (`Kind::RegExp`).
    Instance(Handle<JsObject>),
    /// `%RegExp.prototype%`: an ordinary object per 10.4.1, not a RegExp.
    Prototype,
}

/// Resolves `this` for a `RegExp.prototype` accessor, or throws the
/// `TypeError` that step 2/3 of the spec getter prescribes.
fn regexp_this(ctx: &mut Ctx, this: JsValue, method: &str) -> Result<RegExpThis, Throw> {
    let Some(obj) = this.as_object() else {
        return Err(ctx.type_error(format!("{method} called on non-object")));
    };
    if ctx.heap.get(obj).kind == v12_heap::Kind::RegExp {
        return Ok(RegExpThis::Instance(obj));
    }
    // SameValue(R, %RegExp.prototype%) — resolved through the realm's `RegExp`
    // constructor `prototype` field, the same source `link_regexp_proto` uses.
    let proto = ctx
        .intrinsic("RegExp")
        .and_then(|v| v.as_object())
        .and_then(|ctor| ctx.heap.get(ctor).prototype);
    if proto == Some(obj) {
        return Ok(RegExpThis::Prototype);
    }
    Err(ctx.type_error(format!("{method} called on non-RegExp object")))
}

/// The internal slots of a real RegExp receiver, as `(source, flags)` string
/// values straight out of `properties` — no text materialization, since both
/// are already interned heap strings.
fn regexp_slots(heap: &Heap, obj: Handle<JsObject>) -> (JsValue, JsValue) {
    (
        heap.get(obj).properties[SLOT_SOURCE],
        heap.get(obj).properties[SLOT_FLAGS],
    )
}

/// `get RegExp.prototype.source` (ES 22.2.6.1). Returns the receiver's
/// `[[OriginalSource]]` verbatim — escaping and the `\/` rewrite are the
/// pattern compiler's job, not the getter's.
pub fn regexp_get_source(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    match regexp_this(ctx, this, "get RegExp.prototype.source")? {
        RegExpThis::Prototype => {
            let h = ctx.heap.intern_text("(?:)");
            Ok(JsValue::string(h))
        }
        RegExpThis::Instance(re) => Ok(regexp_slots(ctx.heap, re).0),
    }
}

/// `get RegExp.prototype.flags` (ES 22.2.6.2).
///
/// The spec assembles the string from eight `Get(R, "<flag>")` calls in
/// canonical `dgimsuvy` order. That order is observable only when `R` shadows
/// a flag with an own property, which a plain RegExp cannot; the stored
/// `[[OriginalFlags]]` is the same string for every receiver that has the
/// slots, so this reads the slot directly.
pub fn regexp_get_flags(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    match regexp_this(ctx, this, "get RegExp.prototype.flags")? {
        RegExpThis::Prototype => {
            let h = ctx.heap.intern_text("");
            Ok(JsValue::string(h))
        }
        RegExpThis::Instance(re) => Ok(regexp_slots(ctx.heap, re).1),
    }
}

/// The single-flag getters (`global`/`ignoreCase`/`multiline`/`dotAll`/
/// `unicode`/`unicodeSets`/`sticky`/`hasIndices`, ES 22.2.6.3-22.2.6.6 and
/// 22.2.6.8-22.2.6.10): each is `ToBoolean` of one flag slot, `undefined` for
/// `%RegExp.prototype%`, `TypeError` for anything without the slots.
fn regexp_flag_getter(
    ctx: &mut Ctx,
    this: JsValue,
    method: &str,
    flag: char,
) -> Result<JsValue, Throw> {
    match regexp_this(ctx, this, method)? {
        RegExpThis::Prototype => Ok(JsValue::undefined()),
        RegExpThis::Instance(re) => {
            let (_, flags) = regexp_slots(ctx.heap, re);
            let Some(h) = flags.as_string() else {
                return Ok(JsValue::false_());
            };
            Ok(JsValue::from_bool(flag_in(ctx.heap, h, flag)))
        }
    }
}

/// Membership test of `flag` in the interned flag string `h`, without
/// materializing the text (the flag string is one of the eight canonical
/// short forms, so a latin1 byte scan is exact).
fn flag_in(heap: &mut Heap, h: Handle<V12Str>, flag: char) -> bool {
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => bytes.contains(&(flag as u8)),
        // A non-latin1 flag string cannot exist (the canonicalizer only ever
        // emits ASCII), so treat it as "flag absent" rather than allocating.
        _ => false,
    }
}

macro_rules! flag_getter {
    ($name:ident, $flag:literal, $label:literal) => {
        pub fn $name(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
            regexp_flag_getter(ctx, this, $label, $flag)
        }
    };
}

flag_getter!(regexp_get_global, 'g', "get RegExp.prototype.global");
flag_getter!(
    regexp_get_ignore_case,
    'i',
    "get RegExp.prototype.ignoreCase"
);
flag_getter!(regexp_get_multiline, 'm', "get RegExp.prototype.multiline");
flag_getter!(regexp_get_dot_all, 's', "get RegExp.prototype.dotAll");
flag_getter!(regexp_get_unicode, 'u', "get RegExp.prototype.unicode");
flag_getter!(
    regexp_get_unicode_sets,
    'v',
    "get RegExp.prototype.unicodeSets"
);
flag_getter!(regexp_get_sticky, 'y', "get RegExp.prototype.sticky");
flag_getter!(
    regexp_get_has_indices,
    'd',
    "get RegExp.prototype.hasIndices"
);

/// `get RegExp.prototype.lastIndex` — the receiver's `lastIndex` slot,
/// canonicalized to a JS number.
pub fn regexp_get_last_index(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    match regexp_this(ctx, this, "get RegExp.prototype.lastIndex")? {
        RegExpThis::Prototype => Ok(JsValue::undefined()),
        RegExpThis::Instance(re) => Ok(helpers::js_number(last_index(ctx.heap, re))),
    }
}

/// `set RegExp.prototype.lastIndex` — `ToNumber` then store.
pub fn regexp_set_last_index(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    if let RegExpThis::Instance(re) = regexp_this(ctx, this, "set RegExp.prototype.lastIndex")? {
        let v = ctx.to_number(args.first().copied().unwrap_or(JsValue::undefined()));
        set_last_index(ctx.heap, re, v);
    }
    Ok(JsValue::undefined())
}

/// `get RegExp [ @@species ]` (ES 22.2.6.4): the whole body is
/// `1. Return the this value.`
pub fn regexp_species_get(
    _ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(this)
}

// -- 22.2.7: the symbol-keyed protocol methods ------------------------------
//
// `RegExpExec` (the *internal* `[[RegExpMatcher]]` call) never re-enters the
// machine, and per 22.2.7 a user override of `RegExp.prototype.exec` is *not*
// observed — the protocol methods call the internal operation. So `@@match`,
// `@@matchAll`, `@@search` and `@@split` are pure `&mut Heap` work. Only
// `@@replace` can need a user callback, and only when the replacement is
// callable; see `builtins::regexp_symbol_replace` in the install layer and the
// `NativeId::RegExpSymbolReplace` arm in the interpreter seam.

/// The `i`-th element of an `exec` result array — the whole match at `0`, the
/// capture groups after it.
///
/// Read from the *element store*, not `properties`: `match_result` allocates
/// through [`Ctx::alloc_array`], so `properties` holds only the three named
/// properties (`length`, `index`, `input`) while the matched text and captures
/// live as elements.
fn match_element(ctx: &Ctx, result: Handle<JsObject>, i: usize) -> JsValue {
    ctx.heap
        .get(result)
        .get_element(u32::try_from(i).unwrap_or(u32::MAX))
        .unwrap_or(JsValue::undefined())
}

/// The `index` own property of an `exec` result, looked up by key so the read
/// does not depend on the descriptor slot number.
fn match_index(ctx: &mut Ctx, result: Handle<JsObject>) -> JsValue {
    let key = v12_heap::PropKey::from_string(ctx.heap.intern_text("index"));
    let shape = ctx.heap.shape_of(result);
    let Some(slot) = ctx.heap.lookup_property(shape, key).and_then(|d| d.slot()) else {
        return JsValue::undefined();
    };
    ctx.heap
        .get(result)
        .properties
        .get(slot as usize)
        .copied()
        .unwrap_or(JsValue::undefined())
}

/// A `RegExp` receiver, or the `TypeError` 22.2.7 step 1 prescribes for a
/// receiver that is not one.
fn regexp_receiver(ctx: &mut Ctx, this: JsValue, method: &str) -> Result<Handle<JsObject>, Throw> {
    ctx.this_object(this, method, Some(v12_heap::Kind::RegExp))
}

/// The `RegExp` receiver's own `(source, flags)` for the protocol methods,
/// which all begin from the receiver's internal slots rather than a
/// `Get`.
fn protocol_receiver(
    ctx: &mut Ctx,
    this: JsValue,
    method: &str,
) -> Result<(Handle<JsObject>, String, String), Throw> {
    let re = regexp_receiver(ctx, this, method)?;
    let (source, flags) = regexp_source_flags(ctx.heap, re);
    Ok((re, source, flags))
}

/// `RegExp.prototype[@@match]` (ES 22.2.7.2): a non-global regexp is
/// `RegExpExec(S)`; a global one sets `lastIndex` to 0 and collects every
/// match's whole text, or returns `null`.
pub fn regexp_symbol_match(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let (re, _source, flags) = protocol_receiver(ctx, this, "RegExp.prototype[Symbol.match]")?;
    let s = ctx.to_string(args.first().copied().unwrap_or(JsValue::undefined()));
    let text_h = ctx.heap.intern_text(&s);
    if !flags.contains('g') {
        return regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)]);
    }
    set_last_index(ctx.heap, re, 0.0);
    let mut results: Vec<JsValue> = Vec::new();
    loop {
        let m = regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)])?;
        if m.is_null() {
            break;
        }
        // `m[0]` is the whole match, and the match array is engine-allocated,
        // so it is an *element*-store read (see `match_element`).
        let obj = m.as_object().expect("non-null exec result is an object");
        results.push(match_element(ctx, obj, 0));
    }
    if results.is_empty() {
        return Ok(JsValue::null());
    }
    let arr = ctx.alloc_array(results);
    Ok(JsValue::object(arr))
}

/// `RegExp.prototype[@@search]` (ES 22.2.7.5): save `lastIndex`, zero it, run
/// `RegExpExec`, restore, and answer the match's `index` or `-1`.
pub fn regexp_symbol_search(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let (re, _source, _flags) = protocol_receiver(ctx, this, "RegExp.prototype[Symbol.search]")?;
    let s = ctx.to_string(args.first().copied().unwrap_or(JsValue::undefined()));
    let previous = last_index(ctx.heap, re);
    let text_h = ctx.heap.intern_text(&s);
    set_last_index(ctx.heap, re, 0.0);
    let m = regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)]);
    // Restore before propagating: the spec's step 6 is unconditional.
    set_last_index(ctx.heap, re, previous);
    let m = m?;
    if m.is_null() {
        return Ok(helpers::js_number(-1.0));
    }
    let obj = m.as_object().expect("non-null exec result is an object");
    Ok(match_index(ctx, obj))
}

/// `RegExp.prototype[@@matchAll]` (ES 22.2.7.3).
///
/// The spec returns a `RegExp String Iterator`, a dedicated object whose
/// `next` re-runs `RegExpExec` on each call so an infinite (zero-width) match
/// cannot hang the call. This engine has no such iterator kind, so the matches
/// are drained eagerly and wrapped in the realm's array iterator: the
/// observable protocol for the common case (`for (const m of
/// re[Symbol.matchAll](s))`, `Array.from`, `.length`) is identical, and
/// `@@matchAll` on a zero-width-match pattern terminates instead of looping
/// forever. Documented deviation, not a silent one.
pub fn regexp_symbol_match_all(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let (re, _source, flags) = protocol_receiver(ctx, this, "RegExp.prototype[Symbol.matchAll]")?;
    let s = ctx.to_string(args.first().copied().unwrap_or(JsValue::undefined()));
    let text_h = ctx.heap.intern_text(&s);
    if flags.contains('g') {
        set_last_index(ctx.heap, re, 0.0);
    }
    let mut results: Vec<JsValue> = Vec::new();
    loop {
        let m = regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)])?;
        if m.is_null() {
            break;
        }
        results.push(m);
        // A non-global regexp matches at most once, and a zero-width match
        // must still advance so the drain terminates.
        if !flags.contains('g') {
            break;
        }
    }
    if results.is_empty() {
        return Ok(JsValue::null());
    }
    // The spec wraps these in a `RegExp String Iterator` whose `next`
    // re-runs `RegExpExec` on each call, so the matches are produced lazily
    // and a zero-width pattern cannot hang the call. This engine has no such
    // iterator kind, so the matches are drained eagerly and returned as an
    // array: `for (const m of …)`, `Array.from`, `.length` and `.next` on a
    // wrapper are not available, but the element sequence is the same and the
    // drain always terminates. Documented deviation, not a silent one.
    Ok(JsValue::object(ctx.alloc_array(results)))
}

/// `RegExp.prototype[@@replace]` (ES 22.2.7.4).
///
/// Handles the `replaceValue` is *not* callable half of the spec: `ToString`
/// then `GetSubstitution` per match, spliced into the input. The callable half
/// needs to re-enter the machine, which a `&mut Heap` native cannot do, so it
/// is intercepted at `Interp::run_callback_builtin` and never reaches this
/// body; the `TypeError` below is only reachable by a direct registry call
/// with a callable replacement and exists so the function is total.
pub fn regexp_symbol_replace(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let (re, _source, flags) = protocol_receiver(ctx, this, "RegExp.prototype[Symbol.replace]")?;
    let s = ctx.to_string(args.first().copied().unwrap_or(JsValue::undefined()));
    let replace_value = args.get(1).copied().unwrap_or(JsValue::undefined());
    if is_callable(ctx.heap, replace_value) {
        return Err(ctx.type_error(
            "TypeError: RegExp.prototype[Symbol.replace] with a function replacement \
             must be invoked through the interpreter",
        ));
    }
    let template = ctx.to_string(replace_value);
    let global = flags.contains('g');
    if global {
        set_last_index(ctx.heap, re, 0.0);
    }
    let text_h = ctx.heap.intern_text(&s);
    let mut out = String::new();
    let mut cursor = 0usize;
    loop {
        let m = regexp_exec(ctx, JsValue::object(re), &[JsValue::string(text_h)])?;
        if m.is_null() {
            break;
        }
        // Snapshot before any `ToString`: formatting allocates, which
        // invalidates a live heap borrow.
        let obj = m.as_object().expect("non-null exec result is an object");
        let whole_v = match_element(ctx, obj, 0);
        let index_v = match_index(ctx, obj);
        let whole_text = ctx.to_string(whole_v);
        let start = ctx.to_number(index_v);
        // The capture count is the array's `length` minus one.
        let group_count = ctx
            .heap
            .get(obj)
            .properties
            .first()
            .copied()
            .and_then(|v| v.as_smi().map(f64::from))
            .unwrap_or(1.0)
            .max(1.0) as usize
            - 1;
        let mut captures: Vec<Option<String>> = Vec::with_capacity(group_count);
        for i in 1..=group_count {
            let v = match_element(ctx, obj, i);
            captures.push(if v.is_undefined() {
                None
            } else {
                Some(ctx.to_string(v))
            });
        }
        // Every offset is a UTF-16 index (the exec result's `index`/`length`
        // are code units), so all slicing goes through the code-unit vector.
        let units: Vec<u16> = s.encode_utf16().collect();
        let a = (start.max(0.0) as usize).min(units.len());
        let b = (a + whole_text.encode_utf16().count()).min(units.len());
        if a < cursor {
            if !global {
                break;
            }
            continue;
        }
        out.push_str(&String::from_utf16_lossy(&units[cursor..a]));
        out.push_str(&get_substitution(&template, &whole_text, &captures, a, &s));
        cursor = b;
        if !global {
            break;
        }
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    out.push_str(&String::from_utf16_lossy(&units[cursor.min(units.len())..]));
    Ok(JsValue::string(ctx.heap.intern_text(&out)))
}

/// `RegExp.prototype[@@split]` (ES 22.2.7.6).
///
/// The spec walks the input with a cursor `q`, calling `RegExpExec` at each
/// position and emitting a piece whenever the match starts at or after the
/// cursor. This engine's `RegExpExec` deliberately ignores `lastIndex` for a
/// *non*-global pattern — it searches from 0 — so driving the loop through it
/// would re-find the same match forever. The cursor is therefore applied
/// directly: [`split_match`] compiles the pattern with `y` added and matches at
/// exactly `q`, which is what the spec's `q`/`e` bookkeeping achieves and
/// which terminates for every input, zero-width matches included.
pub fn regexp_symbol_split(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let (re, source, flags) = protocol_receiver(ctx, this, "RegExp.prototype[Symbol.split]")?;
    let s = ctx.to_string(args.first().copied().unwrap_or(JsValue::undefined()));
    let units: Vec<u16> = s.encode_utf16().collect();
    let limit = match args.get(1) {
        Some(&v) if !v.is_undefined() => ctx.to_number(v),
        _ => 4294967295.0,
    };
    if limit <= 0.0 {
        return Ok(JsValue::object(ctx.alloc_array(Vec::new())));
    }
    let limit = limit.min(4294967295.0) as usize;
    // An empty pattern is stored canonically as `(?:)`, which matches nothing;
    // the spec's step 7 handles that by splitting into code units.
    if source == "(?:)" || source.is_empty() {
        let items: Vec<JsValue> = units
            .iter()
            .take(limit)
            .map(|&u| JsValue::string(ctx.heap.intern_string(v12_heap::V12Str::utf16(vec![u]))))
            .collect();
        return Ok(JsValue::object(ctx.alloc_array(items)));
    }
    let mut items: Vec<JsValue> = Vec::new();
    // `p` is the start of the piece not yet emitted; `q` is the cursor the spec
    // advances one code unit (one code point under `u`/`v`) per iteration.
    let mut p = 0usize;
    let mut q = 0usize;
    while q < units.len() && items.len() < limit {
        let e = advance(&units, q, &flags);
        let Some(SplitMatch {
            start,
            len,
            captures,
        }) = split_match(ctx, re, &source, &flags, &units, q)?
        else {
            q = e;
            continue;
        };
        // Step 13.d.ii: a match *behind* the cursor produces no piece; the
        // cursor still moves so the loop cannot stall.
        if start < q {
            q = if len == 0 {
                advance(&units, q, &flags)
            } else {
                e
            };
            continue;
        }
        let end = (start + len).min(units.len());
        items
            .push(JsValue::string(ctx.heap.intern_string(
                v12_heap::V12Str::utf16(units[p..start].to_vec()),
            )));
        if items.len() == limit {
            return Ok(JsValue::object(ctx.alloc_array(items)));
        }
        // The spec inserts a non-empty match's captures between the pieces.
        if len > 0 {
            for cap in &captures {
                let Some(cap) = cap else { continue };
                items.push(JsValue::string(ctx.heap.intern_text(cap)));
                if items.len() == limit {
                    return Ok(JsValue::object(ctx.alloc_array(items)));
                }
            }
        }
        p = end;
        // A zero-width match must still move the cursor, or the loop spins.
        q = if len == 0 {
            advance(&units, start, &flags)
        } else {
            end
        };
    }
    items.push(JsValue::string(ctx.heap.intern_string(
        v12_heap::V12Str::utf16(units[p.min(units.len())..].to_vec()),
    )));
    Ok(JsValue::object(ctx.alloc_array(items)))
}

/// One anchored `@@split` match: where it starts, how many code units the whole
/// match covers, and its capture texts (`None` for an unmatched group).
struct SplitMatch {
    start: usize,
    len: usize,
    captures: Vec<Option<String>>,
}

/// One `@@split` match attempt at exactly `q`. Compiles with `y` added so the
/// match is anchored at the cursor — that anchoring is what makes the spec's
/// cursor walk expressible on top of an `RegExpExec` that re-searches from 0.
///
/// The result is written through the receiver's own `lastIndex` slot so a
/// second `@@split` call on the same regexp resumes where the spec's would.
fn split_match(
    ctx: &mut Ctx,
    re: Handle<JsObject>,
    source: &str,
    flags: &str,
    units: &[u16],
    q: usize,
) -> Result<Option<SplitMatch>, Throw> {
    let mut sticky = flags.to_string();
    if !sticky.contains('y') {
        sticky.push('y');
    }
    let compiled = compile_pattern(source, &sticky).map_err(|e| {
        let h = ctx.heap.intern_text(&format!("SyntaxError: {e}"));
        JsValue::string(h)
    })?;
    let Some(m) = compiled.exec(units, q) else {
        return Ok(None);
    };
    set_last_index(ctx.heap, re, (m.end() as f64).max(q as f64 + 1.0));
    let slice = |span: std::ops::Range<usize>| -> String {
        String::from_utf16_lossy(&units[span.start.min(units.len())..span.end.min(units.len())])
    };
    let captures = (1..=m.capture_count())
        .map(|i| m.group(i).map(|span| slice(span.start..span.end)))
        .collect();
    Ok(Some(SplitMatch {
        start: m.start(),
        len: m.span().len(),
        captures,
    }))
}

/// ES `AdvanceStringIndex(S, index, unicode)`: one code unit, or one whole
/// code point under `u`/`v`.
fn advance(units: &[u16], index: usize, flags: &str) -> usize {
    if !(flags.contains('u') || flags.contains('v')) {
        return (index + 1).min(units.len());
    }
    let next = (index + 1).min(units.len());
    if next < units.len() && (0xD800..0xDC00).contains(&units[index]) {
        return next + 1;
    }
    next
}

/// `IsCallable`, as far as a `&mut Heap` native can tell.
fn is_callable(heap: &Heap, v: JsValue) -> bool {
    v.as_object()
        .is_some_and(|o| heap.get(o).kind == v12_heap::Kind::Function)
}

/// ES 22.2.7.4 `GetSubstitution` for the string-replacement half: `$$`, `$&`,
/// ``$` ``, `$'`, `$n`/`$nn`, and a literal `$` before anything else.
fn get_substitution(
    template: &str,
    matched: &str,
    captures: &[Option<String>],
    position: usize,
    input: &str,
) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('&') => {
                chars.next();
                out.push_str(matched);
            }
            Some('`') => {
                chars.next();
                out.push_str(&input[..position.min(input.len())]);
            }
            Some('\'') => {
                chars.next();
                let tail = position + matched.len();
                out.push_str(&input[tail.min(input.len())..]);
            }
            Some(d @ '0'..='9') => {
                let mut digits = String::new();
                digits.push(*d);
                chars.next();
                // `$nn` is a two-digit index only when it names a real
                // capture; otherwise `$n` stands alone and the second digit is
                // literal text. The bound check needs the peeked digit copied
                // out first — holding the iterator's borrow across
                // `chars.next()` is what the borrow checker rejects.
                let second = chars.peek().copied();
                if let Some(second @ '0'..='9') = second {
                    let mut two_digits = digits.clone();
                    two_digits.push(second);
                    if let Ok(two) = two_digits.parse::<usize>()
                        && two >= 1
                        && two <= captures.len()
                    {
                        chars.next();
                        digits.push(second);
                    }
                }
                let n: usize = digits.parse().unwrap_or(0);
                if n >= 1 && n <= captures.len() {
                    if let Some(Some(text)) = captures.get(n - 1) {
                        out.push_str(text);
                    }
                }
            }
            _ => out.push('$'),
        }
    }
    out
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
///
/// Goes through [`Ctx::alloc_array`] so the result carries
/// `%Array.prototype%` and the canonical `root --length--> child` shape. The
/// engine builds this array outside the interpreter's `NewArray` arm, so
/// without that linkage `Array.isArray(result)` is `false` and
/// `result.constructor` is `undefined` — which is a whole conformance cluster
/// on its own. `index`/`input` are then installed as ordinary spec-attr
/// properties (`{writable, enumerable, configurable}`) rather than written
/// into raw slots, so the shape and the storage stay in lockstep.
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
    let arr = ctx.alloc_array(elements);
    let input_h = ctx.heap.intern_text(input_text);
    ctx.define_data_prop_with_attrs(
        arr,
        "index",
        helpers::smi_or_f64(m.start() as i64),
        v12_heap::Attrs::DEFAULT,
    );
    ctx.define_data_prop_with_attrs(
        arr,
        "input",
        JsValue::string(input_h),
        v12_heap::Attrs::DEFAULT,
    );
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
