//! Object built-ins.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.

use v12_heap::{Handle, JsObject, JsValue, PropKey, V12Str};
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

/// `Object.create(proto, [properties])` – makes a new ordinary object with
/// `proto` as its prototype, then (ES 20.1.2.2 step 3) applies the optional
/// `properties` object through `ObjectDefineProperties`. `proto` may be an
/// object or `null`.
pub fn object_create(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let proto = args.first().copied().unwrap_or(JsValue::undefined());
    let proto_handle = if proto.is_null() {
        None
    } else if let Some(h) = proto.as_object() {
        Some(h)
    } else if proto.is_undefined() {
        // ES 20.1.2.2 step 1: `undefined` is treated like `null`.
        None
    } else {
        return Err(ctx.type_error("TypeError: Object.create prototype must be object or null"));
    };
    let obj = ctx.heap.alloc(JsObject::environment(0, proto_handle));
    if let Some(props) = args.get(1).copied()
        && !props.is_undefined()
    {
        apply_define_properties(ctx, obj, props)?;
    }
    Ok(JsValue::object(obj))
}

/// `Object([value])` – callable/constructible form: `undefined`/`null`
/// produce a fresh ordinary object; a primitive wraps into its kind object
/// (v1: a plain object carrying the primitive as storage is not modeled, so
/// primitives return a fresh object); an object passes through.
pub fn object_construct(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or(JsValue::undefined());
    if let Some(obj) = value.as_object() {
        return Ok(JsValue::object(obj));
    }
    let obj = ctx.heap.alloc(JsObject::default());
    Ok(JsValue::object(obj))
}

/// `Object.getPrototypeOf(obj)` – returns the prototype.
pub fn object_get_prototype_of(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.getPrototypeOf called on non-object"))?;
    match ctx.heap.get(obj).prototype {
        Some(p) => Ok(JsValue::object(p)),
        None => Ok(JsValue::null()),
    }
}

/// ES 6.2.5.5 `ToPropertyDescriptor`: reads `value`/`writable`/`enumerable`/
/// `configurable`/`get`/`set`, keeping each field *absent* when the source
/// object does not have it. Throws `TypeError` on a `get`/`set` that is
/// present but neither callable nor `undefined`, and on a descriptor mixing
/// the data and accessor halves.
fn to_property_descriptor(
    ctx: &mut Ctx,
    v: JsValue,
) -> Result<crate::internal_methods::FullDescriptor, Throw> {
    let Some(obj) = v.as_object() else {
        return Err(ctx.type_error("TypeError: Property description must be an object"));
    };
    let mut desc = crate::internal_methods::FullDescriptor::default();
    for name in [
        "value",
        "writable",
        "enumerable",
        "configurable",
        "get",
        "set",
    ] {
        let key = ctx.heap.intern_text(name);
        let pk = PropKey::from_string(key);
        // ES step 3: `HasProperty(Obj, name)` — an *inherited* field counts,
        // so this must walk the prototype chain, not just the own shape.
        let present =
            crate::internal_methods::dispatch_has(&mut *ctx.heap, obj, pk).map_err(Throw::Value)?;
        if !present {
            continue;
        }
        let got =
            crate::internal_methods::dispatch_get(&mut *ctx.heap, obj, pk, JsValue::object(obj))
                .map_err(Throw::Value)?;
        match name {
            "value" => desc.value = Some(got),
            "writable" => desc.writable = Some(super::boolean::to_boolean(ctx, got)),
            "enumerable" => desc.enumerable = Some(super::boolean::to_boolean(ctx, got)),
            "configurable" => desc.configurable = Some(super::boolean::to_boolean(ctx, got)),
            "get" | "set" => {
                // ES step 7/8: `undefined` is allowed (means "no accessor
                // function"); any other non-callable is a TypeError.
                if !got.is_undefined() {
                    let callable = got
                        .as_object()
                        .is_some_and(|h| ctx.heap.get(h).kind == v12_heap::Kind::Function);
                    if !callable {
                        return Err(ctx.type_error(
                            "TypeError: Accessor property must be a function or undefined",
                        ));
                    }
                }
                if name == "get" {
                    desc.get = Some(got);
                } else {
                    desc.set = Some(got);
                }
            }
            _ => unreachable!(),
        }
    }
    // ES step 10: a descriptor with both halves is invalid.
    if desc.is_data() && desc.is_accessor() {
        return Err(ctx.type_error(
            "TypeError: Invalid property descriptor: cannot both specify accessors and a value or writable attribute",
        ));
    }
    Ok(desc)
}

/// `Object.defineProperty(obj, key, descriptor)` – defines a property via
/// shape, honoring the descriptor's `value`/`writable`/`enumerable`/
/// `configurable` flags (absent flags default to `false`).
pub fn object_define_property(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    if args.len() < 2 {
        return Err(ctx.type_error("TypeError: Object.defineProperty requires 2 arguments"));
    }
    let obj = args[0]
        .as_object()
        .ok_or_else(|| ctx.type_error("TypeError: Object.defineProperty called on non-object"))?;
    let key = property_key(ctx, args[1]).map_err(Throw::Value)?;

    // Delegate to the ordinary [[DefineOwnProperty]] implementation: it
    // dispatches on object kind (arrays, arguments exotics) and handles the
    // shape extension + binding for new keys.
    let descriptor = if args.len() >= 3 {
        to_property_descriptor(ctx, args[2])?
    } else {
        crate::internal_methods::FullDescriptor::default()
    };
    // Array `"length"` has a dedicated `[[DefineOwnProperty]]` (ES 10.4.2.4
    // `ArraySetLength`): a shrinking write deletes trailing elements, a
    // non-integral `[[Value]]` is a RangeError, and an accessor descriptor is
    // rejected. Conservative only in that per-element attributes are not
    // modeled (v1 reports element attrs as all-true), so no deletion fails.
    let defined = match apply_array_length(ctx, obj, key, &descriptor)? {
        Some(outcome) => outcome,
        None => {
            crate::internal_methods::apply_property_descriptor(&mut *ctx.heap, obj, key, descriptor)
                .map_err(Throw::Value)?
        }
    };
    if !defined {
        return Err(
            ctx.type_error("TypeError: Cannot redefine property: Invalid property definition")
        );
    }
    Ok(JsValue::object(obj))
}

/// ES `ToUint32` (6.1.6): modulo 2^32 of the truncated number; NaN and the
/// infinities map to 0.
fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    let m = n.trunc() % 4_294_967_296.0;
    if m < 0.0 {
        (m + 4_294_967_296.0) as u32
    } else {
        m as u32
    }
}

