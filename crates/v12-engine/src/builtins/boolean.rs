//! Boolean built-in.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.

use v12_heap::JsValue;
use v12_native::Throw;

use super::ctx::Ctx;

/// `Boolean(value)` – converts value to boolean following ToBoolean.
pub fn boolean_construct(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let truthy = to_boolean(ctx, v);
    Ok(JsValue::from_bool(truthy))
}

pub(crate) fn to_boolean(ctx: &Ctx, v: JsValue) -> bool {
    if v.is_true() {
        return true;
    }
    if v.is_false() || v.is_undefined() || v.is_null() {
        return false;
    }
    if let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) {
        return n != 0.0 && !n.is_nan();
    }
    if let Some(h) = v.as_string() {
        return !ctx.heap.get(h).is_empty();
    }
    // Objects are truthy.
    if v.is_object() {
        return true;
    }
    false
}

/// `Boolean.prototype.toString` – `"true"`/`"false"` for the primitive
/// receiver (wrapper objects are not modeled).
pub fn boolean_proto_to_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = if this.is_true() {
        "true"
    } else if this.is_false() {
        "false"
    } else {
        return Err(ctx.type_error(
            "TypeError: Boolean.prototype.toString requires that 'this' be a Boolean",
        ));
    };
    Ok(JsValue::string(ctx.heap.intern_text(text)))
}

/// `Boolean.prototype.valueOf` – the primitive receiver itself. A
/// non-Boolean receiver throws (no unchecked `this` passthrough).
pub fn boolean_proto_value_of(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    if this.is_true() || this.is_false() {
        Ok(this)
    } else {
        Err(ctx.type_error(
            "TypeError: Boolean.prototype.valueOf requires that 'this' be a Boolean",
        ))
    }
}
