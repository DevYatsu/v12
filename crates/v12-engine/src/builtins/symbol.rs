//! Symbol built-in.
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

use v12_heap::{Heap, JsValue};
use v12_native::Throw;

/// `Symbol(description?)` — returns a fresh symbol. `new Symbol()` throws
/// (construct path passes the constructor as `this`, a Function object).
pub fn symbol_construct(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if let Some(o) = this.as_object()
        && heap.get(o).kind == v12_heap::Kind::Function
    {
        return Err(Throw::type_error(heap, "TypeError: Symbol is not a constructor"));
    }
    let h = heap.alloc(v12_heap::V12Symbol);
    heap.add_root(JsValue::symbol(h));
    Ok(JsValue::symbol(h))
}

/// `Symbol.for(key)` — v1 returns a fresh symbol (no global registry yet).
pub fn symbol_for(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let h = heap.alloc(v12_heap::V12Symbol);
    heap.add_root(JsValue::symbol(h));
    Ok(JsValue::symbol(h))
}

/// `Symbol.keyFor(sym)` — v1 always `undefined` (no registry).
pub fn symbol_key_for(
    _heap: &mut Heap,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(JsValue::undefined())
}

/// `Symbol.prototype.toString` — `"Symbol()"` (descriptions not modeled).
pub fn symbol_proto_to_string(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.as_symbol().is_none() {
        return Err(Throw::type_error(
            heap,
            "TypeError: Symbol.prototype.toString requires a Symbol",
        ));
    }
    Ok(JsValue::string(heap.intern_text("Symbol()")))
}

/// `Symbol.prototype.valueOf` — the symbol itself.
pub fn symbol_proto_value_of(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.as_symbol().is_none() {
        return Err(Throw::type_error(
            heap,
            "TypeError: Symbol.prototype.valueOf requires a Symbol",
        ));
    }
    Ok(this)
}

/// `Symbol.prototype.description` — v1 `undefined` (opaque symbols).
pub fn symbol_proto_description(
    heap: &mut Heap,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.as_symbol().is_none() {
        return Err(Throw::type_error(
            heap,
            "TypeError: Symbol.prototype.description requires a Symbol",
        ));
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
    heap: &mut Heap,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let h = heap.alloc(v12_heap::V12Symbol);
    heap.add_root(JsValue::symbol(h));
    Ok(JsValue::symbol(h))
}