/// ES 10.4.2.4 `ArraySetLength` for an Array's `"length"` own property.
/// Returns `None` when the receiver is not an Array or `key` is not
/// `"length"` (the caller then uses the ordinary descriptor path); otherwise
/// the spec's boolean outcome. A `[[Value]]` that does not round-trip through
/// `ToUint32` raises `RangeError` (step 5), not `TypeError`.
fn apply_array_length(
    ctx: &mut Ctx,
    obj: Handle<v12_heap::JsObject>,
    key: PropKey,
    desc: &crate::internal_methods::FullDescriptor,
) -> Result<Option<bool>, Throw> {
    if ctx.heap.get(obj).kind != v12_heap::Kind::Array {
        return Ok(None);
    }
    let length_key = PropKey::from_string(ctx.heap.intern_text("length"));
    if key != length_key {
        return Ok(None);
    }
    // Step 3.a.i: an accessor descriptor on `length` is rejected.
    if desc.is_accessor() {
        return Ok(Some(false));
    }
    // Step 1: no `[[Value]]`; the flag-only case takes the ordinary path.
    let Some(value) = desc.value else {
        return Ok(None);
    };
    let number_len = ctx.to_number(value);
    let new_len = to_uint32(number_len);
    // Step 5: `ToUint32` must round-trip through `ToNumber`.
    if f64::from(new_len) != number_len {
        return Err(ctx.range_error("RangeError: Invalid array length"));
    }
    // Step 7: read the current `length` value and its `[[Writable]]`.
    let (old_len, writable) = match array_length_state(ctx, obj) {
        Some(state) => state,
        // No own `length` (an embedder-built array): fall through to the
        // ordinary define path.
        None => return Ok(None),
    };
    if u64::from(new_len) >= old_len {
        // Step 9: grow/same — publish the coerced value, flags per `desc`.
        return Ok(Some(define_coerced_length(ctx, obj, key, desc, new_len)?));
    }
    // Step 10: shrinking a non-writable length fails.
    if !writable {
        return Ok(Some(false));
    }
    // Step 12: delete elements from the top down, then shrink the store so
    // `element_len()` (the array-length view) reports the new length.
    let mut idx = old_len;
    while idx > u64::from(new_len) {
        idx -= 1;
        ctx.heap.get_mut(obj).delete_element(idx as u32);
    }
    let mut kept: Vec<JsValue> = ctx.heap.get(obj).elements_snapshot();
    kept.truncate(new_len as usize);
    ctx.heap.get_mut(obj).replace_elements(kept);
    Ok(Some(define_coerced_length(ctx, obj, key, desc, new_len)?))
}

/// Publishes the `length` value (coerced to `new_len`) and the descriptor's
/// flags through the ordinary path, preserving the current `[[Writable]]`
/// when `desc` omits it.
fn define_coerced_length(
    ctx: &mut Ctx,
    obj: Handle<v12_heap::JsObject>,
    key: PropKey,
    desc: &crate::internal_methods::FullDescriptor,
    new_len: u32,
) -> Result<bool, Throw> {
    let len_v = JsValue::from_i32_smi(new_len as i32)
        .unwrap_or_else(|| JsValue::from_f64(f64::from(new_len)));
    let mut coerced = *desc;
    coerced.value = Some(len_v);
    crate::internal_methods::apply_property_descriptor(&mut *ctx.heap, obj, key, coerced)
        .map_err(Throw::Value)
}

/// The array's `length` value and `[[Writable]]` from its shape descriptor
/// (`None` when the object has no own `"length"` data property).
fn array_length_state(ctx: &mut Ctx, obj: Handle<v12_heap::JsObject>) -> Option<(u64, bool)> {
    let key = PropKey::from_string(ctx.heap.intern_text("length"));
    let shape = ctx.heap.shape_of(obj);
    let desc = ctx.heap.lookup_property(shape, key).copied()?;
    let slot = desc.slot()? as usize;
    let v = ctx.heap.get(obj).properties.get(slot).copied()?;
    let n = v.as_smi().map(f64::from).or(v.as_f64()).unwrap_or(0.0);
    let len = if n.is_finite() && n >= 0.0 {
        n as u64
    } else {
        0
    };
    Some((len, desc.attrs().writable()))
}

/// ES `ToObject` for the own-property queries. Returns the coercible receiver
/// (objects pass through; strings/numbers/booleans/symbols read their own
/// properties through the primitive surface), or `Err` for `null`/`undefined`
/// which the spec maps to a thrown `TypeError`.
fn to_object_arg(ctx: &mut Ctx, arg: JsValue, method: &str) -> Result<JsValue, Throw> {
    if arg.is_null() || arg.is_undefined() {
        return Err(ctx.type_error(format!("TypeError: Object.{method} called on non-object")));
    }
    Ok(arg)
}

/// Own-property queries on a primitive: the spec wraps it in a
/// `String`/`Number`/`Boolean`/`Symbol`/`BigInt` object, which has no own
/// properties except a string's code-unit indices (and `"length"` for
/// `getOwnPropertyNames`). This helper answers the index/`length` surface so
/// the callers do not have to spell the wrapper out per method.
fn primitive_own_index_count(ctx: &mut Ctx, v: JsValue) -> Option<usize> {
    v.as_string().map(|h| ctx.heap.get(h).len())
}

pub fn object_keys(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let arg = args.first().copied().unwrap_or(JsValue::undefined());
    let arg = to_object_arg(ctx, arg, "keys")?;
    let keys: Vec<JsValue> = match arg.as_object() {
        None => {
            // Primitive receiver: a string contributes its indices (all
            // enumerable, per `String` exotic [[OwnPropertyKeys]]); the
            // other primitives contribute nothing.
            (0..primitive_own_index_count(ctx, arg).unwrap_or(0))
                .map(|i| JsValue::string(ctx.heap.intern_text(&i.to_string())))
                .collect()
        }
        Some(obj) => collect_enumerable_string_keys(ctx, obj)
            .into_iter()
            .map(JsValue::string)
            .collect(),
    };
    let arr = ctx.heap.alloc(v12_heap::JsObject::array(keys));
    ctx.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

pub fn object_values(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let arg = args.first().copied().unwrap_or(JsValue::undefined());
    let arg = to_object_arg(ctx, arg, "values")?;
    let vals: Vec<JsValue> = match arg.as_object() {
        None => {
            let Some(h) = arg.as_string() else {
                return Ok(array_value(ctx, Vec::new()));
            };
            // String indices read their code units; the `length` own property
            // is non-enumerable and therefore omitted.
            let len = ctx.heap.get(h).len();
            (0..len)
                .map(|i| JsValue::string(helpers::string_index_unit(&mut *ctx.heap, h, i)))
                .collect()
        }
        Some(obj) => {
            let keys: Vec<Handle<V12Str>> = collect_enumerable_string_keys(ctx, obj);
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                let value = crate::internal_methods::dispatch_get(
                    &mut *ctx.heap,
                    obj,
                    PropKey::from_string(k),
                    JsValue::object(obj),
                )
                .unwrap_or(JsValue::undefined());
                out.push(value);
            }
            out
        }
    };
    Ok(array_value(ctx, vals))
}

pub fn object_entries(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let arg = args.first().copied().unwrap_or(JsValue::undefined());
    let arg = to_object_arg(ctx, arg, "entries")?;
    let pairs: Vec<(JsValue, JsValue)> = match arg.as_object() {
        None => {
            let Some(h) = arg.as_string() else {
                return Ok(array_value(ctx, Vec::new()));
            };
            let len = ctx.heap.get(h).len();
            (0..len)
                .map(|i| {
                    (
                        JsValue::string(ctx.heap.intern_text(&i.to_string())),
                        JsValue::string(helpers::string_index_unit(&mut *ctx.heap, h, i)),
                    )
                })
                .collect()
        }
        Some(obj) => {
            let keys: Vec<Handle<V12Str>> = collect_enumerable_string_keys(ctx, obj);
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                let value = crate::internal_methods::dispatch_get(
                    &mut *ctx.heap,
                    obj,
                    PropKey::from_string(k),
                    JsValue::object(obj),
                )
                .unwrap_or(JsValue::undefined());
                out.push((JsValue::string(k), value));
            }
            out
        }
    };
    let items: Vec<JsValue> = pairs
        .into_iter()
        .map(|(k, v)| {
            let pair = ctx.heap.alloc(v12_heap::JsObject::array(vec![k, v]));
            ctx.add_root(JsValue::object(pair));
            JsValue::object(pair)
        })
        .collect();
    Ok(array_value(ctx, items))
}

