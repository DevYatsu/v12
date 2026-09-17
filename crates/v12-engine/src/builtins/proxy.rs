//! `Proxy` built-ins — phase 1: construction and representation.
//!
//! This phase installs the `%Proxy%` constructor and the proxy exotic object
//! representation. A proxy is a `Kind::Proxy` object carrying its
//! `[[ProxyTarget]]`/`[[ProxyHandler]]` in `JsObject::proxy_target` /
//! `JsObject::proxy_handler` (see `v12-heap/src/object.rs`).
//!
//! Trap dispatch lives in the interpreter (`v12-interp/src/property.rs`):
//! `proxy_op_has`, `proxy_op_get`, `proxy_op_set`, and `proxy_op_own_keys`
//! consult the handler trap via `call_inline`. `ownKeys` has no caller yet
//! (PENDING-WIRING: `Object.keys`/`getOwnPropertyNames`/`getOwnPropertySymbols`
//! and for-in live in `builtins/object.rs`); the remaining traps
//! (`defineProperty`, `getOwnPropertyDescriptor`, `deleteProperty`,
//! `get/setPrototypeOf`, `isExtensible`, `preventExtensions`, `apply`,
//! `construct`) still report "not implemented" via the internal-method stub
//! table (`v12-engine/src/internal_methods.rs`).
//!
//! Revocation is represented, not enforced: `Proxy.revocable` wires a
//! revocation function that clears the two slots. The invariant every future
//! trap dispatcher must check is
//! `kind == Kind::Proxy && proxy_target.is_none()` ⇒ revoked ⇒ `TypeError`.

use v12_heap::{Attrs, Heap, JsObject, JsValue, Kind};
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

/// Makes `ctx`'s error builders file realm-linked errors.
///
/// `call_ctx` (the dispatch shim the `define_builtins!` arms route through)
/// builds a detached `Ctx` with no global, so a `ctx.type_error` would produce
/// an error with no `constructor` back-link and `assert.throws(TypeError, …)`
/// would reject it. The realm global is recoverable from the heap, so attach it
/// when the caller did not.
fn ensure_global(ctx: &mut Ctx) {
    if ctx.global.is_none() {
        ctx.global = ctx.heap.realm_globals().first().copied();
    }
}

/// Validates `target`/`handler` and allocates the proxy exotic object.
///
/// ES `ProxyCreate` steps 1–2: both arguments must be `Object`, otherwise
/// `TypeError`. The handler is *not* consulted — no trap runs during
/// construction.
fn create_proxy(ctx: &mut Ctx, args: &[JsValue]) -> Result<JsValue, Throw> {
    ensure_global(ctx);
    let target = args.first().copied().unwrap_or_else(JsValue::undefined);
    let handler = args.get(1).copied().unwrap_or_else(JsValue::undefined);
    let (Some(target_obj), Some(handler_obj)) = (target.as_object(), handler.as_object()) else {
        return Err(
            ctx.type_error("TypeError: Cannot create proxy with a non-object as target or handler")
        );
    };
    let proxy = ctx.alloc_obj(JsObject::proxy(target_obj, handler_obj));
    Ok(JsValue::object(proxy))
}

/// `new Proxy(target, handler)`.
///
/// The native seam receives the callee as `this` on the construct path and the
/// call receiver on the plain-call path (see `Interp::prepare_construct` /
/// `Interp::prepare_call`), so the `NewTarget is undefined` spec step is the
/// function-kind test below. Limitation: `Proxy.call(Proxy, {}, {})` also sees
/// the constructor as `this` and so constructs instead of throwing; NewTarget
/// is not carried into the native seam in v1.
pub fn proxy_construct(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    ensure_global(ctx);
    let is_construct = this
        .as_object()
        .is_some_and(|o| ctx.heap.get(o).kind == Kind::Function);
    if !is_construct {
        return Err(ctx.type_error("TypeError: Constructor Proxy requires 'new'"));
    }
    create_proxy(ctx, args)
}

/// `Proxy.revocable(target, handler)` — a fresh ordinary object with own data
/// properties `proxy` then `revoke` (ES steps 6–7, `CreateDataProperty`).
///
/// The revocation function is a *bound* function: `FunctionTarget::Bound`
/// points at a state object whose `elements` are `[target_fn, this_arg]` — the
/// layout `Interp::prepare_call`'s `Bound` arm reads. Its inner target is the
/// heap-only native [`revoke_proxy`], and its bound `this` is the proxy, which
/// keeps the proxy reachable through `FunctionTarget::Bound`'s trace arm.
pub fn proxy_revocable(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    ensure_global(ctx);
    let proxy_v = create_proxy(ctx, args)?;
    ctx.add_root(proxy_v);

    let inner = ctx.alloc_obj(JsObject::function(
        v12_heap::FunctionTarget::Native(revoke_proxy),
        None,
    ));
    let state = ctx.alloc_obj(JsObject {
        elements: vec![JsValue::object(inner), proxy_v],
        ..JsObject::default()
    });
    let revoke = ctx.alloc_obj(JsObject::function(
        v12_heap::FunctionTarget::Bound(state),
        None,
    ));
    // `CreateBuiltinFunction` attrs for `length`/`name` (ES 17): non-writable,
    // non-enumerable, configurable. Installed in that order so
    // `getOwnPropertyNames` reports `length` before `name`
    // (revocation-function-property-order.js).
    ctx.define_data_prop_with_attrs(
        revoke,
        "length",
        helpers::smi_or_f64(0),
        Attrs::new(false, false, true),
    );
    let empty_name = ctx.heap.intern_text("");
    ctx.define_data_prop_with_attrs(
        revoke,
        "name",
        JsValue::string(empty_name),
        Attrs::new(false, false, true),
    );

    let result = ctx.alloc_obj(JsObject::default());
    ctx.define_data_prop_with_attrs(result, "proxy", proxy_v, Attrs::DEFAULT);
    ctx.define_data_prop_with_attrs(result, "revoke", JsValue::object(revoke), Attrs::DEFAULT);
    Ok(JsValue::object(result))
}

/// Body of a proxy revocation function (a `FunctionTarget::Native`).
///
/// `this` is the proxy, delivered by the `FunctionTarget::Bound` wrapper.
/// Clearing both slots is the whole of revocation in this phase; a later
/// phase turns every trapped operation on a revoked proxy into a `TypeError`.
fn revoke_proxy(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
    if let Some(proxy) = this.as_object() {
        let obj = heap.get_mut(proxy);
        if obj.kind == Kind::Proxy {
            obj.proxy_target = None;
            obj.proxy_handler = None;
        }
    }
    Ok(JsValue::undefined())
}
