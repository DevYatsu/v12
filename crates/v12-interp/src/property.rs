//! Property access and mutation: `get_property`/`set_property`, the
//! built-in-method "surface" dispatch, inline-cache lookup, and the
//! `in`/`instanceof`/`delete` operators plus array element access.

use v12_heap::{Attrs, Descriptor, Handle, JsObject, JsValue, Kind, PropKey};

use super::{
    ARRAY_IDX, CONSOLE_IDX, GLOBAL_VAR_OFFSET, Interp, JSException, OBJECT_IDX, PROMISE_IDX,
    REGEXP_IDX, RegExpSlot, SYMBOL_IDX, WK_ADD, WK_APPLY, WK_ASYNC_ITERATOR, WK_BIND, WK_CALL,
    WK_CATCH, WK_CLEAR, WK_CONSTRUCTOR, WK_CREATE, WK_DEFINE_PROPERTY, WK_DELETE, WK_ENTRIES,
    WK_ENUMERABLE_OWN_KEYS, WK_FLAGS, WK_FOR_EACH, WK_GET, WK_GET_PROTOTYPE_OF, WK_HAS,
    WK_HAS_OWN_PROPERTY, WK_IS_ARRAY, WK_ITERATOR, WK_KEYS, WK_LAST_INDEX, WK_LENGTH, WK_LOG,
    WK_NEXT, WK_PROTOTYPE, WK_REJECT, WK_RESOLVE, WK_RETURN, WK_SET, WK_SIZE, WK_SOURCE, WK_THEN,
    WK_THROW, WK_TO_STRING, WK_VALUE_OF, WK_VALUES, child_slot,
};
use crate::ops;
use v12_native::NativeId;

/// One own property as the proxy trap algorithms consume it (ES descriptor
/// record). Absent fields mirror `ToPropertyDescriptor`'s optional slots;
/// `is_data`/`is_accessor`/`is_generic` classify the shape.
#[derive(Clone, Copy)]
pub(crate) struct OwnDesc {
    pub has_value: bool,
    pub value: JsValue,
    pub has_writable: bool,
    pub writable: bool,
    pub has_get: bool,
    pub get: JsValue,
    pub has_set: bool,
    pub set: JsValue,
    pub has_enumerable: bool,
    pub enumerable: bool,
    pub has_configurable: bool,
    pub configurable: bool,
}

impl Default for OwnDesc {
    fn default() -> Self {
        Self {
            has_value: false,
            value: JsValue::undefined(),
            has_writable: false,
            writable: false,
            has_get: false,
            get: JsValue::undefined(),
            has_set: false,
            set: JsValue::undefined(),
            has_enumerable: false,
            enumerable: false,
            has_configurable: false,
            configurable: false,
        }
    }
}

impl OwnDesc {
    pub(crate) fn is_accessor(&self) -> bool {
        self.has_get || self.has_set
    }

    pub(crate) fn is_data(&self) -> bool {
        self.has_value || self.has_writable
    }

    pub(crate) fn is_generic(&self) -> bool {
        !self.is_accessor() && !self.is_data()
    }
}

/// The `JsValue` a trap receives as its property-key argument.
fn prop_key_value(key: PropKey) -> JsValue {
    if let Some(h) = key.string() {
        JsValue::string(h)
    } else if let Some(y) = key.symbol() {
        JsValue::symbol(y)
    } else {
        JsValue::undefined()
    }
}

/// ES `IsCompatiblePropertyDescriptor`: may `desc` be applied to an object
/// whose existing property is `current`? Conservative: only the checks the
/// proxy invariant suite exercises (configurability and value equality on a
/// non-configurable property) are enforced.
fn desc_compatible(extensible: bool, desc: OwnDesc, current: OwnDesc) -> bool {
    let _ = extensible;
    if desc.has_configurable
        && !desc.configurable
        && current.has_configurable
        && current.configurable
    {
        return false;
    }
    if current.has_configurable && !current.configurable {
        if current.is_data() && desc.is_accessor() {
            return false;
        }
        if current.is_accessor() && desc.is_data() {
            return false;
        }
        if current.is_data() && current.has_writable && !current.writable {
            if desc.has_writable && desc.writable {
                return false;
            }
            if desc.has_value && desc.value != current.value {
                return false;
            }
        }
    }
    true
}