/// Internal `for-in` helper: own *enumerable* string keys in spec order
/// (array indices ascending, then named keys in insertion order).
/// `null`/`undefined` yield an empty array (the loop body never runs).
/// Dispatch-only (never installed on a JS object); see the bare entries in
/// `define_builtins!`.
pub fn object_enumerable_own_keys(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(array_value(ctx, Vec::new()));
    };
    // for-in enumerates the whole prototype chain (ES `EnumerateObjectProperties`
    // is specified recursively over [[GetPrototypeOf]]): own enumerable keys in
    // `OrdinaryOwnPropertyKeys` order, then the prototype's, skipping a key
    // already seen (a shadowing own key suppresses the inherited one).
    let mut items: Vec<JsValue> = Vec::new();
    let mut seen: Vec<PropKey> = Vec::new();
    let mut cur = Some(obj);
    while let Some(o) = cur {
        for h in collect_enumerable_string_keys(ctx, o) {
            let key = PropKey::from_string(h);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            items.push(JsValue::string(h));
        }
        cur = ctx.heap.get(o).prototype;
    }
    Ok(array_value(ctx, items))
}

/// Whether `desc` is a live own property of `obj`. `delete` stores `hole` in a
/// data property's slot while leaving the shared shape descriptor in place, so
/// a holed data descriptor is not observable. Accessors have no slot and are
/// always live.
fn descriptor_is_live(
    heap: &v12_heap::Heap,
    obj: Handle<v12_heap::JsObject>,
    desc: &v12_heap::Descriptor,
) -> bool {
    match desc {
        v12_heap::Descriptor::Data { slot, .. } => heap
            .get(obj)
            .properties
            .get(*slot as usize)
            .is_some_and(|v| !v.is_hole()),
        v12_heap::Descriptor::Accessor { .. } => true,
    }
}

/// Dictionary-rung analog: data entries read non-hole storage,
/// accessors are always live.
fn dict_entry_is_live(
    heap: &v12_heap::Heap,
    obj: Handle<v12_heap::JsObject>,
    entry: &v12_heap::DictEntry,
) -> bool {
    if entry.is_accessor {
        return true;
    }
    heap.get(obj)
        .properties
        .get(entry.slot as usize)
        .is_some_and(|v| !v.is_hole())
}

pub fn object_has_own_property(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let this_obj = this.as_object().ok_or_else(|| {
        ctx.type_error("TypeError: Object.prototype.hasOwnProperty called on non-object")
    })?;
    let key = args.first().copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key).map_err(Throw::Value)?;
    if has_element_index(ctx, this_obj, pk) {
        return Ok(JsValue::from_bool(true));
    }
    // Dictionary rung first (overflow keys live only here).
    if let Some((entry, _)) = crate::internal_methods::dict_lookup(ctx.heap, this_obj, pk) {
        return Ok(JsValue::from_bool(dict_entry_is_live(
            ctx.heap, this_obj, &entry,
        )));
    }
    let shape = ctx.heap.shape_of(this_obj);
    let found = ctx
        .heap
        .lookup_property(shape, pk)
        .is_some_and(|d| descriptor_is_live(ctx.heap, this_obj, d));
    Ok(JsValue::from_bool(found))
}

/// `Object.prototype.propertyIsEnumerable(key)` – whether `key` is an own
/// enumerable data property of `this`.
pub fn object_proto_property_is_enumerable(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = this.as_object().ok_or_else(|| {
        ctx.type_error("TypeError: Object.prototype.propertyIsEnumerable called on non-object")
    })?;
    let key_v = args.first().copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v).map_err(Throw::Value)?;
    // Element-store index keys are enumerable own data properties.
    if has_element_index(ctx, obj, pk) {
        return Ok(JsValue::from_bool(true));
    }
    // Dictionary rung first (overflow keys live only here).
    if let Some((entry, _)) = crate::internal_methods::dict_lookup(ctx.heap, obj, pk) {
        if entry.is_accessor {
            return Ok(JsValue::from_bool(entry.attrs.enumerable()));
        }
        let populated = ctx
            .heap
            .get(obj)
            .properties
            .get(entry.slot as usize)
            .is_some_and(|v| !v.is_hole());
        return Ok(JsValue::from_bool(populated && entry.attrs.enumerable()));
    }
    let shape = ctx.heap.shape_of(obj);
    let enumerable = match ctx.heap.lookup_property(shape, pk) {
        Some(desc) if desc.is_data() => {
            let populated = desc
                .slot()
                .and_then(|slot| ctx.heap.get(obj).properties.get(slot as usize))
                .is_some_and(|v| !v.is_hole());
            populated && desc.attrs().enumerable()
        }
        Some(desc) => desc.attrs().enumerable(),
        None => false,
    };
    Ok(JsValue::from_bool(enumerable))
}

pub fn object_proto_to_string(
    ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let text = if this.is_object()
        && ctx.heap.get(this.as_object().unwrap()).kind == v12_heap::Kind::Array
    {
        "[object Array]"
    } else {
        "[object Object]"
    };
    Ok(JsValue::string(ctx.heap.intern_text(text)))
}

pub fn object_proto_value_of(
    _ctx: &mut Ctx,
    this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(this)
}

pub fn function_proto_to_string(
    ctx: &mut Ctx,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    Ok(JsValue::string(ctx.heap.intern_text("function() {}")))
}

/// `[[GetOwnProperty]]` on a primitive receiver, per ES `ToObject` wrapper
/// semantics: a string exposes its code-unit indices (writable, enumerable)
/// and `"length"` (non-writable, non-enumerable); every other primitive has
/// no own properties.
fn primitive_descriptor(ctx: &mut Ctx, v: JsValue, pk: PropKey) -> JsValue {
    let Some(h) = v.as_string() else {
        return JsValue::undefined();
    };
    let key_text = pk.string().map(|k| ctx.string_text(k));
    let len = ctx.heap.get(h).len();
    if key_text.as_deref() == Some("length") {
        let d = ctx.alloc_obj(JsObject::default());
        let len_v = if let Some(smi) = JsValue::from_i32_smi(len as i32) {
            smi
        } else {
            JsValue::from_f64(len as f64)
        };
        define_plain_prop(ctx, d, "value", len_v);
        define_plain_prop(ctx, d, "writable", JsValue::from_bool(false));
        define_plain_prop(ctx, d, "enumerable", JsValue::from_bool(false));
        define_plain_prop(ctx, d, "configurable", JsValue::from_bool(false));
        return JsValue::object(d);
    }
    let Some(index) = crate::internal_methods::prop_key_to_index(ctx.heap, pk) else {
        return JsValue::undefined();
    };
    if index as usize >= len {
        return JsValue::undefined();
    }
    let unit = helpers::string_index_unit(&mut *ctx.heap, h, index as usize);
    let d = ctx.alloc_obj(JsObject::default());
    define_plain_prop(ctx, d, "value", JsValue::string(unit));
    define_plain_prop(ctx, d, "writable", JsValue::from_bool(false));
    define_plain_prop(ctx, d, "enumerable", JsValue::from_bool(true));
    define_plain_prop(ctx, d, "configurable", JsValue::from_bool(false));
    JsValue::object(d)
}

