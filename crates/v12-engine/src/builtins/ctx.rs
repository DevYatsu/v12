//! Phase 3 step 1: canonical `Ctx` conversions (see `docs/builtins-arch-plan.md` §4.1+§5 step 3).
//!
//! `Ctx` owns the conversion logic (`require_object_coercible`,
//! `this_object`, `to_number`, `to_string`, `string_text`); the free
//! functions in `helpers` are thin shims delegating here so existing
//! `fn(&mut Heap, …)` bodies keep compiling until their file migrates.

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

    // -- conv (canonical implementations; `helpers` shims delegate here) -----

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

    /// Checked receiver object for `method`: `TypeError` naming `method`
    /// when `this` is not an object or not of `kind` (when given).
    pub fn this_object(
        &mut self,
        v: JsValue,
        method: &str,
        kind: Option<v12_heap::Kind>,
    ) -> Result<Handle<JsObject>, Throw> {
        let Some(obj) = v.as_object() else {
            return Err(Throw::type_error(
                &mut *self.heap,
                format!("TypeError: {method} called on non-object"),
            ));
        };
        if let Some(kind) = kind
            && self.heap.get(obj).kind != kind
        {
            return Err(Throw::type_error(
                &mut *self.heap,
                format!("TypeError: {method} called on non-{kind:?}"),
            ));
        }
        Ok(obj)
    }

    /// ES `ToNumber` subset: Smi/double pass through; `true`→1.0,
    /// `false`/`null`→0.0, `undefined`→NaN; a string is trimmed
    /// (empty→0.0, else parsed as f64, failure→NaN); objects → NaN.
    pub fn to_number(&mut self, v: JsValue) -> f64 {
        if let Some(n) = v.as_smi().map(f64::from) {
            return n;
        }
        if let Some(n) = v.as_f64() {
            return n;
        }
        if v.is_true() {
            return 1.0;
        }
        if v.is_false() || v.is_null() {
            return 0.0;
        }
        if let Some(h) = v.as_string() {
            let text = self.string_text(h);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return 0.0;
            }
            return trimmed.parse::<f64>().unwrap_or(f64::NAN);
        }
        f64::NAN
    }

    /// String text of a value: strings render their text, real arrays
    /// render comma-joined elements, everything else renders the way
    /// `console.log` observes it (Tier-0 display subset).
    pub fn to_string(&mut self, v: JsValue) -> String {
        if let Some(obj) = v.as_object()
            && self.heap.get(obj).kind == v12_heap::Kind::Array
        {
            return Self::array_join_text(self.heap, obj, 0);
        }
        if let Some(h) = v.as_string() {
            return self.string_text(h);
        }
        Self::display_text(v)
    }

    /// String text of a heap string, flattened and lossy-converted.
    pub fn string_text(&mut self, h: Handle<v12_heap::V12Str>) -> String {
        self.heap.flatten(h);
        match &self.heap.get(h).storage {
            v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
            _ => String::new(),
        }
    }

    /// Comma-joined element text of a real array (`undefined`/`null`/holes
    /// render empty, matching `Array.prototype.join`). Nested arrays
    /// recurse; `depth` caps the recursion so cyclic arrays terminate.
    fn array_join_text(
        heap: &mut Heap,
        obj: Handle<JsObject>,
        depth: usize,
    ) -> String {
        if depth > 8 {
            return String::new();
        }
        // Snapshot before formatting: rendering an element may allocate (and
        // thus collect), invalidating a live borrow of the element store.
        let elements: Vec<JsValue> = heap.get(obj).elements_snapshot();
        let mut parts = Vec::with_capacity(elements.len());
        for v in elements {
            if v.is_undefined() || v.is_null() || v.is_hole() {
                parts.push(String::new());
            } else if let Some(nested) = v
                .as_object()
                .filter(|h| heap.get(*h).kind == v12_heap::Kind::Array)
            {
                parts.push(Self::array_join_text(heap, nested, depth + 1));
            } else if let Some(h) = v.as_string() {
                Self::string_text_of(heap, h, &mut parts);
            } else {
                parts.push(Self::display_text(v));
            }
        }
        parts.join(",")
    }

    /// Pushes the flattened text of `h` onto `parts` (array-join helper).
    fn string_text_of(
        heap: &mut Heap,
        h: Handle<v12_heap::V12Str>,
        parts: &mut Vec<String>,
    ) {
        heap.flatten(h);
        let text = match &heap.get(h).storage {
            v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
            _ => String::new(),
        };
        parts.push(text);
    }

    /// `console.log`-style display text for a non-string value (Tier-0 subset).
    fn display_text(v: JsValue) -> String {
        if let Some(number) = v.as_smi().map(f64::from).or(v.as_f64()) {
            if number.is_nan() {
                return "NaN".to_string();
            }
            if number == f64::INFINITY {
                return "Infinity".to_string();
            }
            if number == f64::NEG_INFINITY {
                return "-Infinity".to_string();
            }
            return format!("{number}");
        }
        if v.is_true() {
            return "true".to_string();
        }
        if v.is_false() {
            return "false".to_string();
        }
        if v.is_undefined() {
            return "undefined".to_string();
        }
        if v.is_null() {
            return "null".to_string();
        }
        if v.is_object() {
            return "[object Object]".to_string();
        }
        "<unprintable>".to_string()
    }

    // -- props (unified install family, plan §3) ---------------------------

    /// Shape-descriptor install of one data property with explicit attrs.
    /// Single canonical path: every install in the workspace funnels here.
    pub fn define_data_prop_with_attrs(
        &mut self,
        obj: Handle<JsObject>,
        name: &str,
        value: JsValue,
        attrs: v12_heap::Attrs,
    ) {
        use v12_heap::{PropKey, V12Str};
        let h = if name.is_ascii() {
            self.heap.intern_string(V12Str::latin1_slice(name.as_bytes()))
        } else {
            self.heap
                .intern_string(V12Str::utf16(name.encode_utf16().collect()))
        };
        let key = PropKey::from_string(h);
        let shape = self.heap.shape_of_mut(obj);
        debug_assert!(
            self.heap.lookup_property(shape, key).is_none(),
            "duplicate install of `{name}`"
        );
        let child = self.heap.add_property(shape, key, attrs);
        self.heap.bind_shape(obj, child);
        // NOTE: no `properties.len() == property_keys.len()` assert here. The
        // global's intrinsic prefix (`realm.rs`: 18 values pushed with no
        // shape/keys) means the two vecs are NOT in lockstep on pre-existing
        // objects — documented drift (plan §7(a)), not something installs may
        // assume. `add_property` assigns the descriptor slot; the push below
        // keeps the shape-bound suffix aligned.
        self.heap.get_mut(obj).properties.push(value);
        self.heap.get_mut(obj).property_keys.push(Some(key));
        // Enforcement (plan §3.4): the fresh descriptor is present with the
        // declared attrs. (Slot-vs-storage-index lockstep holds only on fresh
        // objects; the global intrinsic prefix breaks it, so no index assert.)
        let bound = self.heap.shape_of(obj);
        let desc = self
            .heap
            .lookup_property(bound, key)
            .copied()
            .expect("freshly installed key must resolve");
        debug_assert_eq!(
            desc.attrs(),
            attrs,
            "installed attrs for `{name}` must match declaration"
        );
    }

