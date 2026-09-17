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

/// `EvalError(message)` – same construction, `EvalError` class.
pub fn eval_error_create(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    error_create_named(ctx, "EvalError", args)
}

/// `URIError(message)` – same construction, `URIError` class.
pub fn uri_error_create(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    error_create_named(ctx, "URIError", args)
}

/// `Error.isError(value)` – whether `value` is an error object (any class).
pub fn error_is_error(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let is_error = args
        .first()
        .and_then(|v| v.as_object())
        .is_some_and(|obj| ctx.heap.get(obj).kind == v12_heap::Kind::Error);
    Ok(JsValue::from_bool(is_error))
}

/// `Error.prototype.toString()` – `"name: message"` (either side omitted
/// when empty).
///
/// Dispatch-only until it is installed on `Error.prototype` (needs realm
/// wiring — PENDING-WIRING).
pub fn error_proto_to_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(obj) = this.as_object() else {
        return Err(ctx.type_error("TypeError: Error.prototype.toString called on non-object"));
    };
    let get_str = |ctx: &mut Ctx, name: &str, fallback: &str| -> Result<String, Throw> {
        let key = v12_heap::PropKey::from_string(ctx.heap.intern_text(name));
        let got =
            crate::internal_methods::dispatch_get(&mut *ctx.heap, obj, key, JsValue::object(obj))
                .map_err(Throw::Value)?;
        if got.is_undefined() {
            return Ok(fallback.to_string());
        }
        Ok(ctx.to_string(got))
    };
    let name = get_str(ctx, "name", "Error")?;
    let message = get_str(ctx, "message", "")?;
    let text = if name.is_empty() {
        message
    } else if message.is_empty() {
        name
    } else {
        format!("{name}: {message}")
    };
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
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
    let value = super::registry::error_object(&mut *ctx.heap, global, kind, &text);
    // ES `InstallErrorCause`: when `options` (2nd arg) is an object with a
    // non-`undefined` `cause`, install it as an own data property.
    if let Some(options) = args.get(1).and_then(|v| v.as_object()) {
        let key = v12_heap::PropKey::from_string(ctx.heap.intern_text("cause"));
        let cause = crate::internal_methods::dispatch_get(
            &mut *ctx.heap,
            options,
            key,
            JsValue::object(options),
        )
        .map_err(Throw::Value)?;
        if !cause.is_undefined() {
            let obj = value.as_object().expect("error_object returns an object");
            super::builtin_install_prop(&mut *ctx.heap, obj, "cause", cause);
        }
    }
    Ok(value)
}