/// Index-keyed element-store entry for an Array/Arguments receiver: ES
/// element own property with `{ writable: true, enumerable: true,
/// configurable: true }`. Returns the element value (`None` for non-index
/// keys, non-element-store kinds, and holes).
fn element_index_value(
    ctx: &mut Ctx,
    obj: Handle<v12_heap::JsObject>,
    pk: PropKey,
) -> Option<JsValue> {
    let kind = ctx.heap.get(obj).kind;
    if kind != v12_heap::Kind::Array && kind != v12_heap::Kind::Arguments {
        return None;
    }
    let idx = crate::internal_methods::prop_key_to_index(ctx.heap, pk)?;
    let v = ctx.heap.get(obj).get_element(idx)?;
    if v.is_hole() { None } else { Some(v) }
}

fn has_element_index(ctx: &mut Ctx, obj: Handle<v12_heap::JsObject>, pk: PropKey) -> bool {
    element_index_value(ctx, obj, pk).is_some()
}

/// ES `ArrayIndex` test for an already-interned string key: a canonical
/// decimal spelling in `0..=2**32 - 2` with no leading zeros. `None` for
/// anything else (including `"-0"`, `"01"`, and out-of-range lengths).
fn array_index_key(heap: &mut v12_heap::Heap, h: Handle<V12Str>) -> Option<u32> {
    heap.flatten(h);
    let len = heap.get(h).len();
    if len == 0 || len > 10 {
        return None;
    }
    let mut acc: u32 = 0;
    for i in 0..len {
        let unit = match &heap.get(h).storage {
            v12_heap::StrStorage::Latin1(bytes) => u16::from(bytes[i]),
            v12_heap::StrStorage::Utf16(units) => units[i],
            v12_heap::StrStorage::Cons { .. } | v12_heap::StrStorage::Sliced { .. } => return None,
        };
        if !(u16::from(b'0')..=u16::from(b'9')).contains(&unit) {
            return None;
        }
        if i == 0 && unit == u16::from(b'0') && len > 1 {
            return None;
        }
        acc = acc
            .checked_mul(10)?
            .checked_add(u32::from(unit - u16::from(b'0')))?;
    }
    if acc == u32::MAX {
        return None;
    }
    Some(acc)
}

/// Reorders `keys` per ES `OrdinaryOwnPropertyKeys`: array-index keys
/// ascending (numeric), then the remaining string keys in their existing
/// (insertion) order.
fn sort_own_keys(heap: &mut v12_heap::Heap, keys: &mut Vec<Handle<V12Str>>) {
    let indexed: Vec<(u32, Handle<V12Str>)> = keys
        .iter()
        .filter_map(|&h| array_index_key(heap, h).map(|i| (i, h)))
        .collect();
    if indexed.is_empty() {
        return;
    }
    let mut index_keys: Vec<(u32, Handle<V12Str>)> = indexed;
    index_keys.sort_by_key(|&(i, _)| i);
    let mut rest: Vec<Handle<V12Str>> = Vec::with_capacity(keys.len());
    for &h in keys.iter() {
        if array_index_key(heap, h).is_none() {
            rest.push(h);
        }
    }
    *keys = index_keys.into_iter().map(|(_, h)| h).collect();
    keys.extend(rest);
}

/// Enumerable own string keys in `OrdinaryOwnPropertyKeys` order:
/// array-element indices ascending (Array/Arguments element stores), then the
/// remaining enumerable shape/dictionary string keys in insertion order with
/// integer-like spellings sorted ahead. Backs
/// `Object.keys`/`values`/`entries` and for-in.
fn collect_enumerable_string_keys(
    ctx: &mut Ctx,
    obj: Handle<v12_heap::JsObject>,
) -> Vec<Handle<V12Str>> {
    let kind = ctx.heap.get(obj).kind;
    let indexed_store = kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments;
    let mut keys: Vec<Handle<V12Str>> = Vec::new();
    if indexed_store {
        let bound = ctx.heap.get(obj).element_len() as u32;
        for i in 0..bound {
            if ctx
                .heap
                .get(obj)
                .get_element(i)
                .is_some_and(|v| !v.is_hole())
            {
                keys.push(ctx.heap.intern_text(&i.to_string()));
            }
        }
    }
    for h in collect_own_string_keys(ctx.heap, obj) {
        // Element-store indices were emitted above; skip numeric keys that
        // also reached the shape or dictionary rung to avoid duplicates.
        if indexed_store && array_index_key(ctx.heap, h).is_some() {
            continue;
        }
        let enumerable =
            if let Some(entry) = crate::internal_methods::dict_entry_by_key(ctx.heap, obj, h) {
                entry.attrs.enumerable()
            } else {
                ctx.heap
                    .lookup_property(ctx.heap.shape_of(obj), PropKey::from_string(h))
                    .is_some_and(|d| d.attrs().enumerable())
            };
        if enumerable {
            keys.push(h);
        }
    }
    keys
}

/// Own string keys from the shape descriptors (the shape is authoritative:
/// properties defined via [[DefineOwnProperty]] never touch the parallel
/// `property_keys` vec), plus dictionary-rung overflow keys in insertion
/// order (the two stores never overlap).
fn collect_own_string_keys(
    heap: &mut v12_heap::Heap,
    obj: Handle<v12_heap::JsObject>,
) -> Vec<Handle<V12Str>> {
    let shape = heap.shape_of(obj);
    let mut keys: Vec<Handle<V12Str>> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .filter_map(|d| d.key().string())
        .collect();
    if let Some(map) = heap.get(obj).dictionary.as_ref() {
        let mut overflow: Vec<(u32, Handle<V12Str>)> = map
            .iter()
            .filter_map(|(k, e)| k.string().map(|h| (e.seq, h)))
            .collect();
        overflow.sort_by_key(|&(seq, _)| seq);
        keys.extend(overflow.into_iter().map(|(_, h)| h));
    }
    sort_own_keys(heap, &mut keys);
    keys
}

fn property_key(ctx: &mut Ctx, v: JsValue) -> Result<PropKey, JsValue> {
    if let Some(h) = v.as_string() {
        // Intern: `PropKey` identity is handle identity, so a non-canonical
        // handle (concat, slice, computed key) must alias the canonical
        // instance or shape lookup misses. Flatten-once first, then a
        // single `intern_string` over the flat storage.
        ctx.heap.flatten(h);
        let owned = match &ctx.heap.get(h).storage {
            v12_heap::StrStorage::Latin1(bytes) => v12_heap::V12Str::latin1(bytes.clone()),
            v12_heap::StrStorage::Utf16(units) => v12_heap::V12Str::utf16(units.clone()),
            // Unreachable post-flatten (flatten materializes in place), but
            // degrade gracefully instead of assuming it: re-read lossy text.
            _ => v12_heap::V12Str::utf16(
                helpers::string_text(&mut *ctx.heap, h)
                    .encode_utf16()
                    .collect(),
            ),
        };
        return Ok(PropKey::from_string(ctx.heap.intern_string(owned)));
    }
    if let Some(sym) = v.as_symbol() {
        return Ok(PropKey::from_symbol(sym));
    }
    // Coerce via ToString.
    let h = to_string_handle(ctx, v)?;
    Ok(PropKey::from_string(h))
}

