//! Symbol built-in.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.
//!
//! v1: symbols are fresh heap handles (`V12Symbol` is an opaque unit —
//! identity is the handle). `Symbol.for` shares per-key symbols through
//! the heap's global registry (`Heap::symbol_for_key`); `keyFor` reads
//! the reverse map. Well-known singletons keep their existing O(1) paths
//! (interpreter surface + install layer) and never consult the registry.
//!
//! Well-known singleton contract: the JS-visible identity of
//! `Symbol.iterator` is owned by the interpreter realm — its
//! `symbol_iterator_surface` answers the read with the realm's
//! lazily-allocated, rooted handle *before* shape lookup, so
//! `Symbol.iterator === Symbol.iterator` holds without engine caching.
//! The engine-side [`symbol_well_known`] handler below only runs if the
//! installed native is explicitly *called* (normal property reads never
//! invoke it); it is shared by all twelve well-known ids, so it cannot key
//! a per-name cache itself — per-name singletons belong to the
//! install/dispatch layer (`install_value`-style installs), not here.

use v12_heap::JsValue;
use v12_native::Throw;

use super::ctx::Ctx;

/// Allocates a fresh rooted symbol (v1: no description, no registry).
fn fresh_symbol(ctx: &mut Ctx) -> JsValue {
    let h = ctx.heap.alloc(v12_heap::V12Symbol);
    let v = JsValue::symbol(h);
    ctx.add_root(v);
    v
}

/// `Symbol(description?)` — returns a fresh symbol. `new Symbol()` throws
/// (construct path passes the constructor as `this`, a Function object).
pub fn symbol_construct(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if let Some(o) = this.as_object()
        && ctx.heap.get(o).kind == v12_heap::Kind::Function
    {
        return Err(ctx.type_error("TypeError: Symbol is not a constructor"));
    }
    Ok(fresh_symbol(ctx))
}

/// `Symbol.for(key)` — the shared symbol for `key` from the heap's
/// global registry (spec: same key → identical symbol across calls;
/// `Symbol.for` never mints). The key coerces via ToString; the
/// registry probe is O(1), allocation happens once per distinct key.
pub fn symbol_for(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let key_arg = args.first().copied().unwrap_or(JsValue::undefined());
    let key_text = ctx.to_string(key_arg);
    Ok(JsValue::symbol(ctx.heap.symbol_for_key(&key_text)))
}

/// `Symbol.keyFor(sym)` — the registry key for a `Symbol.for` symbol,
/// else `undefined` (fresh and well-known symbols were never
/// registered). Non-symbols throw per spec.
pub fn symbol_key_for(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(sym) = args.first().and_then(|v| v.as_symbol()) else {
        return Err(ctx.type_error("TypeError: Symbol.keyFor requires a symbol"));
    };
    // Copy the key out first: the registry borrow must end before the
    // interning mutable borrow below.
    match ctx.heap.symbol_key_for(sym).map(str::to_owned) {
        Some(key) => Ok(JsValue::string(ctx.heap.intern_text(&key))),
        None => Ok(JsValue::undefined()),
    }
}

/// `Symbol.prototype.toString` — `"Symbol()"` (descriptions not modeled).
pub fn symbol_proto_to_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.as_symbol().is_none() {
        return Err(ctx.type_error("TypeError: Symbol.prototype.toString requires a Symbol"));
    }
    Ok(JsValue::string(ctx.heap.intern_text("Symbol()")))
}

/// `Symbol.prototype.valueOf` — the symbol itself.
pub fn symbol_proto_value_of(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.as_symbol().is_none() {
        return Err(ctx.type_error("TypeError: Symbol.prototype.valueOf requires a Symbol"));
    }
    Ok(this)
}

/// `Symbol.prototype.description` — v1 `undefined` (opaque symbols).
pub fn symbol_proto_description(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.as_symbol().is_none() {
        return Err(
            ctx.type_error("TypeError: Symbol.prototype.description requires a Symbol"),
        );
    }
    Ok(JsValue::undefined())
}

/// Well-known symbols: each call mints a fresh symbol. This handler only
/// runs if the installed native is explicitly called — normal
/// `Symbol.<name>` reads never reach it (`Symbol.iterator` is answered by
/// the interpreter realm's singleton surface before shape lookup, so
/// `===` identity holds at the JS level). Shared by all well-known ids, so
/// per-name singleton caching must live in the install/dispatch layer.
pub fn symbol_well_known(
    ctx: &mut Ctx,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(fresh_symbol(ctx))
}
