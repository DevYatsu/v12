//! Error built-ins.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.

use v12_heap::{JsObject, JsValue};
use v12_native::Throw;

use super::ctx::Ctx;

/// `Error(message)` – creates an error object with a message.
pub fn error_create(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let message = args.first().copied().unwrap_or(JsValue::undefined());
    let name_h = ctx.heap.intern_text("Error");
    let msg_h = if let Some(h) = message.as_string() {
        h
    } else if message.is_undefined() {
        ctx.heap.intern_text("")
    } else {
        // Non-string message: render it (best-effort) as the message text.
        let text = ctx.to_string(message);
        ctx.heap.intern_text(&text)
    };
    let obj = ctx.alloc_obj(JsObject::error(name_h, msg_h));
    Ok(JsValue::object(obj))
}