fn to_string_handle(ctx: &mut Ctx, v: JsValue) -> Result<Handle<V12Str>, JsValue> {
    let text = ctx.to_string(v);
    Ok(ctx.heap.intern_text(&text))
}

// ---------------------------------------------------------------------------
// Object statics
// ---------------------------------------------------------------------------

/// Allocates an array value from `items` (rooted, like every native-created
/// object that outlives the call).
fn array_value(ctx: &mut Ctx, items: Vec<JsValue>) -> JsValue {
    let arr = ctx.heap.alloc(v12_heap::JsObject::array(items));
    // Link [[Prototype]] to %Array.prototype% (via the Array constructor's
    // linked field) so `result.constructor === Array`; mirrors the
    // error-instance linking in `builtins/registry.rs`.
    if let Some(ctor) = ctx.intrinsic("Array").and_then(|v| v.as_object())
        && let Some(proto) = ctx.heap.get(ctor).prototype
    {
        ctx.heap.get_mut(arr).prototype = Some(proto);
    }
    ctx.add_root(JsValue::object(arr));
    JsValue::object(arr)
}

/// `Object.is(a, b)` – ES `SameValue`: NaN is NaN, and ±0 differ.
pub fn object_is(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let a = args.first().copied().unwrap_or(JsValue::undefined());
    let b = args.get(1).copied().unwrap_or(JsValue::undefined());
    let same = match (
        a.as_smi().map(f64::from).or(a.as_f64()),
        b.as_smi().map(f64::from).or(b.as_f64()),
    ) {
        (Some(x), Some(y)) => {
            (x.is_nan() && y.is_nan()) || (x == y && (x != 0.0 || x.to_bits() == y.to_bits()))
        }
        _ => helpers::strict_equals(ctx.heap, a, b),
    };
    Ok(JsValue::from_bool(same))
}

/// `Object.hasOwn(obj, key)` – `hasOwnProperty` as a static.
pub fn object_has_own(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.hasOwn called on non-object"))?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v).map_err(Throw::Value)?;
    if has_element_index(ctx, obj, pk) {
        return Ok(JsValue::from_bool(true));
    }
    let shape = ctx.heap.shape_of(obj);
    Ok(JsValue::from_bool(
        ctx.heap
            .lookup_property(shape, pk)
            .is_some_and(|d| descriptor_is_live(ctx.heap, obj, d)),
    ))
}

/// `Object.assign(target, ...sources)` – copies own *enumerable* properties
/// from each source onto the target via [[DefineOwnProperty]].
pub fn object_assign(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let target = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.assign called on non-object"))?;
    for &source in args.iter().skip(1) {
        let Some(src) = source.as_object() else {
            continue;
        };
        // Integer-indexed elements: aligned element-store copy.
        if ctx.heap.get(src).kind == v12_heap::Kind::Array {
            let elems = ctx.heap.get(src).elements_snapshot();
            for (i, v) in elems.iter().enumerate() {
                if !v.is_hole() {
                    ctx.heap.get_mut(target).set_element(i as u32, *v);
                }
            }
        } else {
            let elems = ctx.heap.get(src).elements.clone();
            for (i, v) in elems.iter().enumerate() {
                if !v.is_hole() {
                    ctx.heap.get_mut(target).set_element(i as u32, *v);
                }
            }
        }
        // Named properties: snapshot from the shape descriptors (the shape
        // is authoritative — values defined via [[DefineOwnProperty]] never
        // touch the parallel `property_keys` vec), copying only the
        // enumerable ones. Snapshot first: defines may allocate.
        let shape = ctx.heap.shape_of(src);
        let slots: Vec<(PropKey, u32)> = ctx
            .heap
            .get(shape)
            .descriptors
            .as_slice()
            .iter()
            .filter(|d| d.attrs().enumerable())
            .filter_map(|d| d.slot().map(|slot| (d.key(), slot)))
            .collect();
        let mut pairs: Vec<(PropKey, JsValue)> = Vec::with_capacity(slots.len());
        for (pk, slot) in slots {
            if let Some(&v) = ctx.heap.get(src).properties.get(slot as usize) {
                pairs.push((pk, v));
            }
        }
        for (pk, value) in pairs {
            crate::internal_methods::ordinary_define_own_property(
                &mut *ctx.heap,
                target,
                pk,
                crate::internal_methods::PropertyDescriptor {
                    value: Some(value),
                    ..Default::default()
                },
            )
            .map_err(Throw::Value)?;
        }
    }
    Ok(JsValue::object(target))
}

/// `Object.freeze(obj)` / `Object.seal(obj)`. ES `SetIntegrityLevel` sets the
/// non-extensible flag and then redefines *every* own property: `seal` clears
/// `[[Configurable]]`, `freeze` also clears `[[Writable]]`. The per-object
/// integrity flag alone would report correct `Object.isFrozen`, but each
/// property's own descriptor (`Object.getOwnPropertyDescriptor`) must change
/// too.
fn set_integrity(
    ctx: &mut Ctx,
    args: &[JsValue],
    level: v12_heap::IntegrityLevel,
) -> Result<JsValue, Throw> {
    match args.first().and_then(|v| v.as_object()) {
        Some(obj) => {
            // Read every own key before mutating: the redefines below can
            // spill the object to the dictionary rung, which is disjoint from
            // the frozen shape and would otherwise hide the remaining keys.
            let keys: Vec<PropKey> = own_key_list(ctx, obj);
            ctx.heap.set_integrity_level(obj, level);
            let frozen = matches!(level, v12_heap::IntegrityLevel::Frozen);
            for key in keys {
                let desc = if frozen {
                    crate::internal_methods::FullDescriptor {
                        writable: Some(false),
                        configurable: Some(false),
                        ..Default::default()
                    }
                } else {
                    crate::internal_methods::FullDescriptor {
                        configurable: Some(false),
                        ..Default::default()
                    }
                };
                crate::internal_methods::apply_property_descriptor(&mut *ctx.heap, obj, key, desc)
                    .map_err(Throw::Value)?;
            }
            Ok(JsValue::object(obj))
        }
        None => Ok(args.first().copied().unwrap_or(JsValue::undefined())),
    }
}

/// Every own property key (string and symbol) in `OrdinaryOwnPropertyKeys`
/// order, for `SetIntegrityLevel`'s redefine sweep.
fn own_key_list(ctx: &mut Ctx, obj: Handle<v12_heap::JsObject>) -> Vec<PropKey> {
    let mut keys: Vec<PropKey> = Vec::new();
    let kind = ctx.heap.get(obj).kind;
    if kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments {
        let bound = ctx.heap.get(obj).element_len() as u32;
        for i in 0..bound {
            if ctx
                .heap
                .get(obj)
                .get_element(i)
                .is_some_and(|v| !v.is_hole())
            {
                keys.push(PropKey::from_string(ctx.heap.intern_text(&i.to_string())));
            }
        }
    }
    let named: Vec<Handle<V12Str>> = collect_own_string_keys(ctx.heap, obj);
    for h in named {
        if (kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments)
            && array_index_key(ctx.heap, h).is_some()
        {
            continue;
        }
        keys.push(PropKey::from_string(h));
    }
    // Symbol keys last (OrdinaryOwnPropertyKeys order).
    let shape = ctx.heap.shape_of(obj);
    let symbols: Vec<PropKey> = ctx
        .heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|d| d.key())
        .filter(|k| k.is_symbol())
        .collect();
    keys.extend(symbols);
    keys
}