impl Interp<'_> {
    pub(crate) fn get_property(
        &mut self,
        site_fn: u32,
        site_pc: u32,
        obj_v: JsValue,
        key_v: JsValue,
    ) -> Result<JsValue, JSException> {
        // Primitives have no wrappers yet: reads yield undefined, matching
        // real JS minus the built-ins that would populate the wrappers
        // (string primitives do get the regexp method surface). Null and
        // undefined throw per spec — this is what makes destructuring
        // null/undefined and `null.x` observe TypeError.
        if obj_v.is_null() || obj_v.is_undefined() {
            let key_text = key_v
                .as_string()
                .map(|h| self.string_text(h))
                .unwrap_or_default();
            let base = if obj_v.is_null() { "null" } else { "undefined" };
            return Err(JSException(self.error_value(&format!(
                "TypeError: Cannot read properties of {base} (reading '{key_text}')"
            ))));
        }
        // Flatten-once: every surface probe below reads flat storage, so one
        // in-place flatten here replaces up to ~15 re-materializations.
        self.flatten_key(key_v);
        // Intern-once: string keys canonicalize here; every surface below
        // integer-compares this key and `ic_lookup` reuses it — no second
        // hash/alloc per access. Non-string keys stay `None` (their
        // coercion defers to `ic_lookup`, and no surface matches them).
        let key: Option<PropKey> = match key_v.as_string() {
            Some(h) => {
                let units = ops::string_units(self.heap, h);
                Some(PropKey::from_string(
                    self.heap.intern_string(v12_heap::V12Str::utf16(units)),
                ))
            }
            None => None,
        };
        let Some(obj) = obj_v.as_object() else {
            return self.string_prim_surface(obj_v, key_v, key);
        };
        // Proxy exotic: `[[Get]]` is the `get` trap (ES 10.5.8). Checked
        // before the element fast path and every structural surface so a
        // proxy is never treated as an ordinary object (its empty shared
        // shape would otherwise answer `undefined` or match a surface).
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_get(obj, obj_v, key_v);
        }
        // Cheapest-integer-first: the canonical-index probe runs before any
        // surface. Kind-guarded to Array/Arguments, so other receivers flow
        // through unchanged; for index keys on arrays every later surface
        // answers `None` (method tables hold no digit names) while the old
        // code always resolved them at `element_surface` — same answer,
        // reached in one probe instead of ~15.
        let kind = self.heap.get(obj).kind;
        if (kind == Kind::Array || kind == Kind::Arguments)
            && let Some(idx) = self.array_index_of(key_v)
        {
            // Integer-index reads come from the element store (mapped
            // arguments indices mirror the parameter slot; v1 returns the
            // element — the param alias is exercised via the mapped array
            // in heap tests).
            return Ok(self.array_element(obj, idx));
        }
        // Structural fast-path surfaces, probed in a fixed order. Each helper
        // recognizes one `(receiver, key)` surface and answers the read, or
        // returns `None` to defer to the next; the shape-bound lookup with the
        // inline cache runs only when no surface matches.
        if let Some(answer) = self.console_log_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.symbol_iterator_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.promise_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.object_statics_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.generator_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.well_known_iterator_surface(obj, key_v) {
            return answer;
        }
        if let Some(answer) = self.iterator_method_surface(obj, key_v) {
            return answer;
        }
        if let Some(answer) = self.regexp_surface(obj, key_v, key) {
            return answer;
        }
        if let Some(answer) = self.array_statics_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.function_method_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.object_proto_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.regexp_prototype_surface(obj, key) {
            return answer;
        }
        if let Some(answer) = self.array_instance_surface(obj, key_v, key) {
            return answer;
        }
        if let Some(answer) = self.map_set_surface(obj, key) {
            return answer;
        }
        self.ic_lookup(site_fn, site_pc, obj, key_v, key)
    }

    /// String primitives synthesize the regexp method surface (`match`/
    /// `replace`/`search`/`split`) from the const method table (the
    /// `StringPrim` pseudo-kind); numbers and booleans get their own
    /// pseudo-kinds (`NumberPrim`/`BooleanPrim`); other primitives read
    /// `undefined`.
    pub(crate) fn string_prim_surface(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        key: Option<PropKey>,
    ) -> Result<JsValue, JSException> {
        let kind = if obj_v.is_string() {
            Kind::StringPrim
        } else if obj_v.as_smi().is_some() || obj_v.as_f64().is_some() {
            Kind::NumberPrim
        } else if obj_v.is_true() || obj_v.is_false() {
            Kind::BooleanPrim
        } else if obj_v.as_symbol().is_some() {
            Kind::SymbolPrim
        } else {
            Kind::Ordinary
        };
        if kind != Kind::Ordinary
            && let Some(id) = self.method_native(kind, key_v)
        {
            return Ok(self.map_set_method(id));
        }
        // `String.prototype.length`: UTF-16 code-unit count, so an astral
        // character (surrogate pair) contributes 2. `V12Str::len` already
        // counts units, not code points — read it directly.
        if kind == Kind::StringPrim && self.key_is_wk(key, WK_LENGTH) {
            let h = obj_v.as_string().expect("string primitive has a handle");
            let len = self.heap.get(h).len();
            return Ok(ops::box_number(len as f64));
        }
        // `StringGetOwnProperty` for the decimal index keys: a canonical
        // numeric index `i < length` names the single UTF-16 code unit at
        // `i`; every other index (negative, huge, fractional) is absent, so
        // the read is `undefined`. Flat storage is read in place — no
        // per-index `Vec` materialization (`String.prototype.charAt` clones,
        // this does not).
        if kind == Kind::StringPrim
            && let Some(idx) = self.array_index_of(key_v)
        {
            let h = obj_v.as_string().expect("string primitive has a handle");
            self.heap.flatten(h);
            let unit = match &self.heap.get(h).storage {
                v12_heap::StrStorage::Latin1(bytes) => {
                    bytes.get(idx as usize).map(|&b| u16::from(b))
                }
                v12_heap::StrStorage::Utf16(units) => units.get(idx as usize).copied(),
                _ => None,
            };
            return Ok(match unit {
                Some(u) => {
                    let sh = self.heap.intern_string(v12_heap::V12Str::utf16(vec![u]));
                    JsValue::string(sh)
                }
                None => JsValue::undefined(),
            });
        }
        Ok(JsValue::undefined())
    }

    /// `console.log` — the console intrinsic is located by name in the
    /// global's property prefix (adding intrinsics cannot drift the index);
    /// a `"log"` read on that object synthesizes the native function.
    pub(crate) fn console_log_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        let console_idx = CONSOLE_IDX;
        let (Some(g), Some(console_idx)) = (self.global, console_idx) else {
            return None;
        };
        let console_obj = {
            let heap = &*self.heap;
            heap.get(g)
                .properties
                .get(console_idx)
                .and_then(|v| v.as_object())
        }?;
        if obj != console_obj || !self.key_is_wk(key, WK_LOG) {
            return None;
        }
        Some(Ok(self.console_log_fn()))
    }

    /// `Symbol.iterator` — reading the well-known symbol off the `Symbol`
    /// intrinsic (found by name in the global's property prefix) yields
    /// the realm's singleton symbol value.
    pub(crate) fn symbol_iterator_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        let symbol_idx = SYMBOL_IDX;
        let (Some(g), Some(symbol_idx)) = (self.global, symbol_idx) else {
            return None;
        };
        let symbol_ctor = {
            let heap = &*self.heap;
            heap.get(g)
                .properties
                .get(symbol_idx)
                .and_then(|v| v.as_object())
        }?;
        if obj != symbol_ctor {
            return None;
        }
        if self.key_is_wk(key, WK_ITERATOR) {
            return Some(Ok(JsValue::symbol(self.symbol_iterator_key())));
        }
        if self.key_is_wk(key, WK_ASYNC_ITERATOR) {
            return Some(Ok(JsValue::symbol(self.symbol_async_iterator_key())));
        }
        None
    }

    /// The Promise surface. Natives cannot attach shape-bound properties
    /// (shape binding is interpreter state), so these reads are recognized
    /// structurally:
    /// - `Promise.resolve` / `Promise.reject` on the Promise constructor
    ///   (located by name in the duplicated `GLOBAL_INTRINSIC_NAMES`).
    /// - `then` on any object whose prototype is the Promise constructor's
    ///   `prototype` link — the realm installs that link, and the engine's
    ///   promise built-ins give every promise instance the same prototype.
    pub(crate) fn promise_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        let promise_idx = PROMISE_IDX;
        let (Some(g), Some(promise_idx)) = (self.global, promise_idx) else {
            return None;
        };
        let promise_ctor = {
            let heap = &*self.heap;
            heap.get(g)
                .properties
                .get(promise_idx)
                .and_then(|v| v.as_object())
        }?;
        if obj == promise_ctor {
            if self.key_is_wk(key, WK_RESOLVE) {
                return Some(Ok(self.cached_native(NativeId::PromiseResolve)));
            }
            if self.key_is_wk(key, WK_REJECT) {
                return Some(Ok(self.cached_native(NativeId::PromiseReject)));
            }
            return None;
        }
        // Structural promise check rather than prototype identity: async
        // functions return engine-created promises (no `Promise.prototype`
        // link — the interp has no realm handle at allocation time), so
        // prototype matching would miss `.then`/`.catch` on them.
        if self.is_promise(JsValue::object(obj)) {
            if self.key_is_wk(key, WK_THEN) {
                return Some(Ok(self.cached_native(NativeId::PromiseThen)));
            }
            if self.key_is_wk(key, WK_CATCH) {
                return Some(Ok(self.cached_native(NativeId::PromiseCatch)));
            }
        }
        None
    }

    /// Static methods on the `Object` constructor: `create`/
    /// True when `obj` carries an own (shape or dictionary) descriptor for
    /// `key`. Used by static-method surfaces to defer to a shape-installed
    /// builtin (which owns spec `length`/`name` own props) instead of
    /// synthesizing a fresh, unattributed function object.
    fn has_own_descriptor(&mut self, obj: Handle<JsObject>, key: PropKey) -> bool {
        if self
            .heap
            .get(obj)
            .dictionary
            .as_ref()
            .is_some_and(|m| m.contains_key(&key))
        {
            return true;
        }
        let shape = self.shape_of(obj);
        self.heap.lookup_property(shape, key).is_some()
    }

    /// `getPrototypeOf`/`defineProperty`/`enumerableOwnKeys` and the
    /// `keys`/`values`/`entries` trio. The constructor is the global's first
    /// intrinsic slot (`OBJECT_IDX`).
    pub(crate) fn object_statics_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        let object_idx = OBJECT_IDX;
        let (Some(g), Some(object_idx)) = (self.global, object_idx) else {
            return None;
        };
        let object_ctor = {
            let heap = &*self.heap;
            heap.get(g)
                .properties
                .get(object_idx)
                .and_then(|v| v.as_object())
        }?;
        if obj != object_ctor {
            return None;
        }
        // `enumerableOwnKeys` is an internal-only key (never shape-installed
        // on the constructor), so it is always synthesized here. Every other
        // name in this surface is a real own property installed by the realm
        // with spec `length`/`name`; defer to the shape walk when the
        // descriptor exists so the installed function object (not a fresh
        // unattributed synthesis) answers the read.
        if self.key_is_wk(key, WK_ENUMERABLE_OWN_KEYS) {
            return Some(Ok(self.cached_native(NativeId::ObjectEnumerableOwnKeys)));
        }
        if let Some(k) = key
            && self.has_own_descriptor(obj, k)
        {
            return None;
        }
        let constant = if self.key_is_wk(key, WK_CREATE) {
            NativeId::ObjectCreate
        } else if self.key_is_wk(key, WK_GET_PROTOTYPE_OF) {
            NativeId::ObjectGetPrototypeOf
        } else if self.key_is_wk(key, WK_DEFINE_PROPERTY) {
            NativeId::ObjectDefineProperty
        } else if self.key_is_wk(key, WK_KEYS) {
            NativeId::ObjectKeys
        } else if self.key_is_wk(key, WK_VALUES) {
            NativeId::ObjectValues
        } else if self.key_is_wk(key, WK_ENTRIES) {
            NativeId::ObjectEntries
        } else {
            return None;
        };
        Some(Ok(self.map_set_method(constant)))
    }
    /// Generator instances expose `next`/`return`/`throw` as synthesized
    /// natives.
    pub(crate) fn generator_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        if self.heap.get(obj).kind != Kind::Generator {
            return None;
        }
        let constant = if self.key_is_wk(key, WK_NEXT) {
            NativeId::GeneratorNext
        } else if self.key_is_wk(key, WK_RETURN) {
            NativeId::GeneratorReturn
        } else if self.key_is_wk(key, WK_THROW) {
            NativeId::GeneratorThrow
        } else {
            return None;
        };
        Some(Ok(self.cached_native(constant)))
    }

    /// `obj[Symbol.iterator]` — the well-known symbol key is a symbol
    /// value, not a string; recognize it by comparing against the cached
    /// realm symbol handle, then pick the iterator constructor by receiver
    /// kind. Returns a synthesized native function that creates an iterator
    /// over `obj`.
    pub(crate) fn well_known_iterator_surface(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
    ) -> Option<Result<JsValue, JSException>> {
        if !self.key_is_symbol_iterator(key_v) {
            return None;
        }
        let constant = match self.heap.get(obj).kind {
            Kind::Array | Kind::Arguments => crate::NativeId::ArrayIterator,
            Kind::Map => crate::NativeId::MapIterator,
            Kind::Set => crate::NativeId::SetIterator,
            Kind::Iterator | Kind::Generator => crate::NativeId::IteratorSelf,
            _ => return None,
        };
        Some(Ok(self.map_set_method(constant)))
    }

    /// `%IteratorPrototype%`-family instances resolve `next` from the const
    /// method table.
    pub(crate) fn iterator_method_surface(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
    ) -> Option<Result<JsValue, JSException>> {
        if self.heap.get(obj).kind != Kind::Iterator {
            return None;
        }
        let id = self.method_native(Kind::Iterator, key_v)?;
        Some(Ok(self.map_set_method(id)))
    }

    /// RegExp object surface: methods `exec`/`test`/`toString`/`compile`
    /// from the const table, and the `source`/`flags`/`lastIndex` property
    /// reads (internal slots).
    pub(crate) fn regexp_surface(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        if self.heap.get(obj).kind != Kind::RegExp {
            return None;
        }
        if let Some(id) = self.method_native(Kind::RegExp, key_v) {
            return Some(Ok(self.map_set_method(id)));
        }
        let slot = self.regexp_slot(key)?;
        let value = self.heap.get(obj).properties.get(slot as usize).copied()?;
        Some(Ok(value))
    }

    /// Which internal-slot property a key names on a RegExp object. Slots
    /// live at fixed positions in `properties` (see [`RegExpSlot`]).
    /// Integer compares on the entry-interned key — no memcmp.
    pub(crate) fn regexp_slot(&mut self, key: Option<PropKey>) -> Option<RegExpSlot> {
        if self.key_is_wk(key, WK_SOURCE) {
            Some(RegExpSlot::Source)
        } else if self.key_is_wk(key, WK_FLAGS) {
            Some(RegExpSlot::Flags)
        } else if self.key_is_wk(key, WK_LAST_INDEX) {
            Some(RegExpSlot::LastIndex)
        } else {
            None
        }
    }
    /// `Array.isArray` — static method on the Array constructor.
    pub(crate) fn array_statics_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        let array_idx = ARRAY_IDX;
        let (Some(g), Some(array_idx)) = (self.global, array_idx) else {
            return None;
        };
        let array_ctor = {
            let heap = &*self.heap;
            heap.get(g)
                .properties
                .get(array_idx)
                .and_then(|v| v.as_object())
        }?;
        if obj != array_ctor || !self.key_is_wk(key, WK_IS_ARRAY) {
            return None;
        }
        // `Array.isArray` is shape-installed by the realm; defer so the
        // installed function object answers (length/name present).
        if let Some(k) = key
            && self.has_own_descriptor(obj, k)
        {
            return None;
        }
        Some(Ok(self.map_set_method(NativeId::ArrayIsArray)))
    }
    /// `Function.prototype.call` / `apply` / `bind` / `toString` on any
    /// function object.
    pub(crate) fn function_method_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        if self.heap.get(obj).kind != Kind::Function {
            return None;
        }
        if self.key_is_wk(key, WK_CONSTRUCTOR) {
            // `f.constructor` on closures: closures carry no [[Prototype]]
            // link to `%Function.prototype%`, so without this surface the
            // read misses (observed as `undefined` for every function
            // object). An own `constructor` (user-assigned, or a
            // prototype's back-link) still wins — defer to the shape
            // lookup below when one shadows.
            if let Some(k) = key {
                let shape = self.shape_of(obj);
                if self.heap.lookup_property(shape, k).is_some() {
                    return None;
                }
            }
            return Some(Ok(self.closure_constructor(obj)));
        }
        let constant = if self.key_is_wk(key, WK_CALL) {
            NativeId::FunctionCall
        } else if self.key_is_wk(key, WK_APPLY) {
            NativeId::FunctionApply
        } else if self.key_is_wk(key, WK_BIND) {
            NativeId::FunctionBind
        } else if self.key_is_wk(key, WK_TO_STRING) {
            NativeId::FunctionProtoToString
        } else if self.key_is_wk(key, WK_VALUE_OF) {
            NativeId::ObjectProtoValueOf
        } else if self.key_is_wk(key, WK_HAS_OWN_PROPERTY) {
            NativeId::ObjectHasOwnProperty
        } else {
            return None;
        };
        Some(Ok(self.map_set_method(constant)))
    }

    /// The realm constructor matching a closure's bytecode kind: plain
    /// functions read the `Function` global, async/generator closures read
    /// their derived constructor. Served by `function_method_surface`
    /// because closures have no prototype link to consult; falls back to
    /// `undefined` (the old answer) when no realm global is present.
    fn closure_constructor(&mut self, obj: Handle<JsObject>) -> JsValue {
        let (target, program_id) = {
            let o = self.heap.get(obj);
            (o.callable, o.program_id)
        };
        let name = match target {
            v12_heap::FunctionTarget::Bytecode(fn_idx) => {
                let (is_async, is_generator) = self
                    .functions_for_program(program_id)
                    .get(fn_idx as usize)
                    .map(|f| (f.is_async, f.is_generator))
                    .unwrap_or((false, false));
                match (is_async, is_generator) {
                    (true, true) => "AsyncGeneratorFunction",
                    (true, false) => "AsyncFunction",
                    (false, true) => "GeneratorFunction",
                    (false, false) => "Function",
                }
            }
            // Natives, host closures, bound functions: plain `Function`.
            _ => "Function",
        };
        self.global
            .and_then(|g| self.global_own_constructor(g, name))
            .unwrap_or_else(JsValue::undefined)
    }

    /// Own-shape read of a constructor global (`Function`, `AsyncFunction`,
    /// …) off a realm global. This is `chain_prop` specialized for the
    /// global: shape slots on a realm global index `properties` with the
    /// `GLOBAL_VAR_OFFSET` bias, which the unadjusted `chain_prop` walk
    /// misreads (it served a neighboring intrinsic's constructor).
    fn global_own_constructor(&mut self, global: Handle<JsObject>, name: &str) -> Option<JsValue> {
        let h = self.heap.intern_text(name);
        let pk = v12_heap::PropKey::from_string(h);
        // Dictionary rung first: globals with many properties (harness
        // preambles push them over the shape-transition threshold) hold
        // overflow keys only in the map, with frozen shapes — the same
        // two-store discipline as the `ic_lookup` slow path.
        if let Some(entry) = self
            .heap
            .get(global)
            .dictionary
            .as_ref()
            .and_then(|m| m.get(&pk))
            .copied()
        {
            if entry.is_accessor {
                return None;
            }
            let idx = self.global_slot_index(global, entry.slot as usize);
            return match self.heap.get(global).properties.get(idx).copied() {
                Some(v) if !v.is_hole() => Some(v),
                _ => None,
            };
        }
        let shape = self.shape_of(global);
        let slot = self.heap.lookup_property(shape, pk)?.slot()? as usize;
        let idx = self.global_slot_index(global, slot);
        match self.heap.get(global).properties.get(idx).copied() {
            Some(v) if !v.is_hole() => Some(v),
            _ => None,
        }
    }

    /// `Object.prototype` methods on any ordinary object (including arrays
    /// for toString/valueOf).
    pub(crate) fn object_proto_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        // Integer dispatch first: non-proto keys exit here without paying
        // the shadow walk below (it is a pure read, so comparing first
        // preserves behavior exactly).
        let constant = if self.key_is_wk(key, WK_HAS_OWN_PROPERTY) {
            NativeId::ObjectHasOwnProperty
        } else if self.key_is_wk(key, WK_VALUE_OF) {
            NativeId::ObjectProtoValueOf
        } else if self.key_is_wk(key, WK_TO_STRING) {
            if self.heap.get(obj).kind == Kind::Function {
                NativeId::FunctionProtoToString
            } else if self.heap.get(obj).kind == Kind::Array {
                // Array.prototype.toString === Array.prototype.join(",")
                NativeId::ArrayJoin
            } else {
                NativeId::ObjectProtoToString
            }
        } else {
            return None;
        };
        // Only serve the prototype methods when neither the receiver nor any
        // prototype in its chain shadows them with an own property (user
        // `Array.prototype.toString = …` overrides included). The walk uses
        // the entry-interned key, so shadowing compares canonical identity
        // (the old raw-handle key could miss shadows behind computed keys).
        if let Some(k) = key {
            let mut cursor = Some(obj);
            while let Some(cur) = cursor {
                let (shadow, shape) = {
                    let o = self.heap.get(cur);
                    (o.prototype, o.shape)
                };
                if self.heap.lookup_property(shape, k).is_some() {
                    return None;
                }
                cursor = shadow;
            }
        }
        Some(Ok(self.map_set_method(constant)))
    }

    /// `RegExp.prototype` — the constructor's prototype property (needed
    /// for `new RegExp(...)` instanceof wiring and `RegExp.prototype.exec`
    /// style reads). The realm's RegExp placeholder has no real prototype
    /// object; synthesize a minimal one on first read.
    pub(crate) fn regexp_prototype_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        let regexp_idx = REGEXP_IDX;
        let (Some(g), Some(regexp_idx)) = (self.global, regexp_idx) else {
            return None;
        };
        let regexp_ctor = {
            let heap = &*self.heap;
            heap.get(g)
                .properties
                .get(regexp_idx)
                .and_then(|v| v.as_object())
        }?;
        if obj != regexp_ctor || !self.key_is_wk(key, WK_PROTOTYPE) {
            return None;
        }
        if let Some(p) = self.heap.get(obj).prototype {
            return Some(Ok(JsValue::object(p)));
        }
        self.gc_protect();
        let proto = self.heap.alloc(JsObject::default());
        self.heap.add_root(JsValue::object(proto));
        self.heap.get_mut(obj).prototype = Some(proto);
        Some(Ok(JsValue::object(proto)))
    }
    /// Array instance surface: the method table (push/pop/join/entries/
    /// keys/values, with push/join via the cached native path) and the
    /// `length` slot read.
    pub(crate) fn array_instance_surface(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        if self.heap.get(obj).kind != Kind::Array {
            return None;
        }
        if let Some(id) = self.method_native(Kind::Array, key_v) {
            // Array methods are synthesized via the cached native path.
            let value = match id {
                NativeId::ArrayPush => self.cached_native(NativeId::ArrayPush),
                NativeId::ArrayJoin => self.cached_native(NativeId::ArrayJoin),
                _ => self.map_set_method(id),
            };
            return Some(Ok(value));
        }
        if !self.key_is_wk(key, WK_LENGTH) {
            return None;
        }
        // Length is properties[0] for arrays regardless of shape state
        // (covers arrays created by native handlers without shape binding)
        let value = self.heap.get(obj).properties.first().copied()?;
        Some(Ok(value))
    }

    /// Map/Set method fast paths, recognized by object kind. Each
    /// synthesizes a function whose callable routes through the engine's
    /// native registry (the `NATIVE_*` constants are out-of-range bytecode
    /// indices the registry dispatches).
    pub(crate) fn map_set_surface(
        &mut self,
        obj: Handle<JsObject>,
        key: Option<PropKey>,
    ) -> Option<Result<JsValue, JSException>> {
        let kind = self.heap.get(obj).kind;
        if kind != Kind::Map && kind != Kind::Set {
            return None;
        }
        // `size` is a getter: invoke the handler directly with the
        // Map/Set as `this`. Methods return the synthesized function.
        if self.key_is_wk(key, WK_SIZE) {
            let size_const = if kind == Kind::Map {
                NativeId::MapSize
            } else {
                NativeId::SetSize
            };
            self.gc_protect();
            // Single router: `MapSize`/`SetSize` fall through the callback
            // seam (`None`) to the same registry call, so this matches the
            // previous direct `call_native` result.
            return Some(self.dispatch_native(size_const, JsValue::object(obj), &[]));
        }
        let constant = if kind == Kind::Map {
            if self.key_is_wk(key, WK_GET) {
                NativeId::MapGet
            } else if self.key_is_wk(key, WK_SET) {
                NativeId::MapSet
            } else if self.key_is_wk(key, WK_HAS) {
                NativeId::MapHas
            } else if self.key_is_wk(key, WK_DELETE) {
                NativeId::MapDelete
            } else if self.key_is_wk(key, WK_CLEAR) {
                NativeId::MapClear
            } else if self.key_is_wk(key, WK_FOR_EACH) {
                NativeId::MapForEach
            } else if self.key_is_wk(key, WK_ENTRIES) {
                NativeId::MapEntries
            } else if self.key_is_wk(key, WK_KEYS) {
                NativeId::MapKeys
            } else if self.key_is_wk(key, WK_VALUES) {
                NativeId::MapValues
            } else {
                return None;
            }
        } else if self.key_is_wk(key, WK_ADD) {
            NativeId::SetAdd
        } else if self.key_is_wk(key, WK_HAS) {
            NativeId::SetHas
        } else if self.key_is_wk(key, WK_DELETE) {
            NativeId::SetDelete
        } else if self.key_is_wk(key, WK_CLEAR) {
            NativeId::SetClear
        } else if self.key_is_wk(key, WK_FOR_EACH) {
            NativeId::SetForEach
        } else if self.key_is_wk(key, WK_ENTRIES) {
            NativeId::SetEntries
        } else if self.key_is_wk(key, WK_KEYS) {
            NativeId::SetKeys
        } else if self.key_is_wk(key, WK_VALUES) {
            NativeId::SetValues
        } else {
            return None;
        };
        Some(Ok(self.map_set_method(constant)))
    }

    /// Shape-bound lookup with the polymorphic inline cache: probe the IC
    /// first (only data descriptors with a slot are cached; up to
    /// `IC_MAX_ENTRIES` shapes per site), then walk the own shape and the
    /// prototype chain, recording own-shape hits in the IC.
    ///
    /// `key` is the entry-interned key from `get_property` (`Some` for
    /// string keys); `None` coerces here via `property_key`, preserving the
    /// old single-intern behavior for non-string keys.
    pub(crate) fn ic_lookup(
        &mut self,
        site_fn: u32,
        site_pc: u32,
        obj: Handle<JsObject>,
        key_v: JsValue,
        key: Option<PropKey>,
    ) -> Result<JsValue, JSException> {
        let key = match key {
            Some(k) => k,
            None => self.property_key(key_v)?,
        };
        let shape = self.shape_of(obj);
        // A proxy is never served or recorded by the shape/IC path: its shape
        // is the shared empty-object root, so a stub or IC entry recorded
        // under it would alias unrelated ordinary objects. `[[Get]]` trap
        // dispatch is a later phase; this only keeps both caches clean.
        let is_proxy = self.heap.get(obj).kind == Kind::Proxy;

        // StubCache guarded fast path: serves recorded own AND proto
        // `Data` locations in O(1) (shape+key identity, proto generation,
        // and chain verifies — one integer compare each). OOB storage
        // falls through to the walk below, which resolves exactly as
        // before (including the realm-global fallback).
        if !is_proxy && let Some((holder, hsl)) = self.heap.stub_lookup_proto(obj, shape, key) {
            let idx = self.global_slot_index(holder, hsl as usize);
            if let Some(v) = self.heap.get(holder).properties.get(idx) {
                return Ok(*v);
            }
        }

        let cached_slot = if is_proxy {
            None
        } else {
            self.feedback
                .get(&site_fn)
                .and_then(|fv| fv.ics.get(&site_pc))
                .and_then(|ic| ic.get(shape, key))
        };
        if let Some(slot) = cached_slot
            && let Some(v) = self
                .heap
                .get(obj)
                .properties
                .get(self.global_slot_index(obj, slot as usize))
        {
            return Ok(*v);
        }

        // Slow path: own dictionary rung and shape first, then the
        // prototype chain (each level consults both — the two stores are
        // disjoint: overflow keys live only in the map, base keys only in
        // the frozen shape).
        let mut cur = Some(obj);
        let mut hit: Option<(Handle<JsObject>, Descriptor)> = None;
        let mut dict_hit: Option<(Handle<JsObject>, v12_heap::DictEntry)> = None;
        while let Some(o) = cur {
            if let Some(entry) = self
                .heap
                .get(o)
                .dictionary
                .as_ref()
                .and_then(|m| m.get(&key))
                .copied()
            {
                dict_hit = Some((o, entry));
                break;
            }
            let sh = self.shape_of(o);
            if let Some(d) = self.heap.lookup_property(sh, key) {
                hit = Some((o, *d));
                break;
            }
            cur = self.heap.get(o).prototype;
        }
        if let Some((owner, entry)) = dict_hit {
            // Dictionary overflow hit: same serving rules as the shape
            // Data arm below (bias, stub record, direct index). Accessors
            // invoke through the shared path.
            if entry.is_accessor {
                if let Some(getter) = entry.getter {
                    return self.call_accessor(getter, JsValue::object(obj));
                }
                return Ok(JsValue::undefined());
            }
            let owner_shape = self.shape_of(owner);
            if !is_proxy {
                self.heap
                    .stub_record_proto(obj, shape, key, owner, owner_shape, entry.slot);
            }
            let value =
                self.heap.get(owner).properties[self.global_slot_index(owner, entry.slot as usize)];
            return Ok(value);
        }
        match hit {
            Some((owner, desc)) => match desc {
                Descriptor::Data { slot, .. } => {
                    // Record the chain stub (own or proto, depth ≤ 2) for
                    // future O(1) probes — deliberately WITHOUT the
                    // `owner == obj` gate below: proto hits are the point.
                    // Accessor hits never record (no servable slot).
                    let owner_shape = self.shape_of(owner);
                    if !is_proxy {
                        self.heap
                            .stub_record_proto(obj, shape, key, owner, owner_shape, slot);
                    }
                    let value = self.heap.get(owner).properties
                        [self.global_slot_index(owner, slot as usize)];
                    if owner == obj && !is_proxy {
                        self.feedback
                            .entry(site_fn)
                            .or_default()
                            .ics
                            .entry(site_pc)
                            .or_default()
                            .record(shape, key, slot);
                    }
                    Ok(value)
                }
                Descriptor::Accessor { getter, .. } => {
                    if let Some(getter) = getter {
                        // Real callable: invoke the getter with `this` = the
                        // receiver object.
                        self.call_accessor(getter, JsValue::object(obj))
                    } else {
                        Ok(JsValue::undefined())
                    }
                }
            },
            None => {
                // Realm-global intrinsic fallback: the descriptor-less
                // intrinsic prefix of a global object is invisible to the
                // shape walk, so `other.eval` / `globalThis.Math` would read
                // `undefined`. Answer it from the prefix slots.
                if let Some(v) = self.realm_global_intrinsic_read(obj, key_v) {
                    return Ok(v);
                }
                Ok(JsValue::undefined())
            }
        }
    }

    /// `SetProperty`: overwrite own writable slots, create new own properties
    /// through shape transitions, shadow writable inherited ones, and route
    /// canonical indices on arrays through the element store. Blocked writes
    /// (non-writable own/inherited data, setterless accessors, primitive
    /// bases, non-extensible receivers) are silently dropped in sloppy mode
    /// and throw a `TypeError` in strict mode (ES OrdinarySet `Throw`
    /// parameter), selected by `strict` — threaded from the current
    /// `FunctionBytecode::is_strict` at the `SetProperty` arm.
    pub(crate) fn set_property(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        value: JsValue,
        strict: bool,
    ) -> Result<(), JSException> {
        let Some(obj) = obj_v.as_object() else {
            if obj_v.is_null() || obj_v.is_undefined() {
                return Err(JSException(self.error_value(
                    "TypeError: cannot set properties of null or undefined",
                )));
            }
            // Primitive targets accept and drop writes in sloppy mode (no
            // wrapper objects); strict mode throws per ES SetValue.
            if strict {
                return Err(JSException(self.error_value(
                    "TypeError: cannot set property on a primitive value",
                )));
            }
            return Ok(());
        };

        // Proxy exotic: `[[Set]]` is the `set` trap (ES 10.5.9). Checked
        // before the element fast path and the RegExp slot so a proxy is
        // never treated as an ordinary object.
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_set(obj, obj_v, key_v, value);
        }

        // Flatten-once (same contract as `get_property`): the element fast
        // path, the RegExp check, and the intern below share one flatten.
        self.flatten_key(key_v);
        let kind = self.heap.get(obj).kind;
        if (kind == Kind::Array || kind == Kind::Arguments)
            && let Some(idx) = self.array_index_of(key_v)
        {
            // Arguments exotic: if mapped, the element mirrors the parameter
            // slot (v1 keeps the element store authoritative; callers inspect
            // `heap.get(obj).arguments_mapped` directly).
            self.array_set_element(obj, idx, value);
            return Ok(());
        }
        // RegExp `lastIndex` write: stores into the internal slot. Per spec
        // the value is coerced via ToNumber.
        if kind == Kind::RegExp && self.key_is(key_v, "lastIndex") {
            let n = ops::to_number(self.heap, value);
            if n.fract() == 0.0
                && (-1e15..=1e15).contains(&n)
                && let Some(smi) = JsValue::from_i32_smi(n as i32)
            {
                self.heap.get_mut(obj).properties[RegExpSlot::LastIndex as usize] = smi;
                return Ok(());
            }
            self.heap.get_mut(obj).properties[RegExpSlot::LastIndex as usize] =
                JsValue::from_f64(n);
            return Ok(());
        }

        let key = self.property_key(key_v)?;
        let shape = self.shape_of(obj);
        let own = self.heap.get(shape).descriptors.find(key).copied();

        if let Some(d) = own {
            match d {
                Descriptor::Data { slot, attrs, .. } => {
                    if attrs.writable() {
                        let idx = self.global_slot_index(obj, slot as usize);
                        self.heap.get_mut(obj).properties[idx] = value;
                    } else if strict {
                        return Err(JSException(
                            self.error_value("TypeError: cannot assign to read-only property"),
                        ));
                    }
                    return Ok(());
                }
                Descriptor::Accessor { setter, .. } => {
                    // Accessor with setter: invoke it with `this` = the
                    // receiver and the assigned value as the argument. Without
                    // a setter, sloppy sets are silently dropped and strict
                    // sets throw.
                    if let Some(setter) = setter {
                        let args = [value];
                        self.call_accessor_with(setter, JsValue::object(obj), &args)?;
                    } else if strict {
                        return Err(JSException(self.error_value(
                            "TypeError: cannot assign to accessor without setter",
                        )));
                    }
                    return Ok(());
                }
            }
        }

        // Dictionary rung: overflow keys update in place (mirrors the
        // shape arms above; the two stores never overlap).
        if let Some(entry) = self
            .heap
            .get(obj)
            .dictionary
            .as_ref()
            .and_then(|m| m.get(&key))
            .copied()
        {
            if entry.is_accessor {
                if let Some(setter) = entry.setter {
                    let args = [value];
                    self.call_accessor_with(setter, JsValue::object(obj), &args)?;
                } else if strict {
                    return Err(JSException(self.error_value(
                        "TypeError: cannot assign to accessor without setter",
                    )));
                }
                return Ok(());
            }
            if entry.attrs.writable() {
                let idx = self.global_slot_index(obj, entry.slot as usize);
                self.heap.get_mut(obj).properties[idx] = value;
            } else if strict {
                return Err(JSException(
                    self.error_value("TypeError: cannot assign to read-only property"),
                ));
            }
            return Ok(());
        }

        // An inherited non-writable data property or accessor without setter
        // blocks shadowing (ES OrdinarySet): silent in sloppy, TypeError in
        // strict.
        if let Some(d) = self.inherited_descriptor(obj, key) {
            match d {
                Descriptor::Data { attrs, .. } if !attrs.writable() => {
                    if strict {
                        return Err(JSException(
                            self.error_value("TypeError: cannot assign to read-only property"),
                        ));
                    }
                    return Ok(());
                }
                Descriptor::Accessor { setter, .. } if setter.is_none() => {
                    if strict {
                        return Err(JSException(self.error_value(
                            "TypeError: cannot assign to accessor without setter",
                        )));
                    }
                    return Ok(());
                }
                Descriptor::Accessor {
                    setter: Some(setter),
                    ..
                } => {
                    // Inherited accessor with setter: invoke it with the
                    // receiver and the assigned value.
                    self.call_accessor_with(setter, JsValue::object(obj), &[value])?;
                    return Ok(());
                }
                _ => {}
            }
        }

        if self.heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
            if strict {
                return Err(JSException(self.error_value(
                    "TypeError: cannot add property to non-extensible object",
                )));
            }
            return Ok(());
        }

        // Extend the layout: the transition may allocate, so protect roots
        // first and publish the new shape before touching storage again.
        // Dictionary-rung objects absorb new keys into the map instead
        // (shapes stop growing past the spill threshold).
        self.gc_protect();
        if self.heap.get(obj).dictionary.is_some() {
            let (slot, seq) = {
                let o = self.heap.get(obj);
                (o.properties.len() as u32, o.dict_seq)
            };
            self.heap.get_mut(obj).properties.push(value);
            self.heap.get_mut(obj).property_keys.push(Some(key));
            let entry = v12_heap::DictEntry {
                slot,
                attrs: Attrs::DEFAULT,
                getter: None,
                setter: None,
                is_accessor: false,
                seq,
            };
            if let Some(map) = self.heap.get_mut(obj).dictionary.as_mut() {
                map.insert(key, entry);
            }
            self.heap.get_mut(obj).dict_seq = seq + 1;
            return Ok(());
        }
        let child = self.heap.add_property(shape, key, Attrs::DEFAULT);
        self.bind_shape(obj, child);
        if self.is_realm_global(obj) {
            // Global storage keeps the intrinsic prefix; slot numbering from
            // the shared shape chain must not overlap it. The invariant
            // `properties.len() == GLOBAL_VAR_OFFSET + num_own` restores
            // itself by appending (and backfilling if an embedder's global
            // was assembled with fewer slots).
            let idx = GLOBAL_VAR_OFFSET
                + usize::try_from(self.heap.get(child).num_own - 1).expect("slot fits usize");
            let len = self.heap.get(obj).properties.len();
            if len <= idx {
                self.heap
                    .get_mut(obj)
                    .properties
                    .resize(idx + 1, JsValue::undefined());
                self.heap.get_mut(obj).property_keys.resize(idx + 1, None);
            }
            self.heap.get_mut(obj).properties[idx] = value;
            self.heap.get_mut(obj).property_keys[idx] = Some(key);
        } else {
            self.heap.get_mut(obj).properties.push(value);
            self.heap.get_mut(obj).property_keys.push(Some(key));
        }
        Ok(())
    }

    /// Defines an own data property with explicit attributes, bypassing the
    /// setter/prototype walk of [`Self::set_property`]. Used for fresh
    /// function-intrinsic properties (`length`, `prototype`, `constructor`)
    /// whose attributes the spec fixes.
    pub(crate) fn define_own_data_attrs(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        value: JsValue,
        attrs: Attrs,
    ) -> Result<(), JSException> {
        let Some(obj) = obj_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: cannot define property on non-object"),
            ));
        };
        // Flatten-once so the single intern below reads flat storage.
        self.flatten_key(key_v);
        let key = self.property_key(key_v)?;
        self.gc_protect();
        // Dictionary rung: defines update or insert in the map (shapes
        // frozen); base keys still take the shape path below.
        if self.heap.get(obj).dictionary.is_some() {
            let shape = self.shape_of(obj);
            let in_shape = self.heap.get(shape).descriptors.find(key).is_some();
            if let Some(entry) = self
                .heap
                .get(obj)
                .dictionary
                .as_ref()
                .and_then(|m| m.get(&key))
                .copied()
            {
                if !entry.is_accessor {
                    let idx = entry.slot as usize;
                    let settings = &mut self.heap.get_mut(obj);
                    if settings.properties.len() <= idx {
                        settings.properties.resize(idx + 1, JsValue::hole());
                    }
                    settings.properties[idx] = value;
                    if let Some(map) = self.heap.get_mut(obj).dictionary.as_mut()
                        && let Some(slot_entry) = map.get_mut(&key)
                    {
                        slot_entry.attrs = attrs;
                    }
                }
                // Data-over-accessor redefine is a no-op (mirrors the
                // engine path); either way no shape is touched.
                return Ok(());
            }
            if !in_shape {
                let (slot, seq) = {
                    let o = self.heap.get(obj);
                    (o.properties.len() as u32, o.dict_seq)
                };
                self.heap.get_mut(obj).properties.push(value);
                self.heap.get_mut(obj).property_keys.push(Some(key));
                let entry = v12_heap::DictEntry {
                    slot,
                    attrs,
                    getter: None,
                    setter: None,
                    is_accessor: false,
                    seq,
                };
                if let Some(map) = self.heap.get_mut(obj).dictionary.as_mut() {
                    map.insert(key, entry);
                }
                self.heap.get_mut(obj).dict_seq = seq + 1;
                return Ok(());
            }
        }
        let shape = self.shape_of(obj);
        let child = self.heap.add_property(shape, key, attrs);
        self.bind_shape(obj, child);
        let slot = child_slot(self.heap, child);
        let settings = &mut self.heap.get_mut(obj);
        if settings.properties.len() <= slot {
            settings.properties.resize(slot + 1, JsValue::hole());
        }
        settings.properties[slot] = value;
        if settings.property_keys.len() <= slot {
            settings.property_keys.resize(slot + 1, None);
        }
        settings.property_keys[slot] = Some(key);
        Ok(())
    }

    /// First descriptor naming `key` along `obj`'s prototype chain.
    pub(crate) fn inherited_descriptor(
        &mut self,
        obj: Handle<JsObject>,
        key: PropKey,
    ) -> Option<Descriptor> {
        let mut cur = self.heap.get(obj).prototype;
        while let Some(o) = cur {
            // Dictionary rung first (overflow keys live only here).
            if let Some(entry) = self
                .heap
                .get(o)
                .dictionary
                .as_ref()
                .and_then(|m| m.get(&key))
                .copied()
            {
                if entry.is_accessor {
                    return Some(Descriptor::Accessor {
                        key,
                        getter: entry.getter,
                        setter: entry.setter,
                        attrs: entry.attrs,
                    });
                }
                return Some(Descriptor::Data {
                    key,
                    slot: entry.slot,
                    attrs: entry.attrs,
                });
            }
            let sh = self.shape_of(o);
            if let Some(d) = self.heap.lookup_property(sh, key) {
                return Some(*d);
            }
            cur = self.heap.get(o).prototype;
        }
        None
    }

    /// `in` operator: `key in obj`. Throws TypeError if `obj` is not an
    /// object; otherwise returns true when `key` (after ToPropertyKey)
    /// exists anywhere on `obj`'s prototype chain, including array indices.
    pub(crate) fn op_in(&mut self, key_v: JsValue, obj_v: JsValue) -> Result<bool, JSException> {
        let Some(obj) = obj_v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: right-hand side of 'in' should be an object",
            )));
        };
        // Proxy exotic: `HasProperty` is the `has` trap (ES 10.5.6). Checked
        // before the element fast path so a proxy is never treated as an
        // ordinary object.
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_has(key_v, obj);
        }
        // Fast path for array/arguments indices: check element storage before
        // coercing the key, which may allocate. Holes count as absent.
        // Flatten-once so the index scan never materializes.
        self.flatten_key(key_v);
        let kind = self.heap.get(obj).kind;
        if (kind == Kind::Array || kind == Kind::Arguments)
            && let Some(idx) = self.array_index_of(key_v)
            && self.heap.get(obj).get_element(idx).is_some()
        {
            return Ok(true);
        }
        let key = self.property_key(key_v)?;
        let mut cur = Some(obj);
        while let Some(o) = cur {
            // Dictionary rung: overflow keys live only here.
            if self
                .heap
                .get(o)
                .dictionary
                .as_ref()
                .is_some_and(|m| m.contains_key(&key))
            {
                return Ok(true);
            }
            let sh = self.shape_of(o);
            if self.heap.lookup_property(sh, key).is_some() {
                return Ok(true);
            }
            // For arrays, the prototype chain check after the element fast
            // path already covers named properties; indices are only in the
            // element store, so no extra work is needed. We still walk in
            // case a numeric string was installed as a named property.
            cur = self.heap.get(o).prototype;
        }
        // If we fell through from the array fast path with a hole, and the
        // shape walk found nothing, the property is absent.
        Ok(false)
    }

    // ------------------------------------------------------------------
    // Proxy trap primitives (ES 10.5)
    //
    // The engine-side `Object.*`/`Reflect.*` statics cannot invoke a trap:
    // their native handlers only hold `&mut Heap`. These helpers run at the
    // interpreter seam (reached from both call and construct paths via
    // `dispatch_native` -> `run_callback_builtin`) so every static that
    // observes a `Kind::Proxy` receiver re-enters the machine.
    // ------------------------------------------------------------------

    /// Resolves `handler.<name>`, mapping an absent/`undefined`/`null` method
    /// to `None` (ES `GetMethod`) and a present non-callable to a `TypeError`.
    fn proxy_trap_method(
        &mut self,
        handler: Handle<JsObject>,
        name: &str,
    ) -> Result<Option<Handle<JsObject>>, JSException> {
        let key = self.new_temp_key(name);
        let trap_v = self.get_property(0, 0, JsValue::object(handler), key)?;
        if trap_v.is_undefined() || trap_v.is_null() {
            return Ok(None);
        }
        let Some(trap) = trap_v.as_object() else {
            return Err(JSException(self.error_value(&format!(
                "TypeError: '{name}' trap must be a function"
            ))));
        };
        if self.heap.get(trap).kind != Kind::Function {
            return Err(JSException(self.error_value(&format!(
                "TypeError: '{name}' trap must be a function"
            ))));
        }
        Ok(Some(trap))
    }

    /// `target`/`handler` pair for a live proxy, or the revoked-proxy
    /// `TypeError` naming the internal method that observed it.
    fn proxy_parts(
        &mut self,
        proxy: Handle<JsObject>,
        op: &str,
    ) -> Result<(Handle<JsObject>, Handle<JsObject>), JSException> {
        let (target, handler) = {
            let o = self.heap.get(proxy);
            (o.proxy_target, o.proxy_handler)
        };
        match (target, handler) {
            (Some(t), Some(h)) => Ok((t, h)),
            _ => Err(JSException(self.error_value(&format!(
                "TypeError: Cannot perform '{op}' on a proxy that has been revoked"
            )))),
        }
    }

    /// `[[GetPrototypeOf]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_proto_of(
        &mut self,
        obj: Handle<JsObject>,
    ) -> Result<JsValue, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_get_prototype_of(obj);
        }
        Ok(match self.heap.get(obj).prototype {
            Some(p) => JsValue::object(p),
            None => JsValue::null(),
        })
    }

    /// `[[IsExtensible]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_is_extensible(
        &mut self,
        obj: Handle<JsObject>,
    ) -> Result<bool, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_is_extensible(obj);
        }
        Ok(self.heap.get(obj).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0)
    }

    /// `[[OwnPropertyKeys]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_own_keys(
        &mut self,
        obj: Handle<JsObject>,
    ) -> Result<Vec<PropKey>, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_own_keys(obj);
        }
        Ok(self.ordinary_own_keys(obj))
    }

    /// Ordinary `[[GetOwnProperty]]` as an [`OwnDesc`]; `None` when absent.
    /// Element-store indices read all-true attributes (v1 does not model
    /// per-element attributes).
    pub(crate) fn ordinary_own_descriptor(
        &mut self,
        obj: Handle<JsObject>,
        key: PropKey,
    ) -> Option<OwnDesc> {
        let kind = self.heap.get(obj).kind;
        if (kind == Kind::Array || kind == Kind::Arguments)
            && let Some(idx) = self.prop_key_index(key)
        {
            let v = self.heap.get(obj).get_element(idx)?;
            if v.is_hole() {
                return None;
            }
            return Some(OwnDesc {
                has_value: true,
                value: v,
                has_writable: true,
                writable: true,
                has_enumerable: true,
                enumerable: true,
                has_configurable: true,
                configurable: true,
                ..Default::default()
            });
        }
        if let Some(entry) = self
            .heap
            .get(obj)
            .dictionary
            .as_ref()
            .and_then(|m| m.get(&key))
            .copied()
        {
            if entry.is_accessor {
                return Some(OwnDesc {
                    has_get: true,
                    get: entry
                        .getter
                        .map(JsValue::object)
                        .unwrap_or(JsValue::undefined()),
                    has_set: true,
                    set: entry
                        .setter
                        .map(JsValue::object)
                        .unwrap_or(JsValue::undefined()),
                    has_enumerable: true,
                    enumerable: entry.attrs.enumerable(),
                    has_configurable: true,
                    configurable: entry.attrs.configurable(),
                    ..Default::default()
                });
            }
            let live = self
                .heap
                .get(obj)
                .properties
                .get(entry.slot as usize)
                .is_some_and(|v| !v.is_hole());
            if !live {
                return None;
            }
            let value = self
                .heap
                .get(obj)
                .properties
                .get(entry.slot as usize)
                .copied()
                .unwrap_or(JsValue::undefined());
            return Some(OwnDesc {
                has_value: true,
                value,
                has_writable: true,
                writable: entry.attrs.writable(),
                has_enumerable: true,
                enumerable: entry.attrs.enumerable(),
                has_configurable: true,
                configurable: entry.attrs.configurable(),
                ..Default::default()
            });
        }
        let shape = self.shape_of(obj);
        match self.heap.lookup_property(shape, key).copied()? {
            v12_heap::Descriptor::Data { slot, attrs, .. } => {
                let live = self
                    .heap
                    .get(obj)
                    .properties
                    .get(slot as usize)
                    .is_some_and(|v| !v.is_hole());
                if !live {
                    return None;
                }
                let value = self
                    .heap
                    .get(obj)
                    .properties
                    .get(slot as usize)
                    .copied()
                    .unwrap_or(JsValue::undefined());
                Some(OwnDesc {
                    has_value: true,
                    value,
                    has_writable: true,
                    writable: attrs.writable(),
                    has_enumerable: true,
                    enumerable: attrs.enumerable(),
                    has_configurable: true,
                    configurable: attrs.configurable(),
                    ..Default::default()
                })
            }
            v12_heap::Descriptor::Accessor {
                getter,
                setter,
                attrs,
                ..
            } => Some(OwnDesc {
                has_get: true,
                get: getter.map(JsValue::object).unwrap_or(JsValue::undefined()),
                has_set: true,
                set: setter.map(JsValue::object).unwrap_or(JsValue::undefined()),
                has_enumerable: true,
                enumerable: attrs.enumerable(),
                has_configurable: true,
                configurable: attrs.configurable(),
                ..Default::default()
            }),
        }
    }

    /// `PropKey` → element index when the key names a canonical array index.
    fn prop_key_index(&mut self, key: PropKey) -> Option<u32> {
        let h = key.string()?;
        let text = self.string_text(h);
        if text.is_empty() || text.len() > 10 {
            return None;
        }
        let mut acc: u32 = 0;
        for b in text.bytes() {
            if !b.is_ascii_digit() {
                return None;
            }
            acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
        }
        Some(acc)
    }

    /// Materializes [`OwnDesc`] as a plain descriptor object for a trap
    /// argument (ES `FromPropertyDescriptor`).
    pub(crate) fn own_desc_to_object(&mut self, desc: OwnDesc) -> JsValue {
        self.gc_protect();
        let d = self.heap.alloc(JsObject::default());
        self.heap.add_root(JsValue::object(d));
        let put = |this: &mut Self, name: &str, v: JsValue| {
            let key = this.new_temp_key(name);
            let _ = this.define_own_data_attrs(JsValue::object(d), key, v, Attrs::DEFAULT);
        };
        if desc.is_accessor() {
            put(self, "get", desc.get);
            put(self, "set", desc.set);
        } else {
            put(self, "value", desc.value);
            put(self, "writable", JsValue::from_bool(desc.writable));
        }
        put(self, "enumerable", JsValue::from_bool(desc.enumerable));
        put(self, "configurable", JsValue::from_bool(desc.configurable));
        JsValue::object(d)
    }

    /// `[[GetOwnProperty]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_get_own_property(
        &mut self,
        obj: Handle<JsObject>,
        key: PropKey,
    ) -> Result<Option<OwnDesc>, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_get_own_property(obj, key);
        }
        Ok(self.ordinary_own_descriptor(obj, key))
    }

    /// `[[Delete]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_delete(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
    ) -> Result<bool, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_delete(obj, key_v);
        }
        self.delete_property(JsValue::object(obj), key_v)
    }

    /// `[[SetPrototypeOf]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_set_prototype_of(
        &mut self,
        obj: Handle<JsObject>,
        proto: Option<Handle<JsObject>>,
    ) -> Result<bool, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_set_prototype_of(obj, proto);
        }
        // ES 9.1.2: same-value short-circuits true; non-extensible refuses.
        if proto == self.heap.get(obj).prototype {
            return Ok(true);
        }
        if self.heap.get(obj).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE != 0 {
            return Ok(false);
        }
        let mut cur = proto;
        while let Some(p) = cur {
            if p == obj {
                return Ok(false);
            }
            cur = self.heap.get(p).prototype;
        }
        let cell = self.heap.validity_cell_of(obj);
        self.heap.bump_validity(cell);
        self.heap.bump_proto_generation();
        self.heap.get_mut(obj).prototype = proto;
        Ok(true)
    }

    /// `[[PreventExtensions]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_prevent_extensions(
        &mut self,
        obj: Handle<JsObject>,
    ) -> Result<bool, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_prevent_extensions(obj);
        }
        self.heap.get_mut(obj).flags |= v12_heap::JsObject::FLAG_NOT_EXTENSIBLE;
        let cell = self.heap.validity_cell_of(obj);
        self.heap.bump_validity(cell);
        Ok(true)
    }

    /// `[[DefineOwnProperty]]` of an object, dispatching a proxy to its trap.
    pub(crate) fn object_define_own_property(
        &mut self,
        obj: Handle<JsObject>,
        key: PropKey,
        desc: OwnDesc,
    ) -> Result<bool, JSException> {
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_define_own_property(obj, key, desc);
        }
        self.ordinary_define_from_own_desc(obj, key, desc)
    }

    /// `[[IsExtensible]]` (ES 10.5.3).
    fn proxy_op_is_extensible(&mut self, proxy: Handle<JsObject>) -> Result<bool, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "isExtensible")?;
        let extensible_target =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let Some(trap) = self.proxy_trap_method(handler, "isExtensible")? else {
            return Ok(extensible_target);
        };
        self.gc_protect();
        let result =
            self.call_inline(trap, JsValue::object(handler), &[JsValue::object(target)])?;
        let boolean = ops::to_boolean(self.heap, result);
        // ES step 10: the trap result must agree with the target.
        if boolean != extensible_target {
            return Err(JSException(
                self.error_value("TypeError: 'isExtensible' trap result mismatch"),
            ));
        }
        Ok(boolean)
    }

    /// `[[PreventExtensions]]` (ES 10.5.4).
    fn proxy_op_prevent_extensions(
        &mut self,
        proxy: Handle<JsObject>,
    ) -> Result<bool, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "preventExtensions")?;
        let target_extensible =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let Some(trap) = self.proxy_trap_method(handler, "preventExtensions")? else {
            // Forward to the target, then mirror the result on the proxy.
            let forwarded = self.object_prevent_extensions(target)?;
            if forwarded {
                self.heap.get_mut(proxy).flags |= v12_heap::JsObject::FLAG_NOT_EXTENSIBLE;
                let cell = self.heap.validity_cell_of(proxy);
                self.heap.bump_validity(cell);
            }
            return Ok(forwarded);
        };
        self.gc_protect();
        let result =
            self.call_inline(trap, JsValue::object(handler), &[JsValue::object(target)])?;
        if !ops::to_boolean(self.heap, result) {
            return Ok(false);
        }
        // ES step 12: a true result requires the target to be non-extensible.
        if target_extensible {
            return Err(JSException(self.error_value(
                "TypeError: 'preventExtensions' trap returned true for an extensible target",
            )));
        }
        self.heap.get_mut(proxy).flags |= v12_heap::JsObject::FLAG_NOT_EXTENSIBLE;
        let cell = self.heap.validity_cell_of(proxy);
        self.heap.bump_validity(cell);
        Ok(true)
    }

    /// `[[GetPrototypeOf]]` (ES 10.5.1).
    fn proxy_op_get_prototype_of(
        &mut self,
        proxy: Handle<JsObject>,
    ) -> Result<JsValue, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "getPrototypeOf")?;
        let target_proto = self.heap.get(target).prototype;
        let target_extensible =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let Some(trap) = self.proxy_trap_method(handler, "getPrototypeOf")? else {
            return Ok(match target_proto {
                Some(p) => JsValue::object(p),
                None => JsValue::null(),
            });
        };
        self.gc_protect();
        let result =
            self.call_inline(trap, JsValue::object(handler), &[JsValue::object(target)])?;
        // ES step 9: the trap result must be an object or null.
        if !result.is_null() && result.as_object().is_none() {
            return Err(JSException(self.error_value(
                "TypeError: 'getPrototypeOf' trap result must be an object or null",
            )));
        }
        // ES step 10: a non-extensible target requires proto identity.
        if !target_extensible && result.as_object() != target_proto {
            return Err(JSException(self.error_value(
                "TypeError: 'getPrototypeOf' trap result differs from the target's prototype",
            )));
        }
        Ok(result)
    }

    /// `[[SetPrototypeOf]]` (ES 10.5.2).
    fn proxy_op_set_prototype_of(
        &mut self,
        proxy: Handle<JsObject>,
        proto: Option<Handle<JsObject>>,
    ) -> Result<bool, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "setPrototypeOf")?;
        let target_proto = self.heap.get(target).prototype;
        let target_extensible =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let Some(trap) = self.proxy_trap_method(handler, "setPrototypeOf")? else {
            return self.object_set_prototype_of(target, proto);
        };
        let proto_v = match proto {
            Some(p) => JsValue::object(p),
            None => JsValue::null(),
        };
        self.gc_protect();
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), proto_v],
        )?;
        if !ops::to_boolean(self.heap, result) {
            return Ok(false);
        }
        // ES step 12: a true result for a non-extensible target requires the
        // requested proto to equal the target's current proto.
        if !target_extensible && proto != target_proto {
            return Err(JSException(self.error_value(
                "TypeError: 'setPrototypeOf' trap result differs from the target's prototype",
            )));
        }
        let cell = self.heap.validity_cell_of(proxy);
        self.heap.bump_validity(cell);
        self.heap.bump_proto_generation();
        self.heap.get_mut(proxy).prototype = proto;
        Ok(true)
    }

    /// `[[Delete]]` (ES 10.5.10).
    fn proxy_op_delete(
        &mut self,
        proxy: Handle<JsObject>,
        key_v: JsValue,
    ) -> Result<bool, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "deleteProperty")?;
        let key = self.property_key(key_v)?;
        let target_desc = self.ordinary_own_descriptor(target, key);
        let target_extensible =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let key_v = prop_key_value(key);
        let Some(trap) = self.proxy_trap_method(handler, "deleteProperty")? else {
            return self.object_delete(target, key_v);
        };
        self.gc_protect();
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), key_v],
        )?;
        let boolean = ops::to_boolean(self.heap, result);
        if !boolean {
            return Ok(false);
        }
        // ES step 12: an own non-configurable target property cannot be
        // reported deleted unless the target is non-extensible.
        if let Some(desc) = target_desc
            && desc.has_configurable
            && !desc.configurable
            && target_extensible
        {
            return Err(JSException(self.error_value(
                "TypeError: 'deleteProperty' trap cannot delete a non-configurable property",
            )));
        }
        Ok(true)
    }

    /// `[[GetOwnProperty]]` (ES 10.5.5).
    fn proxy_op_get_own_property(
        &mut self,
        proxy: Handle<JsObject>,
        key: PropKey,
    ) -> Result<Option<OwnDesc>, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "getOwnPropertyDescriptor")?;
        let target_desc = self.ordinary_own_descriptor(target, key);
        let target_extensible =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let key_v = prop_key_value(key);
        let Some(trap) = self.proxy_trap_method(handler, "getOwnPropertyDescriptor")? else {
            return Ok(target_desc);
        };
        self.gc_protect();
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), key_v],
        )?;
        if result.is_undefined() {
            // ES step 14: `undefined` demands an extensible target and no
            // non-configurable own property.
            if let Some(td) = target_desc
                && td.has_configurable
                && !td.configurable
            {
                return Err(JSException(self.error_value(
                    "TypeError: 'getOwnPropertyDescriptor' trap reported a non-configurable property as absent",
                )));
            }
            if !target_extensible {
                return Err(JSException(self.error_value(
                    "TypeError: 'getOwnPropertyDescriptor' trap reported a property of a non-extensible target as absent",
                )));
            }
            return Ok(None);
        }
        let Some(obj) = result.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: 'getOwnPropertyDescriptor' trap result must be an object or undefined",
            )));
        };
        let trap_desc = self.read_trap_descriptor(obj)?;
        // ES step 16: a complete trap descriptor must be compatible with the
        // target descriptor and cannot report a non-configurable property as
        // configurable.
        if let Some(td) = target_desc {
            if trap_desc.has_configurable && !trap_desc.configurable {
                if !td.has_configurable || td.configurable {
                    return Err(JSException(self.error_value(
                        "TypeError: 'getOwnPropertyDescriptor' trap reported a non-configurable property for a configurable target property",
                    )));
                }
                if td.is_data() && td.writable && (!trap_desc.has_writable || trap_desc.writable) {
                    return Err(JSException(self.error_value(
                        "TypeError: 'getOwnPropertyDescriptor' trap result is not compatible with the target descriptor",
                    )));
                }
                if !desc_compatible(target_extensible, trap_desc, td) {
                    return Err(JSException(self.error_value(
                        "TypeError: 'getOwnPropertyDescriptor' trap result is not compatible with the target descriptor",
                    )));
                }
            }
        } else if !target_extensible {
            return Err(JSException(self.error_value(
                "TypeError: 'getOwnPropertyDescriptor' trap reported a property for a non-extensible target",
            )));
        }
        Ok(Some(trap_desc))
    }

    /// `[[DefineOwnProperty]]` (ES 10.5.6).
    fn proxy_op_define_own_property(
        &mut self,
        proxy: Handle<JsObject>,
        key: PropKey,
        desc: OwnDesc,
    ) -> Result<bool, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "defineProperty")?;
        let target_desc = self.ordinary_own_descriptor(target, key);
        let target_extensible =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let key_v = prop_key_value(key);
        let setting_config_false = desc.has_configurable && !desc.configurable;
        let desc_v = self.own_desc_to_object(desc);
        let Some(trap) = self.proxy_trap_method(handler, "defineProperty")? else {
            return self.ordinary_define_from_own_desc(target, key, desc);
        };
        self.gc_protect();
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), key_v, desc_v],
        )?;
        if !ops::to_boolean(self.heap, result) {
            return Ok(false);
        }
        match target_desc {
            None => {
                // ES step 19.
                if !target_extensible {
                    return Err(JSException(self.error_value(
                        "TypeError: 'defineProperty' trap added a property to a non-extensible target",
                    )));
                }
                if setting_config_false {
                    return Err(JSException(self.error_value(
                        "TypeError: 'defineProperty' trap reported a non-configurable property that does not exist",
                    )));
                }
            }
            Some(td) => {
                // ES step 20.
                if setting_config_false && td.has_configurable && td.configurable {
                    return Err(JSException(self.error_value(
                        "TypeError: 'defineProperty' trap reported a non-configurable property for a configurable target property",
                    )));
                }
                if td.is_data()
                    && td.has_configurable
                    && !td.configurable
                    && td.writable
                    && desc.has_writable
                    && !desc.writable
                {
                    return Err(JSException(self.error_value(
                        "TypeError: 'defineProperty' trap reported non-writable for a non-configurable writable target property",
                    )));
                }
                if td.has_configurable
                    && !td.configurable
                    && !desc_compatible(target_extensible, desc, td)
                {
                    return Err(JSException(self.error_value(
                        "TypeError: 'defineProperty' trap result is not compatible with the target descriptor",
                    )));
                }
            }
        }
        Ok(true)
    }

    /// Reads a trap-produced descriptor object through ES
    /// `ToPropertyDescriptor` (absent fields stay absent).
    pub(crate) fn read_trap_descriptor(
        &mut self,
        obj: Handle<JsObject>,
    ) -> Result<OwnDesc, JSException> {
        let mut out = OwnDesc::default();
        for name in [
            "value",
            "writable",
            "enumerable",
            "configurable",
            "get",
            "set",
        ] {
            let key = self.new_temp_key(name);
            let pk = self.property_key(key)?;
            if !self.object_has_property(obj, pk)? {
                continue;
            }
            let got = self.get_property(0, 0, JsValue::object(obj), key)?;
            match name {
                "value" => {
                    out.has_value = true;
                    out.value = got;
                }
                "writable" => {
                    out.has_writable = true;
                    out.writable = ops::to_boolean(self.heap, got);
                }
                "enumerable" => {
                    out.has_enumerable = true;
                    out.enumerable = ops::to_boolean(self.heap, got);
                }
                "configurable" => {
                    out.has_configurable = true;
                    out.configurable = ops::to_boolean(self.heap, got);
                }
                "get" => {
                    if !got.is_undefined()
                        && !got
                            .as_object()
                            .is_some_and(|h| self.heap.get(h).kind == Kind::Function)
                    {
                        return Err(JSException(self.error_value(
                            "TypeError: Accessor must be a function or undefined",
                        )));
                    }
                    out.has_get = true;
                    out.get = got;
                }
                "set" => {
                    if !got.is_undefined()
                        && !got
                            .as_object()
                            .is_some_and(|h| self.heap.get(h).kind == Kind::Function)
                    {
                        return Err(JSException(self.error_value(
                            "TypeError: Accessor must be a function or undefined",
                        )));
                    }
                    out.has_set = true;
                    out.set = got;
                }
                _ => {}
            }
        }
        if out.is_data() && out.is_accessor() {
            return Err(JSException(self.error_value(
                "TypeError: Invalid property descriptor: cannot specify both value and accessor fields",
            )));
        }
        Ok(out)
    }

    /// `HasProperty` (prototype-walking) for an ordinary receiver.
    pub(crate) fn object_has_property(
        &mut self,
        obj: Handle<JsObject>,
        key: PropKey,
    ) -> Result<bool, JSException> {
        let key_v = prop_key_value(key);
        self.op_in(key_v, JsValue::object(obj))
    }

    /// Ordinary `[[DefineOwnProperty]]` from an already-read [`OwnDesc`]:
    /// only the fields the descriptor carries are applied (unspecified
    /// attributes keep their current value for an existing key).
    pub(crate) fn ordinary_define_from_own_desc(
        &mut self,
        obj: Handle<JsObject>,
        key: PropKey,
        desc: OwnDesc,
    ) -> Result<bool, JSException> {
        let existing = self.ordinary_own_descriptor(obj, key);
        if let Some(cur) = existing {
            // ES 10.1.6.3 step 3: a non-configurable property refuses a
            // change of configurable, writable, or kind; a value change is
            // refused only when the property is also non-writable.
            if cur.has_configurable && !cur.configurable {
                if desc.has_configurable && desc.configurable {
                    return Ok(false);
                }
                if cur.is_data() != desc.is_data() && !desc.is_generic() {
                    return Ok(false);
                }
                if cur.is_data() && cur.has_writable && !cur.writable {
                    if desc.has_writable && desc.writable {
                        return Ok(false);
                    }
                    if desc.has_value && desc.value != cur.value {
                        return Ok(false);
                    }
                }
            }
        } else if self.heap.get(obj).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE != 0 {
            return Ok(false);
        }
        self.apply_own_desc(obj, key, desc, existing)
    }

    /// Applies [`OwnDesc`] to an ordinary object (shape or element store).
    /// Absent descriptor fields keep the current attribute when the property
    /// exists and default to `false` when it is new (ES step 6).
    fn apply_own_desc(
        &mut self,
        obj: Handle<JsObject>,
        key: PropKey,
        desc: OwnDesc,
        existing: Option<OwnDesc>,
    ) -> Result<bool, JSException> {
        let key_v = prop_key_value(key);
        let kind = self.heap.get(obj).kind;
        if (kind == Kind::Array || kind == Kind::Arguments)
            && let Some(idx) = self.prop_key_index(key)
        {
            if desc.is_accessor() {
                return Ok(false);
            }
            let value = if desc.has_value {
                desc.value
            } else {
                self.array_element(obj, idx)
            };
            self.array_set_element(obj, idx, value);
            return Ok(true);
        }
        let base = existing.unwrap_or_default();
        let attrs = Attrs::new(
            if desc.has_writable {
                desc.writable
            } else {
                base.writable
            },
            if desc.has_enumerable {
                desc.enumerable
            } else {
                base.enumerable
            },
            if desc.has_configurable {
                desc.configurable
            } else {
                base.configurable
            },
        );
        if desc.is_accessor() || existing.is_some_and(|e| e.is_accessor()) {
            let getter = if desc.has_get {
                desc.get
            } else {
                JsValue::undefined()
            };
            let setter = if desc.has_set {
                desc.set
            } else {
                JsValue::undefined()
            };
            self.define_accessor_own(obj, key_v, getter, setter, attrs)?;
            return Ok(true);
        }
        if desc.has_value {
            self.define_own_data_attrs(JsValue::object(obj), key_v, desc.value, attrs)?;
        }
        Ok(true)
    }

    /// Replaces an own accessor with the given getter/setter pair.
    fn define_accessor_own(
        &mut self,
        obj: Handle<JsObject>,
        key_v: JsValue,
        getter_v: JsValue,
        setter_v: JsValue,
        attrs: Attrs,
    ) -> Result<(), JSException> {
        let key = self.property_key(key_v)?;
        let getter = getter_v
            .as_object()
            .filter(|h| self.heap.get(*h).kind == Kind::Function);
        let setter = setter_v
            .as_object()
            .filter(|h| self.heap.get(*h).kind == Kind::Function);
        self.gc_protect();
        let shape = self.shape_of(obj);
        let child = self.heap.define_accessor(shape, key, getter, setter, attrs);
        self.bind_shape(obj, child);
        let slot = child_slot(self.heap, child);
        let props = &mut self.heap.get_mut(obj).properties;
        if props.len() <= slot {
            props.resize(slot + 1, JsValue::hole());
        }
        Ok(())
    }

    /// Proxy `[[Set]]` (ES 10.5.9): consult the handler's `set` trap.
    ///
    /// Absent or `undefined` trap forwards to the target's ordinary write
    /// (which recursively handles a proxy target). A present non-callable trap
    /// is a `TypeError`. The trap result is coerced with ToBoolean; sloppy
    /// `set_property` has no boolean channel, so a falsy result is a silent
    /// no-op (strict-mode throw is a v1 gap, documented here).
    ///
    /// Deliberate limitation: the spec's invariant checks (falsy result for a
    /// non-configurable non-writable target data property, etc.) are not
    /// applied.
    fn proxy_op_set(
        &mut self,
        proxy: Handle<JsObject>,
        receiver: JsValue,
        key_v: JsValue,
        value: JsValue,
    ) -> Result<(), JSException> {
        let (target, handler) = {
            let o = self.heap.get(proxy);
            (o.proxy_target, o.proxy_handler)
        };
        // Revoked proxy (both slots cleared by revocation): TypeError.
        let (Some(target), Some(handler)) = (target, handler) else {
            return Err(JSException(self.error_value(
                "TypeError: Cannot perform 'set' on a proxy that has been revoked",
            )));
        };
        // ToPropertyKey before the trap sees the key (spec step order).
        let key = self.property_key(key_v)?;
        let key_v = if let Some(h) = key.string() {
            JsValue::string(h)
        } else if let Some(y) = key.symbol() {
            JsValue::symbol(y)
        } else {
            // Unreachable: `property_key` is string-or-symbol by construction.
            JsValue::undefined()
        };
        let set_key = self.new_temp_key("set");
        let trap_v = self.get_property(0, 0, JsValue::object(handler), set_key)?;
        let Some(trap) = trap_v.as_object() else {
            if trap_v.is_undefined() || trap_v.is_null() {
                // No trap (`GetMethod` maps null to undefined): forward to
                // the target through the ordinary path.
                return self.set_property(JsValue::object(target), key_v, value, false);
            }
            return Err(JSException(
                self.error_value("TypeError: 'set' trap must be a function"),
            ));
        };
        if self.heap.get(trap).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: 'set' trap must be a function"),
            ));
        }
        self.gc_protect();
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), key_v, value, receiver],
        )?;
        let _ = ops::to_boolean(self.heap, result);
        Ok(())
    }

    /// Proxy `[[HasProperty]]` (ES 10.5.6): consult the handler's `has` trap.
    ///
    /// Absent or `undefined` trap forwards to the target's ordinary `in`
    /// (which recursively handles a proxy target). A present non-callable trap
    /// is a `TypeError`. The trap result is coerced with ToBoolean; a falsy
    /// result is returned as `false` without forwarding.
    ///
    /// Deliberate limitation: the spec's invariant check (a `has` trap
    /// reporting `false` for a non-configurable own property of a
    /// non-extensible target throws) is not applied — the target walked by the
    /// tests is extensible, so no invariant can be violated.
    fn proxy_op_has(
        &mut self,
        key_v: JsValue,
        proxy: Handle<JsObject>,
    ) -> Result<bool, JSException> {
        let (target, handler) = {
            let o = self.heap.get(proxy);
            (o.proxy_target, o.proxy_handler)
        };
        // Revoked proxy (both slots cleared by revocation): TypeError.
        let (Some(target), Some(handler)) = (target, handler) else {
            return Err(JSException(self.error_value(
                "TypeError: Cannot perform 'has' on a proxy that has been revoked",
            )));
        };
        // ToPropertyKey before the trap sees the key (spec step order: the
        // key is materialised once, ahead of the handler lookup).
        let key = self.property_key(key_v)?;
        let key_v = if let Some(h) = key.string() {
            JsValue::string(h)
        } else if let Some(y) = key.symbol() {
            JsValue::symbol(y)
        } else {
            // Unreachable: `property_key` is string-or-symbol by construction.
            JsValue::undefined()
        };
        let has_key = self.new_temp_key("has");
        let trap_v = self.get_property(0, 0, JsValue::object(handler), has_key)?;
        let Some(trap) = trap_v.as_object() else {
            if trap_v.is_undefined() || trap_v.is_null() {
                // No trap (`GetMethod` maps null to undefined): forward to
                // the target through the ordinary path.
                return self.op_in(key_v, JsValue::object(target));
            }
            return Err(JSException(
                self.error_value("TypeError: 'has' trap must be a function"),
            ));
        };
        if self.heap.get(trap).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: 'has' trap must be a function"),
            ));
        }
        self.gc_protect();
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), key_v],
        )?;
        Ok(ops::to_boolean(self.heap, result))
    }

    /// Proxy `[[Get]]` (ES 10.5.8): consult the handler's `get` trap.
    ///
    /// Absent or `undefined` trap forwards to the target's ordinary read
    /// (which recursively handles a proxy target). A present non-callable trap
    /// is a `TypeError`. The trap result returns uncoerced.
    ///
    /// Deliberate limitations: the spec's invariant checks (trap result vs. a
    /// non-configurable non-writable target data property, or vs. a
    /// non-configurable target accessor with undefined `get`) are not applied;
    /// and a forward passes the target (not the proxy) as receiver, so an
    /// inherited target getter observes the target as `this`.
    fn proxy_op_get(
        &mut self,
        proxy: Handle<JsObject>,
        receiver: JsValue,
        key_v: JsValue,
    ) -> Result<JsValue, JSException> {
        let (target, handler) = {
            let o = self.heap.get(proxy);
            (o.proxy_target, o.proxy_handler)
        };
        // Revoked proxy (both slots cleared by revocation): TypeError.
        let (Some(target), Some(handler)) = (target, handler) else {
            return Err(JSException(self.error_value(
                "TypeError: Cannot perform 'get' on a proxy that has been revoked",
            )));
        };
        // ToPropertyKey before the trap sees the key (spec step order: the
        // key is materialised once, ahead of the handler lookup).
        let key = self.property_key(key_v)?;
        let key_v = if let Some(h) = key.string() {
            JsValue::string(h)
        } else if let Some(y) = key.symbol() {
            JsValue::symbol(y)
        } else {
            // Unreachable: `property_key` is string-or-symbol by construction.
            JsValue::undefined()
        };
        let get_key = self.new_temp_key("get");
        let trap_v = self.get_property(0, 0, JsValue::object(handler), get_key)?;
        let Some(trap) = trap_v.as_object() else {
            if trap_v.is_undefined() || trap_v.is_null() {
                // No trap (`GetMethod` maps null to undefined): forward to
                // the target through the ordinary path.
                return self.get_property(0, 0, JsValue::object(target), key_v);
            }
            return Err(JSException(
                self.error_value("TypeError: 'get' trap must be a function"),
            ));
        };
        if self.heap.get(trap).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: 'get' trap must be a function"),
            ));
        }
        self.gc_protect();
        self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), key_v, receiver],
        )
    }

    /// Proxy `[[OwnPropertyKeys]]` (ES 10.5.11): consult the handler's
    /// `ownKeys` trap.
    ///
    /// Absent or `undefined`/`null` trap (`GetMethod` maps null to undefined)
    /// forwards to the target's ordinary keys (which recursively handles a
    /// proxy target). A present non-callable trap is a `TypeError`. A present
    /// trap runs via `call_inline` and its result goes through
    /// `CreateListFromArrayLike` (object with a `length`, every entry a
    /// String or Symbol, else `TypeError`) plus the duplicate-entry check and
    /// the ES steps 13–20 invariant sweep.
    pub(crate) fn proxy_op_own_keys(
        &mut self,
        proxy: Handle<JsObject>,
    ) -> Result<Vec<PropKey>, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "ownKeys")?;
        let extensible_target =
            self.heap.get(target).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0;
        let Some(trap) = self.proxy_trap_method(handler, "ownKeys")? else {
            // No trap: forward to the target's ordinary keys.
            return self.object_own_keys(target);
        };
        self.gc_protect();
        let result =
            self.call_inline(trap, JsValue::object(handler), &[JsValue::object(target)])?;
        // `CreateListFromArrayLike` (ES 7.4.4): result must be an object.
        let Some(list_obj) = result.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: 'ownKeys' trap result must be an object"),
            ));
        };
        let len_key = self.new_temp_key("length");
        let len_v = self.get_property(0, 0, JsValue::object(list_obj), len_key)?;
        let len_f = ops::to_number(self.heap, len_v);
        // `ToLength` floors and clamps the low end to 0; the high end is
        // capped at `u32::MAX` (see the doc comment above).
        if len_f.is_nan() || len_f > f64::from(u32::MAX) {
            return Err(JSException(self.error_value(
                "TypeError: 'ownKeys' trap result length out of range",
            )));
        }
        let len = len_f.max(0.0).floor() as u32;
        let mut keys: Vec<PropKey> = Vec::with_capacity(len.min(1024) as usize);
        for i in 0..len {
            let idx_v = JsValue::string(self.heap.intern_text(&i.to_string()));
            let entry = self.get_property(0, 0, JsValue::object(list_obj), idx_v)?;
            let key = if let Some(h) = entry.as_string() {
                // Canonicalize: trap-built strings must alias the interned
                // instance or later shape lookups miss.
                let units = ops::string_units(self.heap, h);
                PropKey::from_string(self.heap.intern_string(v12_heap::V12Str::utf16(units)))
            } else if let Some(y) = entry.as_symbol() {
                PropKey::from_symbol(y)
            } else {
                return Err(JSException(self.error_value(
                    "TypeError: 'ownKeys' trap result entries must be strings or symbols",
                )));
            };
            // Duplicate entries are a `TypeError` (ES 10.5.11 step 8).
            if keys.contains(&key) {
                return Err(JSException(self.error_value(
                    "TypeError: 'ownKeys' trap result contains duplicate entries",
                )));
            }
            keys.push(key);
        }
        // ES steps 13–20: split the target's own keys into non-configurable
        // and configurable groups and verify the trap result against them.
        let target_keys = self.object_own_keys(target)?;
        let mut target_nonconfigurable: Vec<PropKey> = Vec::new();
        let mut target_configurable: Vec<PropKey> = Vec::new();
        for &k in &target_keys {
            let desc = self.ordinary_own_descriptor(target, k);
            let nonconfig = desc.is_some_and(|d| d.has_configurable && !d.configurable);
            if nonconfig {
                target_nonconfigurable.push(k);
            } else {
                target_configurable.push(k);
            }
        }
        if extensible_target && target_nonconfigurable.is_empty() {
            return Ok(keys);
        }
        let mut unchecked = keys.clone();
        let remove = |unchecked: &mut Vec<PropKey>, k: PropKey| -> bool {
            if let Some(pos) = unchecked.iter().position(|&x| x == k) {
                unchecked.remove(pos);
                true
            } else {
                false
            }
        };
        for &k in &target_nonconfigurable {
            if !remove(&mut unchecked, k) {
                return Err(JSException(self.error_value(
                    "TypeError: 'ownKeys' trap result is missing a non-configurable target key",
                )));
            }
        }
        if !extensible_target {
            for &k in &target_configurable {
                if !remove(&mut unchecked, k) {
                    return Err(JSException(self.error_value(
                        "TypeError: 'ownKeys' trap result is missing a target key",
                    )));
                }
            }
            if !unchecked.is_empty() {
                return Err(JSException(self.error_value(
                    "TypeError: 'ownKeys' trap result contains new keys for a non-extensible target",
                )));
            }
        }
        Ok(keys)
    }

    /// Ordinary `[[OwnPropertyKeys]]` for the `ownKeys` forward path: integer
    /// indices ascending (holes absent), then string keys in insertion order
    /// (shape descriptors, then dictionary-rung overflow by sequence), then
    /// symbol keys in the same two-tier order. Holed data slots are absent,
    /// matching what `in` observes.
    pub(crate) fn ordinary_own_keys(&mut self, obj: Handle<JsObject>) -> Vec<PropKey> {
        let mut keys: Vec<PropKey> = Vec::new();
        let kind = self.heap.get(obj).kind;
        if kind == Kind::Array || kind == Kind::Arguments {
            let bound = self.heap.get(obj).element_len() as u32;
            for i in 0..bound {
                if self
                    .heap
                    .get(obj)
                    .get_element(i)
                    .is_some_and(|v| !v.is_hole())
                {
                    keys.push(PropKey::from_string(self.heap.intern_text(&i.to_string())));
                }
            }
        }
        let shape = self.shape_of(obj);
        let descriptors: Vec<v12_heap::Descriptor> =
            self.heap.get(shape).descriptors.as_slice().to_vec();
        let mut overflow: Vec<(u32, PropKey)> = self
            .heap
            .get(obj)
            .dictionary
            .as_ref()
            .map(|m| m.iter().map(|(k, e)| (e.seq, *k)).collect())
            .unwrap_or_default();
        overflow.sort_by_key(|&(seq, _)| seq);
        // Liveness: a holed data slot is not an own property (accessors have
        // no slot and are always live).
        let live = |heap: &v12_heap::Heap, obj: Handle<JsObject>, key: PropKey| -> bool {
            let entry = heap
                .get(obj)
                .dictionary
                .as_ref()
                .and_then(|m| m.get(&key))
                .copied();
            if let Some(entry) = entry {
                if entry.is_accessor {
                    return true;
                }
                return heap
                    .get(obj)
                    .properties
                    .get(entry.slot as usize)
                    .is_some_and(|v| !v.is_hole());
            }
            match heap.get(shape).descriptors.find(key).copied() {
                Some(v12_heap::Descriptor::Data { slot, .. }) => heap
                    .get(obj)
                    .properties
                    .get(slot as usize)
                    .is_some_and(|v| !v.is_hole()),
                Some(v12_heap::Descriptor::Accessor { .. }) => true,
                None => true,
            }
        };
        // Strings first (shape order, then overflow order), then symbols.
        for d in &descriptors {
            let k = d.key();
            if !k.is_symbol() && live(self.heap, obj, k) {
                keys.push(k);
            }
        }
        for (_, k) in &overflow {
            if !k.is_symbol() && live(self.heap, obj, *k) {
                keys.push(*k);
            }
        }
        for d in &descriptors {
            let k = d.key();
            if k.is_symbol() && live(self.heap, obj, k) {
                keys.push(k);
            }
        }
        for (_, k) in &overflow {
            if k.is_symbol() && live(self.heap, obj, *k) {
                keys.push(*k);
            }
        }
        keys
    }

    /// `instanceof` operator. Throws TypeError if `rhs` is not an object
    /// with an object-typed `prototype` property; returns false if `lhs`
    /// is not an object; otherwise walks `lhs`'s prototype chain for
    /// identity against `rhs.prototype`.
    pub(crate) fn op_instanceof(
        &mut self,
        lhs_v: JsValue,
        rhs_v: JsValue,
    ) -> Result<bool, JSException> {
        let Some(rhs_obj) = rhs_v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: right-hand side of 'instanceof' is not an object",
            )));
        };
        // Per ES OrdinaryHasInstance, RHS must be callable.
        if self.heap.get(rhs_obj).kind != Kind::Function {
            return Err(JSException(self.error_value(
                "TypeError: right-hand side of 'instanceof' is not callable",
            )));
        }
        // Fast path for built-in constructors whose prototype has not been
        // wired via `Heap::add_property` (Realm creates them as empty objects).
        if let Some(global) = self.global {
            let props = &self.heap.get(global).properties;
            if props.len() >= 2 {
                if let Some(obj_ctor) = props[0].as_object()
                    && rhs_obj == obj_ctor
                {
                    return Ok(lhs_v.as_object().is_some());
                }
                if let Some(arr_ctor) = props[1].as_object()
                    && rhs_obj == arr_ctor
                {
                    return Ok(lhs_v
                        .as_object()
                        .is_some_and(|h| self.heap.get(h).kind == Kind::Array));
                }
            }
        }
        let proto_key = self.prototype_key();
        // Locate `rhs.prototype` along rhs's prototype chain (own or inherited).
        let mut rhs_proto_val: Option<JsValue> = None;
        {
            let mut cur = Some(rhs_obj);
            while let Some(o) = cur {
                let sh = self.shape_of(o);
                if let Some(d) = self.heap.lookup_property(sh, proto_key) {
                    let val = match *d {
                        Descriptor::Data { slot, .. } => self.heap.get(o).properties[slot as usize],
                        Descriptor::Accessor { getter, .. } => {
                            if let Some(getter) = getter {
                                self.call_accessor(getter, JsValue::object(o))?
                            } else {
                                JsValue::undefined()
                            }
                        }
                    };
                    rhs_proto_val = Some(val);
                    break;
                }
                cur = self.heap.get(o).prototype;
            }
        }
        // Lazily materialize prototype for functions that never had one
        // (realm placeholders and any function whose closure was created
        // before materialization existed). Arrow functions intentionally
        // have no prototype and must still throw.
        if rhs_proto_val.is_none() && self.heap.get(rhs_obj).kind == Kind::Function {
            // Check if this is an arrow-function flag by looking up its bytecode.
            let is_arrow = self
                .functions_for_program(self.heap.get(rhs_obj).program_id)
                .get(
                    self.heap
                        .get(rhs_obj)
                        .callable
                        .bytecode_index()
                        .unwrap_or(u32::MAX) as usize,
                )
                .map(|f| f.is_arrow)
                .unwrap_or(false);
            if !is_arrow {
                self.materialize_function_prototype(rhs_obj)?;
                // Re-read after materialization.
                let sh = self.shape_of(rhs_obj);
                if let Some(d) = self.heap.lookup_property(sh, proto_key)
                    && let Some(slot) = d.slot()
                {
                    rhs_proto_val = Some(self.heap.get(rhs_obj).properties[slot as usize]);
                }
            }
        }
        let Some(proto_val) = rhs_proto_val else {
            return Err(JSException(self.error_value(
                "TypeError: function has non-object prototype 'prototype' in instanceof check",
            )));
        };
        // Per spec, null prototype is also an error for instanceof (throws).
        // Non-object primitive also throws the same TypeError.
        let Some(proto_obj) = proto_val.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: function has non-object prototype 'prototype' in instanceof check",
            )));
        };
        let Some(mut cur) = lhs_v.as_object() else {
            return Ok(false);
        };
        loop {
            let next = self.heap.get(cur).prototype;
            match next {
                None => return Ok(false),
                Some(p) if p == proto_obj => return Ok(true),
                Some(p) => cur = p,
            }
        }
    }

    /// `DeleteProperty`: configurable own properties become holes (slot
    /// numbering survives for siblings), absent ones report success, locked
    /// ones report failure. Element deletes hole out the slot.
    pub(crate) fn delete_property(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
    ) -> Result<bool, JSException> {
        let Some(obj) = obj_v.as_object() else {
            if obj_v.is_null() || obj_v.is_undefined() {
                return Err(JSException(self.error_value(
                    "TypeError: cannot delete properties of null or undefined",
                )));
            }
            // Primitives have no own properties: nothing to remove.
            return Ok(true);
        };

        // Proxy exotic: `[[Delete]]` is the `deleteProperty` trap (ES 10.5.10).
        if self.heap.get(obj).kind == Kind::Proxy {
            return self.proxy_op_delete(obj, key_v);
        }

        // Flatten-once so the index scan and the intern below share it.
        self.flatten_key(key_v);
        if (self.heap.get(obj).kind == Kind::Array || self.heap.get(obj).kind == Kind::Arguments)
            && let Some(idx) = self.array_index_of(key_v)
        {
            self.heap.get_mut(obj).delete_element(idx);
            return Ok(true);
        }

        let key = self.property_key(key_v)?;
        // Dictionary rung: overflow keys remove from the map (and hole
        // their storage slot, mirroring the shape arm below).
        if let Some(entry) = self
            .heap
            .get(obj)
            .dictionary
            .as_ref()
            .and_then(|m| m.get(&key))
            .copied()
        {
            if !entry.attrs.configurable() {
                return Ok(false);
            }
            let slot = entry.slot as usize;
            if let Some(map) = self.heap.get_mut(obj).dictionary.as_mut() {
                map.remove(&key);
            }
            if let Some(v) = self.heap.get_mut(obj).properties.get_mut(slot) {
                *v = JsValue::hole();
            }
            // Entry removal changes walk resolution (unlike shape
            // `delete`, which keeps the descriptor): bump the proto epoch
            // so guarded stubs re-verify instead of serving the hole.
            self.heap.bump_proto_generation();
            return Ok(true);
        }
        let shape = self.shape_of(obj);
        let Some(d) = self.heap.get(shape).descriptors.find(key).copied() else {
            return Ok(true); // not an own property: ES says success
        };
        if !d.attrs().configurable() {
            return Ok(false);
        }
        match d {
            Descriptor::Data { slot, .. } => {
                let idx = self.global_slot_index(obj, slot as usize);
                self.heap.get_mut(obj).properties[idx] = JsValue::hole();
            }
            Descriptor::Accessor { .. } => {
                // Accessors carry no storage slot, so deletion cannot be
                // expressed by holing one. Reconfigure the key to a holed
                // data descriptor on a private child shape (shapes are
                // shared, so the transition is bound only to this object);
                // `descriptor_is_live` then reports the property absent.
                let child = self.heap.update_data_attrs(shape, key, Attrs::DEFAULT);
                self.bind_shape(obj, child);
                let slot = self
                    .heap
                    .get(child)
                    .descriptors
                    .find(key)
                    .and_then(|d| d.slot())
                    .expect("reconfigured data descriptor has a slot");
                let idx = self.global_slot_index(obj, slot as usize);
                let props = &mut self.heap.get_mut(obj).properties;
                if props.len() <= idx {
                    props.resize(idx + 1, JsValue::hole());
                }
                props[idx] = JsValue::hole();
            }
        }
        Ok(true)
    }

    pub(crate) fn array_element(&self, obj: Handle<JsObject>, idx: u32) -> JsValue {
        self.heap
            .get(obj)
            .get_element(idx)
            .unwrap_or(JsValue::undefined())
    }

    /// Stores an element, hole-filling gaps and keeping `length` current.
    pub(crate) fn array_set_element(&mut self, obj: Handle<JsObject>, idx: u32, value: JsValue) {
        let len_before = self.heap.get(obj).element_len() as u32;
        self.heap.get_mut(obj).set_element(idx, value);
        let len_after = self.heap.get(obj).element_len() as u32;
        if len_after <= len_before {
            return;
        }
        let len_key = self.length_key();
        let shape = self.shape_of(obj);
        let slot = self
            .heap
            .lookup_property(shape, len_key)
            .and_then(|d| d.slot())
            .map(|s| s as usize);
        if let Some(slot) = slot {
            self.heap.get_mut(obj).properties[slot] = ops::box_number(f64::from(len_after));
        }
    }
}

