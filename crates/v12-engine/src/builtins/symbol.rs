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
//! Well-known singleton contract: the JS-visible identity of a well-known
//! symbol is the *value installed on the `Symbol` constructor*, and nothing
//! else. There are two install routes and both end in a shape-bound data
//! property, so a read is always an ordinary property lookup and never a
//! dispatch:
//!
//! 1. The realm allocates a canonical symbol per name and **overwrites** the
//!    macro's handler-function placeholder with it (`realm.rs`,
//!    `overwrite_data_prop`) — used for `@@iterator`, `@@toPrimitive`,
//!    `@@toStringTag`, `@@species`, and `@@asyncIterator`, which the engine
//!    addresses as keys and so must be the same handle everywhere.
//! 2. [`symbol_well_known`] is evaluated **once, at install time**, and its
//!    result is stored as the property value
//!    ([`super::install_value_with_attrs`]) — used for `@@dispose` and
//!    `@@asyncDispose`, which have no engine-internal consumer.
//!
//! Either way the property *is* the singleton, so `Symbol.dispose ===
//! Symbol.dispose` holds by construction rather than by a cache. The handler
//! is shared by every well-known id and cannot key a per-name cache itself;
//! that is why it is free to mint a fresh symbol on each of the (at most
//! one) evaluations it ever sees.

use v12_heap::{Handle, JsValue};
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
pub fn symbol_construct(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
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
pub fn symbol_key_for(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
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

/// `Symbol.prototype.description` — the `[[Description]]` internal slot, or
/// `undefined` when absent.
///
/// v1 symbols carry no `[[Description]]`: `V12Symbol` is an opaque heap unit
/// (see the module doc), so a `Symbol()` call mints a symbol whose description
/// is genuinely absent and correctly reads `undefined`.
///
/// A *well-known* symbol's description is a spec constant though
/// (`Symbol.dispose.description === "Symbol.dispose"`, 20.4.2), and it is
/// derivable without touching the heap: it is the name of the `Symbol`
/// constructor property whose value is this very symbol. Reading the
/// constructor's own properties is the only place a well-known symbol's
/// identity is recorded, so it doubles as the description table — and it
/// cannot disagree with the singleton, because it *is* the singleton.
pub fn symbol_proto_description(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(sym) = this.as_symbol() else {
        return Err(ctx.type_error("TypeError: Symbol.prototype.description requires a Symbol"));
    };
    Ok(well_known_description(ctx, sym)
        .map(|text| JsValue::string(ctx.heap.intern_text(&text)))
        .unwrap_or(JsValue::undefined()))
}

/// The spec `[[Description]]` of a well-known symbol, or `None` for a symbol
/// that is not one of this realm's (a `Symbol()` call, a `Symbol.for` entry).
///
/// Probes the `Symbol` constructor's own properties for a value equal to
/// `sym`. The name is turned into the spec spelling by prefixing `Symbol.`
/// — which is exactly how 20.4.2 defines every one of them
/// (`@@dispose` is described as `"Symbol.dispose"`), so there is no table to
/// keep in sync.
fn well_known_description(ctx: &mut Ctx, sym: Handle<v12_heap::V12Symbol>) -> Option<String> {
    let ctor = ctx.intrinsic("Symbol").and_then(|v| v.as_object())?;
    // Only string-keyed properties can carry a name, so the name table is the
    // search space: for each well-known name, read the constructor's own value
    // and compare handles. Sixteen shape lookups, once, and only on a
    // `Symbol.prototype.description` call.
    let shape = ctx.heap.shape_of(ctor);
    for wk in WELL_KNOWN_SYMBOL_NAMES {
        let prop = v12_heap::PropKey::from_string(ctx.heap.intern_text(wk));
        let hit = ctx
            .heap
            .lookup_property(shape, prop)
            .and_then(|d| d.slot())
            .and_then(|slot| ctx.heap.get(ctor).properties.get(slot as usize))
            .and_then(|v| v.as_symbol()) == Some(sym);
        if hit {
            return Some(format!("Symbol.{wk}"));
        }
    }
    None
}

/// Every well-known `Symbol` property name (ECMA-262 20.4.2, table 2), in
/// spec order.
///
/// The engine's realm installs most of these by *overwriting* the macro's
/// handler-function placeholder with a canonical symbol, so the property name
/// is the only stable handle on which name a given symbol is. `dispose` and
/// `asyncDispose` are the two that install the singleton directly
/// ([`symbol_well_known`]); the rest exist here so
/// [`well_known_description`] resolves a symbol whose value was installed by
/// either route.
const WELL_KNOWN_SYMBOL_NAMES: &[&str] = &[
    "iterator",
    "asyncIterator",
    "hasInstance",
    "isConcatSpreadable",
    "match",
    "replace",
    "search",
    "species",
    "split",
    "toPrimitive",
    "toStringTag",
    "unscopables",
    "dispose",
    "asyncDispose",
];

/// Well-known symbols: mints the symbol that becomes this realm's singleton.
///
/// The handler is shared by every well-known id and cannot key a per-name
/// cache itself — it is never *called* for a normal read. The install path
/// (`builtins::install_value_with_attrs`) evaluates it exactly once, at
/// install time, and stores the result as the property value, so the stored
/// value *is* the singleton: every later read is an ordinary shape lookup and
/// observes the same handle. That is why this can mint freely, and why
/// `Symbol.dispose === Symbol.dispose` holds.
///
/// Nothing registers the result with `Heap::symbol_for_key`, so
/// `Symbol.keyFor` answers `undefined` for it per 20.4.2.
pub fn symbol_well_known(
    ctx: &mut Ctx,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(fresh_symbol(ctx))
}
