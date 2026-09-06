//! Phase 2: `Ctx` context + adapter shim (see `docs/builtins-arch-plan.md` §5 step 2).
//!
//! `Ctx` is the sole context a builtin receives. For now it wraps `&mut Heap`
//! plus the realm global, the pending-job sink, and the regexp cache handle,
//! and every accessor/conversion/error/prop helper delegates to the existing
//! free helpers. Per-file body migrations (§5 step 3+) replace direct heap
//! pokes with these methods later; no dispatch arms or install paths change
//! in this phase.

use std::cell::RefCell;
use std::rc::Rc;

use v12_heap::{Handle, Heap, JsObject, JsValue};
use v12_native::Throw;

use super::helpers;
use crate::job_queue::Job;

/// Canonical builtin signature (plan §1.2). Bodies migrate to this in §5 step 3+.
pub type BuiltinFn = fn(&mut Ctx, JsValue, &[JsValue]) -> Result<JsValue, Throw>;

/// Exclusive builtin context: heap borrow plus realm readers and sinks.
pub struct Ctx<'a> {
    /// Exclusive heap borrow (never shared).
    pub heap: &'a mut Heap,
    /// Realm global handle (intrinsic-slot reader root).
    pub global: Option<Handle<JsObject>>,
    /// Job-enqueue side channel shared with the engine's job queue.
    pub pending: Option<Rc<RefCell<Vec<Job>>>>,
}

impl<'a> Ctx<'a> {
    /// Wraps the heap borrow with optional realm context.
    pub fn new(
        heap: &'a mut Heap,
        global: Option<Handle<JsObject>>,
        pending: Option<Rc<RefCell<Vec<Job>>>>,
    ) -> Self {
        Self {
            heap,
            global,
            pending,
        }
    }

    // -- realm / intrinsic readers ---------------------------------------

    /// Realm global handle, when the caller supplied one.
    #[must_use]
    pub fn global(&self) -> Option<Handle<JsObject>> {
        self.global
    }

    /// Reads a realm intrinsic by its `GLOBAL_INTRINSICS` name (never a
    /// hardcoded `properties.get(idx)` at the call site).
    #[must_use]
    pub fn intrinsic(&self, name: &str) -> Option<JsValue> {
        let global = self.global?;
        let idx = v12_bytecode::GLOBAL_INTRINSICS
            .iter()
            .position(|&n| n == name)?;
        self.heap.get(global).properties.get(idx).copied()
    }

    // -- roots ------------------------------------------------------------

    /// Allocates an object and roots it (delegates to `helpers::alloc_obj`).
    pub fn alloc_obj(&mut self, obj: JsObject) -> Handle<JsObject> {
        helpers::alloc_obj(self.heap, obj)
    }

    /// Roots a value for the duration of the builtin.
    pub fn add_root(&mut self, value: JsValue) {
        self.heap.add_root(value);
    }

    /// Enqueues a follow-up job on the pending sink (no-op without one).
    pub fn enqueue_job(&self, job: Job) {
        if let Some(pending) = &self.pending {
            pending.borrow_mut().push(job);
        }
    }

    // -- errors (stubs delegating to `Throw` for now) ----------------------

    /// Real `TypeError` object once §4.2 lands; today `Throw::type_error`.
    pub fn type_error(&mut self, msg: impl AsRef<str>) -> Throw {
        Throw::type_error(&mut *self.heap, msg)
    }

    /// Stub: routes through `type_error` until §4.2 wires real error kinds.
    pub fn range_error(&mut self, msg: impl AsRef<str>) -> Throw {
        Throw::type_error(&mut *self.heap, msg)
    }

    /// Stub: routes through `type_error` until §4.2 wires real error kinds.
    pub fn syntax_error(&mut self, msg: impl AsRef<str>) -> Throw {
        Throw::type_error(&mut *self.heap, msg)
    }

    /// Stub: routes through `type_error` until §4.2 wires real error kinds.
    pub fn reference_error(&mut self, msg: impl AsRef<str>) -> Throw {
        Throw::type_error(&mut *self.heap, msg)
    }

    // -- conv (stubs delegating to `helpers` for now) ----------------------

    /// Throws `TypeError` on `undefined`/`null`, else returns the value.
    pub fn require_object_coercible(&mut self, v: JsValue) -> Result<JsValue, Throw> {
        if v.is_undefined() || v.is_null() {
            return Err(Throw::type_error(
                &mut *self.heap,
                "TypeError: value is not object-coercible",
            ));
        }
        Ok(v)
    }

    /// Checked receiver object for `method` (delegates to `helpers::as_object`).
    pub fn this_object(
        &mut self,
        v: JsValue,
        method: &str,
        kind: Option<v12_heap::Kind>,
    ) -> Result<Handle<JsObject>, Throw> {
        helpers::as_object(self.heap, v, method, kind)
    }

    /// ES `ToNumber` subset (delegates to `helpers::to_number`).
    pub fn to_number(&mut self, v: JsValue) -> f64 {
        helpers::to_number(self.heap, v)
    }

    /// String text of a value (delegates to `helpers::value_text`).
    pub fn to_string(&mut self, v: JsValue) -> String {
        helpers::value_text(self.heap, v)
    }

    /// String text of a heap string (delegates to `helpers::string_text`).
    pub fn string_text(&mut self, h: Handle<v12_heap::V12Str>) -> String {
        helpers::string_text(self.heap, h)
    }

    // -- props (stubs delegating to the install helpers for now) -----------

    /// Shape-descriptor install of one data property (delegates to
    /// `builtin_install_prop`).
    pub fn define_data_prop(
        &mut self,
        obj: Handle<JsObject>,
        name: &str,
        value: JsValue,
    ) {
        super::builtin_install_prop(self.heap, obj, name, value);
    }

    /// Allocates a `Kind::Function` for `id` and installs it as `name`.
    pub fn define_method(
        &mut self,
        obj: Option<Handle<JsObject>>,
        name: &str,
        id: v12_native::NativeId,
    ) {
        super::install_native(self.heap, obj, name, id);
    }
}

/// Adapter shim: invokes a legacy `NativeHandler` (`fn(&mut Heap, …)`) with
/// the heap borrowed out of a `Ctx`, so existing bodies compile unchanged
/// while every call site goes through the `Ctx` seam.
pub fn call_legacy(
    handler: super::registry::NativeHandler,
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    handler(ctx.heap, this, args)
}