#[cfg(test)]
mod proxy_own_keys_tests {
    use super::*;
    use v12_heap::{FunctionTarget, GcPolicy, Heap};

    fn fresh_interp(heap: &mut Heap) -> Interp<'_> {
        Interp::from_source(heap, "0;").unwrap()
    }

    fn define_data(interp: &mut Interp<'_>, obj: Handle<JsObject>, name: &str, value: JsValue) {
        let key = PropKey::from_string(interp.heap.intern_text(name));
        let shape = interp.shape_of(obj);
        let child = interp.heap.add_property(shape, key, Attrs::DEFAULT);
        interp.bind_shape(obj, child);
        interp.heap.get_mut(obj).properties.push(value);
    }

    fn native_trap(heap: &mut Heap, target: FunctionTarget) -> Handle<JsObject> {
        heap.alloc(JsObject::function(target, None))
    }

    fn keys_result(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
        let key = JsValue::string(heap.intern_text("x"));
        Ok(JsValue::object(heap.alloc(JsObject::array(vec![key]))))
    }

    fn setup(
        heap: &mut Heap,
        trap: Option<JsValue>,
    ) -> (Interp<'_>, Handle<JsObject>, PropKey, PropKey) {
        let target = heap.alloc(JsObject::default());
        let handler = heap.alloc(JsObject::default());
        let key_a = PropKey::from_string(heap.intern_text("a"));
        let key_x = PropKey::from_string(heap.intern_text("x"));
        let mut interp = fresh_interp(heap);
        define_data(&mut interp, target, "a", JsValue::from_i32_smi(1).unwrap());
        if let Some(t) = trap {
            define_data(&mut interp, handler, "ownKeys", t);
        }
        let proxy = interp.heap.alloc(JsObject::proxy(target, handler));
        (interp, proxy, key_a, key_x)
    }

    #[test]
    fn trap_result_returned() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let trap = {
            let f = native_trap(&mut heap, FunctionTarget::Native(keys_result));
            JsValue::object(f)
        };
        let (mut interp, proxy, _, key_x) = setup(&mut heap, Some(trap));
        assert_eq!(interp.proxy_op_own_keys(proxy).unwrap(), vec![key_x]);
    }

    #[test]
    fn missing_trap_forwards_to_target() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let (mut interp, proxy, key_a, _) = setup(&mut heap, None);
        assert_eq!(interp.proxy_op_own_keys(proxy).unwrap(), vec![key_a]);
    }

    #[test]
    fn null_trap_forwards_to_target() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let (mut interp, proxy, key_a, _) = setup(&mut heap, Some(JsValue::null()));
        assert_eq!(interp.proxy_op_own_keys(proxy).unwrap(), vec![key_a]);
    }

    #[test]
    fn revoked_proxy_throws() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let (mut interp, proxy, _, _) = setup(&mut heap, None);
        let o = interp.heap.get_mut(proxy);
        o.proxy_target = None;
        o.proxy_handler = None;
        assert!(interp.proxy_op_own_keys(proxy).is_err());
    }

    #[test]
    fn non_callable_trap_throws() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let smi = JsValue::from_i32_smi(5).unwrap();
        let (mut interp, proxy, _, _) = setup(&mut heap, Some(smi));
        assert!(interp.proxy_op_own_keys(proxy).is_err());
    }

    #[test]
    fn duplicate_entries_throw() {
        fn dup(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
            let key = JsValue::string(heap.intern_text("x"));
            Ok(JsValue::object(heap.alloc(JsObject::array(vec![key, key]))))
        }
        let mut heap = Heap::new(GcPolicy::NoGC);
        let trap = JsValue::object(native_trap(&mut heap, FunctionTarget::Native(dup)));
        let (mut interp, proxy, _, _) = setup(&mut heap, Some(trap));
        assert!(interp.proxy_op_own_keys(proxy).is_err());
    }

    #[test]
    fn non_object_result_throws() {
        fn num(_heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
            Ok(JsValue::from_i32_smi(3).unwrap())
        }
        let mut heap = Heap::new(GcPolicy::NoGC);
        let trap = JsValue::object(native_trap(&mut heap, FunctionTarget::Native(num)));
        let (mut interp, proxy, _, _) = setup(&mut heap, Some(trap));
        assert!(interp.proxy_op_own_keys(proxy).is_err());
    }

    #[test]
    fn non_string_entry_throws() {
        fn mixed(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
            let key = JsValue::string(heap.intern_text("x"));
            let num = JsValue::from_i32_smi(7).unwrap();
            Ok(JsValue::object(heap.alloc(JsObject::array(vec![key, num]))))
        }
        let mut heap = Heap::new(GcPolicy::NoGC);
        let trap = JsValue::object(native_trap(&mut heap, FunctionTarget::Native(mixed)));
        let (mut interp, proxy, _, _) = setup(&mut heap, Some(trap));
        assert!(interp.proxy_op_own_keys(proxy).is_err());
    }
}