#[allow(dead_code)]
    /// Duplicate-install guard helper (kept for callers; the live check is
    /// the `lookup_property` debug_assert above).
    fn has_own_key(heap: &Heap, obj: Handle<JsObject>, _name: &str) -> bool {
        let _ = (heap, obj);
        false
    }

    /// Shape-descriptor install of one data property (spec `BUILTIN` attrs).
    pub fn define_data_prop(
        &mut self,
        obj: Handle<JsObject>,
        name: &str,
        value: JsValue,
    ) {
        self.define_data_prop_with_attrs(obj, name, value, v12_heap::Attrs::BUILTIN);
    }

    /// Allocates a `Kind::Function` for `id`, stamps spec `length` + `name`
    /// own props, and installs it as `name` on `obj`.
    ///
    /// `length` is `None` for legacy entries that have not declared an arity
    /// yet (no `length` prop installed — current observable behavior). Once
    /// an entry declares `(len)` in `define_builtins!`, the prop is installed
    /// with `{ writable: false, enumerable: false, configurable: true }`.
    pub fn define_method(
        &mut self,
        obj: Option<Handle<JsObject>>,
        name: &str,
        id: v12_native::NativeId,
        length: Option<u32>,
    ) {
        let Some(target) = obj else { return };
        let func = self.alloc_obj(v12_heap::JsObject {
            kind: v12_heap::Kind::Function,
            callable: v12_heap::FunctionTarget::Bytecode(u32::from(id)),
            ..Default::default()
        });
        if let Some(len) = length {
            let len_v = JsValue::from_i32_smi(len as i32).expect("arity fits Smi");
            // length: { writable: false, enumerable: false, configurable: true }
            self.define_data_prop_with_attrs(
                func,
                "length",
                len_v,
                v12_heap::Attrs::new(false, false, true),
            );
            debug_assert_eq!(
                super::builtin_length(id),
                Some(len),
                "installed length must match declared arity"
            );
            // Enforcement (plan §3.4): `length` own-prop present with spec
            // attrs and the declared value.
            self.assert_own_data_value(func, "length", len_v);
        }
        // name: { writable: false, enumerable: false, configurable: true }
        let name_h = self.heap.intern_text(name);
        self.define_data_prop_with_attrs(
            func,
            "name",
            JsValue::string(name_h),
            v12_heap::Attrs::new(false, false, true),
        );
        // Enforcement (plan §3.4): `name` own-prop present with spec attrs
        // and the installed value.
        self.assert_own_data_value(func, "name", JsValue::string(name_h));
        self.define_data_prop(target, name, JsValue::object(func));
    }

    /// Debug-only check (plan §3.4): `obj` has an own data descriptor for
    /// `name` whose stored value is `expected`. Fresh builtin function objects
    /// are storage-lockstep (one push per descriptor), so the descriptor slot
    /// indexes `properties` directly.
    fn assert_own_data_value(
        &mut self,
        obj: Handle<JsObject>,
        name: &str,
        expected: JsValue,
    ) {
        use v12_heap::{PropKey, V12Str};
        let probe = if name.is_ascii() {
            self.heap.intern_string(V12Str::latin1_slice(name.as_bytes()))
        } else {
            self.heap
                .intern_string(V12Str::utf16(name.encode_utf16().collect()))
        };
        let shape = self.heap.shape_of(obj);
        let slot = match self.heap.lookup_property(shape, PropKey::from_string(probe)) {
            Some(v12_heap::Descriptor::Data { slot, .. }) => *slot as usize,
            other => panic!("expected own data prop `{name}`, found {other:?}"),
        };
        debug_assert_eq!(
            self.heap.get(obj).properties.get(slot),
            Some(&expected),
            "installed `{name}` value must match declaration"
        );
    }

    /// Constructor/prototype linkage for an already-materialized pair
    /// (realm placeholders): sets the `prototype` field, installs
    /// `ctor.prototype` (non-writable, non-configurable) and the
    /// `proto.constructor` back-link (`BUILTIN` attrs). Both directions are
    /// expected rooted by the caller (realm roots hold them).
    pub fn install_ctor_link(
        &mut self,
        ctor: Handle<JsObject>,
        proto: Handle<JsObject>,
    ) {
        use v12_heap::{PropKey, V12Str};
        self.heap.get_mut(ctor).prototype = Some(proto);
        // Idempotent: the Promise path wires before the generic loop, so
        // skip the `prototype` install when already present.
        let probe = self.heap.intern_string(V12Str::latin1_slice(b"prototype"));
        let shape = self.heap.shape_of_mut(ctor);
        if self
            .heap
            .lookup_property(shape, PropKey::from_string(probe))
            .is_none()
        {
            self.define_data_prop_with_attrs(
                ctor,
                "prototype",
                JsValue::object(proto),
                v12_heap::Attrs::new(false, false, false),
            );
        }
        let proto_shape = self.heap.shape_of_mut(proto);
        let ctor_probe = self.heap.intern_string(V12Str::latin1_slice(b"constructor"));
        if self
            .heap
            .lookup_property(proto_shape, PropKey::from_string(ctor_probe))
            .is_none()
        {
            self.define_data_prop(proto, "constructor", JsValue::object(ctor));
        }
        debug_assert_eq!(self.heap.get(ctor).prototype, Some(proto));
        // Enforcement (plan §3.4): `ctor.prototype` is spec non-writable,
        // non-configurable; the `constructor` back-link is `BUILTIN`.
        let ctor_shape = self.heap.shape_of(ctor);
        let proto_key =
            PropKey::from_string(self.heap.intern_string(V12Str::latin1_slice(b"prototype")));
        let proto_desc = self
            .heap
            .lookup_property(ctor_shape, proto_key)
            .copied()
            .expect("ctor must carry an own `prototype` prop");
        debug_assert_eq!(
            proto_desc.attrs(),
            v12_heap::Attrs::new(false, false, false),
            "ctor `prototype` must be non-writable, non-configurable"
        );
        let back_shape = self.heap.shape_of(proto);
        let back_key =
            PropKey::from_string(self.heap.intern_string(V12Str::latin1_slice(b"constructor")));
        let back_desc = self
            .heap
            .lookup_property(back_shape, back_key)
            .copied()
            .expect("prototype must carry an own `constructor` back-link");
        debug_assert_eq!(
            back_desc.attrs(),
            v12_heap::Attrs::BUILTIN,
            "`constructor` back-link must use BUILTIN attrs"
        );
    }

    /// Realm helper for per-call rest-array identity (plan §5 step 4): today
    /// this only resolves the `Array` constructor value via the
    /// `GLOBAL_INTRINSICS` slot reader, so call sites stop hardcoding the
    /// raw slot-1 index read. Full move-to-construction-time is deferred (see
    /// `alloc_rest_array`): the own `constructor` install needs shape
    /// machinery owned by the interpreter.
    pub fn array_ctor_value(&self) -> Option<JsValue> {
        self.intrinsic("Array")
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

/// Forward adapter: invokes a migrated `BuiltinFn` (`fn(&mut Ctx, …)`) from a
/// legacy `&mut Heap` dispatch site. Builds a detached `Ctx` (no global, no
/// pending sink) for pure builtins like `math.rs` that need no realm or job
/// context, so `builtin_dispatch` arms can route through the `Ctx` seam
/// without changing the dispatch signature.
pub fn call_ctx(
    handler: BuiltinFn,
    heap: &mut Heap,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let mut ctx = Ctx::new(heap, None, None);
    handler(&mut ctx, this, args)
}
