//! Symbol built-in.
//!
//! v1: symbols are fresh heap handles (`V12Symbol` is an opaque unit —
//! identity is the handle). Descriptions and the `Symbol.for` registry are
//! not modeled; well-known symbols return fresh symbols (still `typeof`
/// `"symbol"`, which is what the conformance slices probe first).

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

/// Well-known symbols: each returns a fresh symbol (v1 — `typeof` correct,
/// identity singletons deferred).
pub fn symbol_well_known(
    heap: &mut Heap,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let h = heap.alloc(v12_heap::V12Symbol);
    heap.add_root(JsValue::symbol(h));
    Ok(JsValue::symbol(h))
}
