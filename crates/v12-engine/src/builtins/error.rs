//! Error built-ins.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.

use v12_heap::JsValue;
use v12_native::Throw;

use super::ctx::Ctx;

/// `Error(message)` – creates an error object with a message.
pub fn error_create(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    error_create_named(ctx, "Error", args)
}

/// `TypeError(message)` – same construction, `TypeError` class.
pub fn type_error_create(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    error_create_named(ctx, "TypeError", args)
}

/// `RangeError(message)` – same construction, `RangeError` class.
pub fn range_error_create(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    error_create_named(ctx, "RangeError", args)
}

/// `ReferenceError(message)` – same construction, `ReferenceError` class.
pub fn reference_error_create(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    error_create_named(ctx, "ReferenceError", args)
}

/// `SyntaxError(message)` – same construction, `SyntaxError` class.
pub fn syntax_error_create(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    error_create_named(ctx, "SyntaxError", args)
}

fn error_create_named(ctx: &mut Ctx, kind: &str, args: &[JsValue]) -> Result<JsValue, Throw> {
    let message = args.first().copied().unwrap_or(JsValue::undefined());
    let text: String = if message.is_undefined() {
        String::new()
    } else if let Some(h) = message.as_string() {
        ctx.string_text(h)
    } else {
        // Non-string message: render it (best-effort) as the message text.
        ctx.to_string(message)
    };
    // `error_object` shape-binds `name`/`message`/`constructor` and links the
    // instance's [[Prototype]] to the class prototype object installed by the
    // realm, so `instanceof`/`.name`/`.message` all observe the class. The
    // native-dispatch `Ctx` carries no global, so fall back to the heap's
    // realm registry (the primary realm's global).
    let global = ctx
        .global()
        .or_else(|| ctx.heap.realm_globals().first().copied());
    Ok(super::registry::error_object(
        &mut *ctx.heap,
        global,
        kind,
        &text,
    ))
}
