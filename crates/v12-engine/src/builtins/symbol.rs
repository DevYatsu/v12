//! Symbol built-in.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.
//!
//! v1: symbols are fresh heap handles (`V12Symbol` is an opaque unit —
//! identity is the handle). Descriptions and the `Symbol.for` registry are
//! not modeled.
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

/// `Symbol.for(key)` — v1 returns a fresh symbol (no global registry yet).
pub fn symbol_for(ctx: &mut Ctx, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(fresh_symbol(ctx))
}

/// `Symbol.keyFor(sym)` — v1 always `undefined` (no registry).
pub fn symbol_key_for(
    _ctx: &mut Ctx,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(JsValue::undefined())
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