pub fn object_freeze(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_integrity(ctx, args, v12_heap::IntegrityLevel::Frozen)
}

pub fn object_seal(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_integrity(ctx, args, v12_heap::IntegrityLevel::Sealed)
}

/// `Object.preventExtensions(obj)` – non-extensible flag only.
pub fn object_prevent_extensions(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    if let Some(obj) = args.first().and_then(|v| v.as_object()) {
        ctx.heap.get_mut(obj).flags |= v12_heap::JsObject::FLAG_NOT_EXTENSIBLE;
        let cell = ctx.heap.validity_cell_of(obj);
        ctx.heap.bump_validity(cell);
    }
    Ok(args.first().copied().unwrap_or(JsValue::undefined()))
}

/// `Object.isSealed(obj)` / `Object.isFrozen(obj)` share this shape;
/// non-objects are always sealed/frozen.
fn check_integrity(
    ctx: &mut Ctx,
    args: &[JsValue],
    check: fn(&v12_heap::JsObject) -> bool,
) -> Result<JsValue, Throw> {
    let result = match args.first().and_then(|v| v.as_object()) {
        Some(obj) => check(ctx.heap.get(obj)),
        None => true,
    };
    Ok(JsValue::from_bool(result))
}

pub fn object_is_sealed(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    check_integrity(ctx, args, v12_heap::JsObject::is_sealed)
}

pub fn object_is_frozen(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    check_integrity(ctx, args, v12_heap::JsObject::is_frozen)
}

/// `Object.isExtensible(obj)` – false for primitives, flag test for objects.
pub fn object_is_extensible(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let extensible = match args.first().and_then(|v| v.as_object()) {
        Some(obj) => ctx.heap.get(obj).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0,
        None => false,
    };
    Ok(JsValue::from_bool(extensible))
}

/// `Object.fromEntries(entries)` – array of `[key, value]` pairs → object.
pub fn object_from_entries(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = ctx.alloc_obj(JsObject::default());
    let Some(entries) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::object(obj));
    };
    let len = ctx.heap.get(entries).element_len();
    for i in 0..len as u32 {
        let Some(pair) = ctx
            .heap
            .get(entries)
            .get_element(i)
            .and_then(|v| v.as_object())
        else {
            continue;
        };
        let k = ctx
            .heap
            .get(pair)
            .get_element(0)
            .unwrap_or(JsValue::undefined());
        let v = ctx
            .heap
            .get(pair)
            .get_element(1)
            .unwrap_or(JsValue::undefined());
        let pk = property_key(ctx, k).map_err(Throw::Value)?;
        crate::internal_methods::ordinary_define_own_property(
            &mut *ctx.heap,
            obj,
            pk,
            crate::internal_methods::PropertyDescriptor {
                value: Some(v),
                ..Default::default()
            },
        )
        .map_err(Throw::Value)?;
    }
    Ok(JsValue::object(obj))
}

/// Own property names in spec order: array indices ascending, then named
/// string keys in insertion order.
pub fn own_property_names(ctx: &mut Ctx, obj: Handle<v12_heap::JsObject>) -> Vec<String> {
    let kind = ctx.heap.get(obj).kind;
    let mut names: Vec<String> = Vec::new();
    if kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments {
        // Element stores are index-keyed, not shape descriptors; enumerate
        // the live slots (holes are absent) before the named keys.
        let bound = ctx.heap.get(obj).element_len() as u32;
        for i in 0..bound {
            if ctx
                .heap
                .get(obj)
                .get_element(i)
                .is_some_and(|v| !v.is_hole())
            {
                names.push(i.to_string());
            }
        }
    }
    // Named keys through the shared ordered collector: it applies
    // `OrdinaryOwnPropertyKeys` ordering (integer-like spellings that live in
    // the shape — e.g. `"10"` past the array-index fast path — sort before
    // the insertion-ordered names) and reads both the shape and the
    // dictionary rung.
    for h in collect_own_string_keys(ctx.heap, obj) {
        names.push(ctx.string_text(h));
    }
    names
}

/// `Object.getOwnPropertyNames(obj)` – own string-keyed properties.
///
/// ES `ToObject` coercion: a string primitive contributes its indices plus
/// `"length"`; other primitives contribute nothing; `null`/`undefined`
/// throw `TypeError`.
pub fn object_get_own_property_names(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let arg = args.first().copied().unwrap_or(JsValue::undefined());
    if arg.is_null() || arg.is_undefined() {
        return Err(ctx.type_error("TypeError: Object.getOwnPropertyNames called on non-object"));
    }
    if let Some(h) = arg.as_string() {
        let len = ctx.heap.get(h).len();
        let mut items: Vec<JsValue> = (0..len)
            .map(|i| JsValue::string(ctx.heap.intern_text(&i.to_string())))
            .collect();
        items.push(JsValue::string(ctx.heap.intern_text("length")));
        return Ok(array_value(ctx, items));
    }
    if arg.as_object().is_none() {
        // Number/boolean/symbol/bigint wrappers carry no own properties.
        return Ok(array_value(ctx, Vec::new()));
    }
    let obj = arg.as_object().unwrap();
    let names = own_property_names(ctx, obj);
    let items: Vec<JsValue> = names
        .iter()
        .map(|n| JsValue::string(ctx.heap.intern_text(n)))
        .collect();
    Ok(array_value(ctx, items))
}

/// `Object.getOwnPropertySymbols(obj)` – own symbol-keyed properties.
pub fn object_get_own_property_symbols(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| {
        ctx.type_error("TypeError: Object.getOwnPropertySymbols called on non-object")
    })?;
    let shape = ctx.heap.shape_of(obj);
    let items: Vec<JsValue> = ctx
        .heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .filter_map(|d| d.key().symbol().map(|s| JsValue::symbol(s)))
        .collect();
    Ok(array_value(ctx, items))
}

/// Slots a resolved own property can occupy: an element-store index, a
/// shape/dictionary data slot, or an accessor pair.
enum SlotKind {
    Data {
        slot: u32,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    },
    Accessor {
        get: Option<v12_heap::Handle<v12_heap::JsObject>>,
        set: Option<v12_heap::Handle<v12_heap::JsObject>>,
        enumerable: bool,
        configurable: bool,
    },
}

/// `[[GetOwnProperty]]` for an ordinary object, materialized as a plain
/// descriptor object (ES `FromPropertyDescriptor`). Returns `None` when the
/// property is absent. Shared by `Object.getOwnPropertyDescriptor` and
/// `Object.getOwnPropertyDescriptors`.
///
/// The reader resolves, in order: the Array/Arguments element store (index
/// keys never reach the shape), the dictionary rung (overflow keys live only
/// there), then the shape descriptors. Element-store hits report the spec's
/// element attrs `{ writable: true, enumerable: true, configurable: true }`.
fn build_descriptor_object(
    ctx: &mut Ctx,
    obj: Handle<v12_heap::JsObject>,
    pk: PropKey,
) -> Option<JsValue> {
    if let Some(value) = element_index_value(ctx, obj, pk) {
        let d = ctx.alloc_obj(JsObject::default());
        define_plain_prop(ctx, d, "value", value);
        define_plain_prop(ctx, d, "writable", JsValue::from_bool(true));
        define_plain_prop(ctx, d, "enumerable", JsValue::from_bool(true));
        define_plain_prop(ctx, d, "configurable", JsValue::from_bool(true));
        return Some(JsValue::object(d));
    }
    let kind: SlotKind =
        if let Some((entry, _)) = crate::internal_methods::dict_lookup(ctx.heap, obj, pk) {
            if !dict_entry_is_live(ctx.heap, obj, &entry) {
                return None;
            }
            if entry.is_accessor {
                SlotKind::Accessor {
                    get: entry.getter,
                    set: entry.setter,
                    enumerable: entry.attrs.enumerable(),
                    configurable: entry.attrs.configurable(),
                }
            } else {
                SlotKind::Data {
                    slot: entry.slot,
                    writable: entry.attrs.writable(),
                    enumerable: entry.attrs.enumerable(),
                    configurable: entry.attrs.configurable(),
                }
            }
        } else {
            let shape = ctx.heap.shape_of(obj);
            let Some(desc) = ctx
                .heap
                .lookup_property(shape, pk)
                .filter(|d| descriptor_is_live(ctx.heap, obj, d))
            else {
                return None;
            };
            // Copy the descriptor out before any allocation (heap borrows nest).
            if let Some(slot) = desc.slot() {
                SlotKind::Data {
                    slot,
                    writable: desc.attrs().writable(),
                    enumerable: desc.attrs().enumerable(),
                    configurable: desc.attrs().configurable(),
                }
            } else {
                SlotKind::Accessor {
                    get: desc.getter(),
                    set: desc.setter(),
                    enumerable: desc.attrs().enumerable(),
                    configurable: desc.attrs().configurable(),
                }
            }
        };
    let d = ctx.alloc_obj(JsObject::default());
    match kind {
        SlotKind::Data {
            slot,
            writable,
            enumerable,
            configurable,
        } => {
            let value = ctx
                .heap
                .get(obj)
                .properties
                .get(slot as usize)
                .copied()
                .unwrap_or(JsValue::undefined());
            define_plain_prop(ctx, d, "value", value);
            define_plain_prop(ctx, d, "writable", JsValue::from_bool(writable));
            define_plain_prop(ctx, d, "enumerable", JsValue::from_bool(enumerable));
            define_plain_prop(ctx, d, "configurable", JsValue::from_bool(configurable));
        }
        SlotKind::Accessor {
            get,
            set,
            enumerable,
            configurable,
        } => {
            define_plain_prop(
                ctx,
                d,
                "get",
                get.map(JsValue::object).unwrap_or(JsValue::undefined()),
            );
            define_plain_prop(
                ctx,
                d,
                "set",
                set.map(JsValue::object).unwrap_or(JsValue::undefined()),
            );
            define_plain_prop(ctx, d, "enumerable", JsValue::from_bool(enumerable));
            define_plain_prop(ctx, d, "configurable", JsValue::from_bool(configurable));
        }
    }
    Some(JsValue::object(d))
}

/// Annex B.2.2.2 `Object.prototype.__defineGetter__(P, getter)`: installs an
/// accessor with `{ [[Enumerable]]: true, [[Configurable]]: true }`.
pub fn object_proto_define_getter(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    object_proto_define_accessor(ctx, this, args, "get")
}

/// Annex B.2.2.3 `Object.prototype.__defineSetter__(P, setter)`.
pub fn object_proto_define_setter(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    object_proto_define_accessor(ctx, this, args, "set")
}

/// Shared body of the Annex B accessor installers: `O` is coerced via
/// `ToObject`, `P` via `ToPropertyKey`, the accessor function is required to
/// be callable, and the property is defined with `{ enumerable: true,
/// configurable: true }` (the other half absent).
fn object_proto_define_accessor(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
    half: &str,
) -> Result<JsValue, Throw> {
    let obj = this
        .as_object()
        .ok_or_else(|| ctx.type_error("TypeError: called on non-object"))?;
    let key_v = args.first().copied().unwrap_or(JsValue::undefined());
    let key = property_key(ctx, key_v).map_err(Throw::Value)?;
    let func = args.get(1).copied().unwrap_or(JsValue::undefined());
    if func.as_object().is_none() {
        return Err(ctx.type_error(format!(
            "TypeError: Object.prototype.__define{half_title}__ called with a non-callable",
            half_title = if half == "get" { "Getter" } else { "Setter" },
        )));
    }
    let mut desc = crate::internal_methods::FullDescriptor {
        enumerable: Some(true),
        configurable: Some(true),
        ..Default::default()
    };
    if half == "get" {
        desc.get = Some(func);
    } else {
        desc.set = Some(func);
    }
    let defined = match apply_array_length(ctx, obj, key, &desc)? {
        Some(outcome) => outcome,
        None => crate::internal_methods::apply_property_descriptor(&mut *ctx.heap, obj, key, desc)
            .map_err(Throw::Value)?,
    };
    if !defined {
        return Err(
            ctx.type_error("TypeError: Cannot redefine property: Invalid property definition")
        );
    }
    Ok(JsValue::undefined())
}

/// `Object.getOwnPropertyDescriptor(obj, key)` – a plain descriptor object,
/// or `undefined` when the property is absent.
pub fn object_get_own_property_descriptor(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let arg = args.first().copied().unwrap_or(JsValue::undefined());
    // ES `ToObject`: `null`/`undefined` throw; other primitives wrap into
    // their kind object. A string wrapper's only own properties are its
    // code-unit indices (all enumerable) and `"length"` (non-writable,
    // non-enumerable).
    let arg = to_object_arg(ctx, arg, "getOwnPropertyDescriptor")?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v).map_err(Throw::Value)?;
    if arg.as_object().is_none() {
        return Ok(primitive_descriptor(ctx, arg, pk));
    }
    let obj = arg.as_object().expect("object arm");
    Ok(build_descriptor_object(ctx, obj, pk).unwrap_or(JsValue::undefined()))
}

/// ES `OwnPropertyKeys` as `PropKey`s in spec order (strings then symbols),
/// including element-store indices. Backs the descriptor sweeps.
fn own_keys_in_order(ctx: &mut Ctx, obj: Handle<v12_heap::JsObject>) -> Vec<PropKey> {
    let mut keys: Vec<PropKey> = Vec::new();
    let kind = ctx.heap.get(obj).kind;
    let element_kind = kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments;
    if element_kind {
        let bound = ctx.heap.get(obj).element_len() as u32;
        for i in 0..bound {
            if ctx
                .heap
                .get(obj)
                .get_element(i)
                .is_some_and(|v| !v.is_hole())
            {
                keys.push(PropKey::from_string(ctx.heap.intern_text(&i.to_string())));
            }
        }
    }
    for h in collect_own_string_keys(ctx.heap, obj) {
        if element_kind && array_index_key(ctx.heap, h).is_some() {
            continue;
        }
        keys.push(PropKey::from_string(h));
    }
    let shape = ctx.heap.shape_of(obj);
    keys.extend(
        ctx.heap
            .get(shape)
            .descriptors
            .as_slice()
            .iter()
            .map(|d| d.key())
            .filter(|k| k.is_symbol()),
    );
    keys
}

/// `Object.getOwnPropertyDescriptors(obj)` – an object whose own enumerable
/// data properties are `key → descriptor` for every own key of `obj` (ES
/// 20.1.2.9; `CreateDataProperty` attrs, so all are writable/enumerable/
/// configurable).
pub fn object_get_own_property_descriptors(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let arg = args.first().copied().unwrap_or(JsValue::undefined());
    let arg = to_object_arg(ctx, arg, "getOwnPropertyDescriptors")?;
    let result = ctx.alloc_obj(JsObject::default());
    // ES step 1: `obj = ToObject(parameter)`, step 2: `ownKeys`. The result is
    // an ordinary object linked to `%Object.prototype%` (test262
    // `normal-object.js` checks the linkage).
    if let Some(ctor) = ctx.intrinsic("Object").and_then(|v| v.as_object())
        && let Some(proto) = ctx.heap.get(ctor).prototype
    {
        ctx.heap.get_mut(result).prototype = Some(proto);
    }
    let Some(obj) = arg.as_object() else {
        // A string primitive wraps into a `String` exotic object whose own
        // keys are its code-unit indices and `"length"`; the other primitives
        // have no own properties.
        if let Some(h) = arg.as_string() {
            let len = ctx.heap.get(h).len();
            for i in 0..len {
                let key = ctx.heap.intern_text(&i.to_string());
                let desc = primitive_descriptor(ctx, arg, PropKey::from_string(key));
                if !desc.is_undefined() {
                    crate::internal_methods::ordinary_define_own_property(
                        &mut *ctx.heap,
                        result,
                        PropKey::from_string(key),
                        crate::internal_methods::PropertyDescriptor {
                            value: Some(desc),
                            writable: true,
                            enumerable: true,
                            configurable: true,
                        },
                    )
                    .map_err(Throw::Value)?;
                }
            }
            let len_key = ctx.heap.intern_text("length");
            let desc = primitive_descriptor(ctx, arg, PropKey::from_string(len_key));
            crate::internal_methods::ordinary_define_own_property(
                &mut *ctx.heap,
                result,
                PropKey::from_string(len_key),
                crate::internal_methods::PropertyDescriptor {
                    value: Some(desc),
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            )
            .map_err(Throw::Value)?;
        }
        return Ok(JsValue::object(result));
    };
    for pk in own_keys_in_order(ctx, obj) {
        let Some(desc) = build_descriptor_object(ctx, obj, pk) else {
            continue;
        };
        // `CreateDataProperty(result, key, desc)`: DEFAULT attrs.
        crate::internal_methods::ordinary_define_own_property(
            &mut *ctx.heap,
            result,
            pk,
            crate::internal_methods::PropertyDescriptor {
                value: Some(desc),
                writable: true,
                enumerable: true,
                configurable: true,
            },
        )
        .map_err(Throw::Value)?;
    }
    Ok(JsValue::object(result))
}

/// `Object.defineProperties(obj, props)` – ES 20.1.2.3.1
/// `ObjectDefineProperties`. See [`apply_define_properties`] for the shared
/// body (also used by `Object.create`'s second argument).
pub fn object_define_properties(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.defineProperties called on non-object"))?;
    let props_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    apply_define_properties(ctx, obj, props_v)?;
    Ok(JsValue::object(obj))
}

/// ES 20.1.2.3.1 `ObjectDefineProperties(O, Properties)`: rethrow a
/// non-object `Properties` as `TypeError`, then collect `(key, descriptor)`
/// pairs from `Properties`'s *enumerable* own properties, completing
/// `ToPropertyDescriptor` for every one of them, and only then apply each via
/// `[[DefineOwnProperty]]`. The two phases are observable: a later malformed
/// descriptor throws before any property is defined (test262
/// `...-ordered-*`).
fn apply_define_properties(
    ctx: &mut Ctx,
    obj: Handle<v12_heap::JsObject>,
    props_v: JsValue,
) -> Result<(), Throw> {
    if props_v.is_null() || props_v.is_undefined() {
        return Err(
            ctx.type_error("TypeError: Object.defineProperties called with non-object descriptors")
        );
    }
    // Phase 1: collect descriptors (no mutation). `ToObject(Properties)`.
    let mut pairs: Vec<(PropKey, crate::internal_methods::FullDescriptor)> = Vec::new();
    if let Some(props) = props_v.as_object() {
        for pk in own_keys_in_order(ctx, props) {
            // Only *enumerable* own properties of `props` contribute.
            if !descriptor_enumerable(ctx, props, pk) {
                continue;
            }
            let desc_obj = crate::internal_methods::dispatch_get(
                &mut *ctx.heap,
                props,
                pk,
                JsValue::object(props),
            )
            .map_err(Throw::Value)?;
            let desc = to_property_descriptor(ctx, desc_obj)?;
            pairs.push((pk, desc));
        }
    }
    // Phase 2: apply.
    for (pk, desc) in pairs {
        // Array `"length"` uses its dedicated `ArraySetLength` path, exactly
        // as `Object.defineProperty` does.
        let defined = match apply_array_length(ctx, obj, pk, &desc)? {
            Some(outcome) => outcome,
            None => {
                crate::internal_methods::apply_property_descriptor(&mut *ctx.heap, obj, pk, desc)
                    .map_err(Throw::Value)?
            }
        };
        if !defined {
            return Err(
                ctx.type_error("TypeError: Cannot redefine property: Invalid property definition")
            );
        }
    }
    Ok(())
}

/// Whether `obj`'s own property `pk` is enumerable (dictionary rung first,
/// then the shape descriptors).
fn descriptor_enumerable(ctx: &mut Ctx, obj: Handle<v12_heap::JsObject>, pk: PropKey) -> bool {
    if let Some((entry, _)) = crate::internal_methods::dict_lookup(ctx.heap, obj, pk) {
        return entry.attrs.enumerable();
    }
    let shape = ctx.heap.shape_of(obj);
    ctx.heap
        .lookup_property(shape, pk)
        .is_some_and(|d| d.attrs().enumerable())
}

/// Defines a plain data property on a fresh descriptor object.
fn define_plain_prop(
    ctx: &mut Ctx,
    obj: v12_heap::Handle<v12_heap::JsObject>,
    name: &str,
    value: JsValue,
) {
    let h = ctx.heap.intern_text(name);
    let key = PropKey::from_string(h);
    let shape = ctx.heap.shape_of_mut(obj);
    let child = ctx.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    ctx.heap.bind_shape(obj, child);
    ctx.heap.get_mut(obj).properties.push(value);
    ctx.heap.get_mut(obj).property_keys.push(Some(key));
}

/// `Object.setPrototypeOf(obj, proto)` – rewires the [[Prototype]] link.
pub fn object_set_prototype_of(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(args.first().copied().unwrap_or(JsValue::undefined()));
    };
    let proto = args.get(1).copied().unwrap_or(JsValue::undefined());
    let link = if proto.is_null() {
        None
    } else if let Some(h) = proto.as_object() {
        Some(h)
    } else {
        return Err(
            ctx.type_error("TypeError: Object.setPrototypeOf prototype must be object or null")
        );
    };
    ctx.heap.get_mut(obj).prototype = link;
    Ok(JsValue::object(obj))
}
