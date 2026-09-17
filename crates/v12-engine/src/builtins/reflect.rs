//! `Reflect` built-ins.
//!
//! `Reflect` is an ordinary, non-callable object with 13 static methods. This
//! module owns the object's method bodies; the realm installs it as a plain
//! global property (like `Math`/`JSON`, not a `GLOBAL_INTRINSICS` slot).
//!
//! **Known v1 limit:** a `NativeHandler` receives only `&mut Heap` and cannot
//! re-enter the interpreter. Operations whose spec algorithm must call a
//! user-visible getter/setter/`valueOf`/`toString`/`Symbol.toPrimitive`
//! therefore cannot observe those side effects; the ordinary-object paths
//! here are implemented, the interpreter-reentry paths are documented gaps.

use v12_heap::{Attrs, Handle, Heap, JsObject, JsValue, PropKey, V12Str};
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

/// ES `ToPropertyKey` (interpreter-reentry-free subset): symbols pass through,
/// strings are canonicalized, everything else is coerced with `ToString`.
pub fn property_key(ctx: &mut Ctx, v: JsValue) -> Result<PropKey, Throw> {
    if let Some(h) = v.as_string() {
        ctx.heap.flatten(h);
        let owned = match &ctx.heap.get(h).storage {
            v12_heap::StrStorage::Latin1(bytes) => V12Str::latin1(bytes.clone()),
            v12_heap::StrStorage::Utf16(units) => V12Str::utf16(units.clone()),
            _ => V12Str::utf16(
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
    let text = ctx.to_string(v);
    Ok(PropKey::from_string(ctx.heap.intern_text(&text)))
}

/// `CreateDataProperty`-style plain descriptor field on a fresh object
/// (writable/enumerable/configurable all true).
fn plain(ctx: &mut Ctx, obj: Handle<JsObject>, name: &str, value: JsValue) {
    ctx.define_data_prop_with_attrs(obj, name, value, Attrs::DEFAULT);
}

/// Builds the `FromPropertyDescriptor` result object for `obj`'s own property
/// `pk`, or `None` when the property is absent. Mirrors the descriptor reader
/// used by `Object.getOwnPropertyDescriptor` but kept local to `Reflect`
/// because the shared one is private to `object.rs`.
fn build_descriptor(
    ctx: &mut Ctx,
    obj: Handle<JsObject>,
    pk: PropKey,
) -> Option<JsValue> {
    let kind = ctx.heap.get(obj).kind;
    if (kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments)
        && let Some(idx) = crate::internal_methods::prop_key_to_index(ctx.heap, pk)
        && let Some(v) = ctx.heap.get(obj).get_element(idx)
        && !v.is_hole()
    {
        let d = ctx.alloc_obj(JsObject::default());
        plain(ctx, d, "value", v);
        plain(ctx, d, "writable", JsValue::from_bool(true));
        plain(ctx, d, "enumerable", JsValue::from_bool(true));
        plain(ctx, d, "configurable", JsValue::from_bool(true));
        return Some(JsValue::object(d));
    }

    // (is_accessor, value_or_get, set, writable, enumerable, configurable)
    let mut data: Option<(JsValue, bool, bool, bool)> = None;
    let mut accessor: Option<(JsValue, JsValue, bool, bool)> = None;

    if let Some((entry, value)) = crate::internal_methods::dict_lookup(ctx.heap, obj, pk) {
        if entry.is_accessor {
            accessor = Some((
                entry
                    .getter
                    .map(JsValue::object)
                    .unwrap_or(JsValue::undefined()),
                entry
                    .setter
                    .map(JsValue::object)
                    .unwrap_or(JsValue::undefined()),
                entry.attrs.enumerable(),
                entry.attrs.configurable(),
            ));
        } else if !value.is_hole() {
            data = Some((
                value,
                entry.attrs.writable(),
                entry.attrs.enumerable(),
                entry.attrs.configurable(),
            ));
        } else {
            return None;
        }
    } else {
        let shape = ctx.heap.shape_of(obj);
        let desc = ctx.heap.lookup_property(shape, pk).copied()?;
        let live = match desc.slot() {
            Some(slot) => ctx
                .heap
                .get(obj)
                .properties
                .get(slot as usize)
                .is_some_and(|v| !v.is_hole()),
            None => true,
        };
        if !live {
            return None;
        }
        if let Some(slot) = desc.slot() {
            let value = ctx
                .heap
                .get(obj)
                .properties
                .get(slot as usize)
                .copied()
                .unwrap_or(JsValue::undefined());
            data = Some((
                value,
                desc.attrs().writable(),
                desc.attrs().enumerable(),
                desc.attrs().configurable(),
            ));
        } else {
            accessor = Some((
                desc.getter().map(JsValue::object).unwrap_or(JsValue::undefined()),
                desc.setter().map(JsValue::object).unwrap_or(JsValue::undefined()),
                desc.attrs().enumerable(),
                desc.attrs().configurable(),
            ));
        }
    }

    let d = ctx.alloc_obj(JsObject::default());
    if let Some((value, w, e, c)) = data {
        plain(ctx, d, "value", value);
        plain(ctx, d, "writable", JsValue::from_bool(w));
        plain(ctx, d, "enumerable", JsValue::from_bool(e));
        plain(ctx, d, "configurable", JsValue::from_bool(c));
    } else {
        let (get, set, e, c) = accessor.expect("descriptor half resolved");
        plain(ctx, d, "get", get);
        plain(ctx, d, "set", set);
        plain(ctx, d, "enumerable", JsValue::from_bool(e));
        plain(ctx, d, "configurable", JsValue::from_bool(c));
    }
    Some(JsValue::object(d))
}

/// Own property keys in ES `OrdinaryOwnPropertyKeys` order: array indices
/// ascending, then string keys in creation order, then symbols.
fn own_keys_ordered(heap: &mut Heap, obj: Handle<JsObject>) -> Vec<PropKey> {
    let kind = heap.get(obj).kind;
    let element_kind = kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments;
    let mut keys: Vec<PropKey> = Vec::new();
    if element_kind {
        let bound = heap.get(obj).element_len() as u32;
        for i in 0..bound {
            if heap.get(obj).get_element(i).is_some_and(|v| !v.is_hole()) {
                keys.push(PropKey::from_string(heap.intern_text(&i.to_string())));
            }
        }
    }

    let shape = heap.shape_of(obj);
    let mut string_keys: Vec<PropKey> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|d| d.key())
        .filter(|k| !k.is_symbol())
        .collect();
    let mut symbol_keys: Vec<PropKey> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|d| d.key())
        .filter(|k| k.is_symbol())
        .collect();
    if let Some(map) = heap.get(obj).dictionary.as_ref() {
        let mut overflow: Vec<(u32, PropKey)> = map.iter().map(|(k, e)| (e.seq, *k)).collect();
        overflow.sort_by_key(|&(seq, _)| seq);
        for (_, k) in overflow {
            if k.is_symbol() {
                symbol_keys.push(k);
            } else {
                string_keys.push(k);
            }
        }
    }

    let mut indexed: Vec<(u32, PropKey)> = string_keys
        .iter()
        .filter_map(|&k| crate::internal_methods::prop_key_to_index(heap, k).map(|i| (i, k)))
        .collect();
    indexed.sort_by_key(|&(i, _)| i);
    for (_, k) in indexed {
        if !element_kind {
            keys.push(k);
        }
    }
    for &k in &string_keys {
        if crate::internal_methods::prop_key_to_index(heap, k).is_some() {
            continue;
        }
        keys.push(k);
    }
    keys.extend(symbol_keys);
    keys
}

/// Allocates an array value (rooted) linked to `%Array.prototype%`.
fn array_value(ctx: &mut Ctx, items: Vec<JsValue>) -> JsValue {
    let arr = ctx.heap.alloc(JsObject::array(items));
    if let Some(ctor) = ctx.intrinsic("Array").and_then(|v| v.as_object())
        && let Some(proto) = ctx.heap.get(ctor).prototype
    {
        ctx.heap.get_mut(arr).prototype = Some(proto);
    }
    ctx.add_root(JsValue::object(arr));
    JsValue::object(arr)
}

/// Requires an object target per the spec's `Type(target) is not Object`
/// guard; `name` names the method in the thrown `TypeError`.
fn require_object(
    ctx: &mut Ctx,
    args: &[JsValue],
    name: &str,
) -> Result<Handle<JsObject>, Throw> {
    args.first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error(format!("Reflect.{name} target is not an object")))
}

/// ES 6.2.5.5 `ToPropertyDescriptor` (local copy: `object.rs` keeps its own
/// private). Reads `value`/`writable`/`enumerable`/`configurable`/`get`/`set`,
/// keeping absent fields absent and rejecting mixed halves.
pub(crate) fn to_property_descriptor(
    ctx: &mut Ctx,
    v: JsValue,
) -> Result<crate::internal_methods::FullDescriptor, Throw> {
    let Some(obj) = v.as_object() else {
        return Err(Throw::Message(
            "TypeError: Property description must be an object".to_string(),
        ));
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
        let present = crate::internal_methods::dispatch_has(&mut *ctx.heap, obj, pk)
            .map_err(Throw::Value)?;
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
                if !got.is_undefined() {
                    let callable = got
                        .as_object()
                        .is_some_and(|h| ctx.heap.get(h).kind == v12_heap::Kind::Function);
                    if !callable {
                        return Err(Throw::Message(
                            "TypeError: Accessor property must be a function or undefined"
                                .to_string(),
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
    if desc.is_data() && desc.is_accessor() {
        return Err(Throw::Message(
            "TypeError: Invalid property descriptor: cannot both specify accessors and a value or writable attribute".to_string(),
        ));
    }
    Ok(desc)
}

/// `Reflect.getPrototypeOf(target)`.
pub fn reflect_get_prototype_of(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "getPrototypeOf")?;
    match ctx.heap.get(obj).prototype {
        Some(p) => Ok(JsValue::object(p)),
        None => Ok(JsValue::null()),
    }
}

/// `Reflect.setPrototypeOf(target, proto)` — boolean; `false` (not a throw)
/// when the target is non-extensible or the change would cycle.
pub fn reflect_set_prototype_of(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "setPrototypeOf")?;
    let proto = args.get(1).copied().unwrap_or(JsValue::undefined());
    let link = if proto.is_null() {
        None
    } else if let Some(h) = proto.as_object() {
        Some(h)
    } else {
        return Err(Throw::Message(
            "TypeError: Reflect.setPrototypeOf proto must be an object or null".to_string(),
        ));
    };
    // ES 9.1.2 step 4: same value short-circuits true, even when frozen.
    if link == ctx.heap.get(obj).prototype {
        return Ok(JsValue::from_bool(true));
    }
    if ctx.heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
        return Ok(JsValue::from_bool(false));
    }
    // Cycle guard: `proto` (or its chain) must not be `obj`.
    let mut cur = link;
    while let Some(p) = cur {
        if p == obj {
            return Ok(JsValue::from_bool(false));
        }
        cur = ctx.heap.get(p).prototype;
    }
    let cell = ctx.heap.validity_cell_of(obj);
    ctx.heap.bump_validity(cell);
    ctx.heap.bump_proto_generation();
    ctx.heap.get_mut(obj).prototype = link;
    Ok(JsValue::from_bool(true))
}

/// `Reflect.isExtensible(target)`.
pub fn reflect_is_extensible(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "isExtensible")?;
    let ext = ctx.heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE == 0;
    Ok(JsValue::from_bool(ext))
}

/// `Reflect.preventExtensions(target)` — always `true` for ordinary objects.
pub fn reflect_prevent_extensions(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "preventExtensions")?;
    ctx.heap.get_mut(obj).flags |= JsObject::FLAG_NOT_EXTENSIBLE;
    let cell = ctx.heap.validity_cell_of(obj);
    ctx.heap.bump_validity(cell);
    Ok(JsValue::from_bool(true))
}

/// `Reflect.has(target, key)`.
pub fn reflect_has(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "has")?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v)?;
    let found = crate::internal_methods::dispatch_has(&mut *ctx.heap, obj, pk)
        .map_err(|v| Throw::Value(v))?;
    Ok(JsValue::from_bool(found))
}

/// `Reflect.deleteProperty(target, key)`.
pub fn reflect_delete_property(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "deleteProperty")?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v)?;
    let kind = crate::internal_methods::kind_of(ctx.heap, obj);
    let deleted = (crate::internal_methods::methods_for(kind).delete)(&mut *ctx.heap, obj, pk)
        .map_err(Throw::Value)?;
    Ok(JsValue::from_bool(deleted))
}

/// `Reflect.ownKeys(target)`.
pub fn reflect_own_keys(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "ownKeys")?;
    let keys = own_keys_ordered(ctx.heap, obj);
    let items: Vec<JsValue> = keys
        .into_iter()
        .map(|k| match k.symbol() {
            Some(s) => JsValue::symbol(s),
            None => JsValue::string(k.string().expect("non-symbol key is a string")),
        })
        .collect();
    Ok(array_value(ctx, items))
}

/// `Reflect.getOwnPropertyDescriptor(target, key)`.
pub fn reflect_get_own_property_descriptor(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "getOwnPropertyDescriptor")?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v)?;
    Ok(build_descriptor(ctx, obj, pk).unwrap_or_else(JsValue::undefined))
}

/// `Reflect.defineProperty(target, key, attributes)` — returns a boolean,
/// never throws on an ordinary rejection.
pub fn reflect_define_property(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "defineProperty")?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v)?;
    let descriptor = if args.len() >= 3 {
        super::reflect::to_property_descriptor(ctx, args[2])?
    } else {
        crate::internal_methods::FullDescriptor::default()
    };
    let defined =
        crate::internal_methods::apply_property_descriptor(&mut *ctx.heap, obj, pk, descriptor)
            .map_err(Throw::Value)?;
    Ok(JsValue::from_bool(defined))
}

/// `Reflect.get(target, key[, receiver])`.
pub fn reflect_get(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = require_object(ctx, args, "get")?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v)?;
    let receiver = args.get(2).copied().unwrap_or(JsValue::object(obj));
    crate::internal_methods::dispatch_get(&mut *ctx.heap, obj, pk, receiver).map_err(Throw::Value)
}

/// The own-property shape of a receiver for the `Reflect.set` receiver walk.
enum OwnDesc {
    Absent,
    Data { writable: bool },
    Accessor { has_setter: bool },
}

/// `[[GetOwnProperty]]` reduced to the fields `Reflect.set` consults. A holed
/// data slot reads as `Absent` (deleted properties are not observable).
fn own_desc(heap: &mut Heap, obj: Handle<JsObject>, pk: PropKey) -> OwnDesc {
    let kind = heap.get(obj).kind;
    if (kind == v12_heap::Kind::Array || kind == v12_heap::Kind::Arguments)
        && let Some(idx) = crate::internal_methods::prop_key_to_index(heap, pk)
    {
        return match heap.get(obj).get_element(idx) {
            Some(v) if !v.is_hole() => OwnDesc::Data { writable: true },
            _ => OwnDesc::Absent,
        };
    }
    if let Some(entry) = heap
        .get(obj)
        .dictionary
        .as_ref()
        .and_then(|m| m.get(&pk))
        .copied()
    {
        if entry.is_accessor {
            return OwnDesc::Accessor {
                has_setter: entry.setter.is_some(),
            };
        }
        let live = heap
            .get(obj)
            .properties
            .get(entry.slot as usize)
            .is_some_and(|v| !v.is_hole());
        if !live {
            return OwnDesc::Absent;
        }
        return OwnDesc::Data {
            writable: entry.attrs.writable(),
        };
    }
    let shape = heap.shape_of(obj);
    match heap.lookup_property(shape, pk).copied() {
        Some(v12_heap::Descriptor::Data { slot, attrs, .. }) => {
            let live = heap
                .get(obj)
                .properties
                .get(slot as usize)
                .is_some_and(|v| !v.is_hole());
            if live {
                OwnDesc::Data {
                    writable: attrs.writable(),
                }
            } else {
                OwnDesc::Absent
            }
        }
        Some(v12_heap::Descriptor::Accessor { setter, .. }) => OwnDesc::Accessor {
            has_setter: setter.is_some(),
        },
        None => OwnDesc::Absent,
    }
}

/// Finds the descriptor that governs a write to `pk`, walking the prototype
/// chain when the receiver's own object does not carry `pk`.
fn inherited_own_desc(heap: &mut Heap, obj: Handle<JsObject>, pk: PropKey) -> OwnDesc {
    let mut cur = Some(obj);
    while let Some(o) = cur {
        match own_desc(heap, o, pk) {
            OwnDesc::Absent => cur = heap.get(o).prototype,
            found => return found,
        }
    }
    OwnDesc::Absent
}

/// `Reflect.set(target, key, value[, receiver])`.
pub fn reflect_set(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let target = require_object(ctx, args, "set")?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v)?;
    let value = args.get(2).copied().unwrap_or(JsValue::undefined());
    let receiver_v = args.get(3).copied().unwrap_or(JsValue::object(target));

    let desc = inherited_own_desc(ctx.heap, target, pk);
    match desc {
        OwnDesc::Data { writable: false } => Ok(JsValue::from_bool(false)),
        OwnDesc::Accessor { has_setter } => {
            // v1: a bytecode setter cannot be invoked from a native handler
            // (no interpreter re-entry). Native/Host setters could be called;
            // ordinary objects report their presence and succeed.
            Ok(JsValue::from_bool(has_setter))
        }
        OwnDesc::Absent | OwnDesc::Data { writable: true } => {
            let Some(receiver) = receiver_v.as_object() else {
                return Ok(JsValue::from_bool(false));
            };
            match own_desc(ctx.heap, receiver, pk) {
                OwnDesc::Accessor { .. } => Ok(JsValue::from_bool(false)),
                OwnDesc::Data { writable: false } => Ok(JsValue::from_bool(false)),
                OwnDesc::Data { writable: true } => {
                    let updated = crate::internal_methods::apply_property_descriptor(
                        &mut *ctx.heap,
                        receiver,
                        pk,
                        crate::internal_methods::FullDescriptor {
                            value: Some(value),
                            ..Default::default()
                        },
                    )
                    .map_err(Throw::Value)?;
                    Ok(JsValue::from_bool(updated))
                }
                OwnDesc::Absent => {
                    let created = crate::internal_methods::apply_property_descriptor(
                        &mut *ctx.heap,
                        receiver,
                        pk,
                        crate::internal_methods::FullDescriptor {
                            value: Some(value),
                            writable: Some(true),
                            enumerable: Some(true),
                            configurable: Some(true),
                            ..Default::default()
                        },
                    )
                    .map_err(Throw::Value)?;
                    Ok(JsValue::from_bool(created))
                }
            }
        }
    }
}

/// Whether `obj` satisfies `IsConstructor` for the paths v1 can distinguish:
/// a function object whose callable is a bytecode index (a real program
/// function) or one of the constructible engine natives. Native-seam
/// placeholders that decode to a `NativeId` are non-constructors.
fn is_constructor(heap: &Heap, obj: Handle<JsObject>) -> bool {
    let o = heap.get(obj);
    if o.kind != v12_heap::Kind::Function {
        return false;
    }
    match o.callable {
        v12_heap::FunctionTarget::Bytecode(idx) => match v12_native::NativeId::try_from(idx) {
            Ok(id) => matches!(
                id,
                v12_native::NativeId::ObjectConstruct
                    | v12_native::NativeId::ArrayConstruct
                    | v12_native::NativeId::BooleanConstruct
                    | v12_native::NativeId::ErrorCreate
                    | v12_native::NativeId::TypeErrorCreate
                    | v12_native::NativeId::RangeErrorCreate
                    | v12_native::NativeId::ReferenceErrorCreate
                    | v12_native::NativeId::SyntaxErrorCreate
                    | v12_native::NativeId::EvalErrorCreate
                    | v12_native::NativeId::UriErrorCreate
            ),
            Err(_) => true,
        },
        _ => false,
    }
}

/// `CreateListFromArrayLike(argumentsList)` for `Reflect.apply`/`construct`:
/// requires an object, reads `length`, then indices `0..len`.
pub fn create_list_from_array_like(
    ctx: &mut Ctx,
    v: JsValue,
) -> Result<Vec<JsValue>, Throw> {
    let Some(obj) = v.as_object() else {
        return Err(Throw::Message(
            "TypeError: CreateListFromArrayLike called on non-object".to_string(),
        ));
    };
    let len_key = PropKey::from_string(ctx.heap.intern_text("length"));
    let len_v = crate::internal_methods::dispatch_get(
        &mut *ctx.heap,
        obj,
        len_key,
        JsValue::object(obj),
    )
    .map_err(Throw::Value)?;
    let len = ctx.to_number(len_v);
    let len = if len.is_finite() && len > 0.0 {
        len.trunc() as usize
    } else {
        0
    };
    let mut out = Vec::with_capacity(len.min(1024));
    for i in 0..len {
        let key = PropKey::from_string(ctx.heap.intern_text(&i.to_string()));
        let v = crate::internal_methods::dispatch_get(
            &mut *ctx.heap,
            obj,
            key,
            JsValue::object(obj),
        )
        .map_err(Throw::Value)?;
        out.push(v);
    }
    Ok(out)
}

/// `Reflect.apply(target, thisArgument, argumentsList)`.
pub fn reflect_apply(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let target_v = args.first().copied().unwrap_or(JsValue::undefined());
    let Some(target) = target_v.as_object() else {
        return Err(ctx.type_error("Reflect.apply target is not a function"));
    };
    if ctx.heap.get(target).kind != v12_heap::Kind::Function {
        return Err(ctx.type_error("Reflect.apply target is not a function"));
    }
    let this_arg = args.get(1).copied().unwrap_or(JsValue::undefined());
    let list = args.get(2).copied().unwrap_or(JsValue::undefined());
    let call_args = create_list_from_array_like(ctx, list)?;
    // Invocable without interpreter re-entry: engine natives and host
    // closures. Bytecode targets need the interpreter and report a honest
    // failure (documented residual blocker).
    match ctx.heap.get(target).callable {
        v12_heap::FunctionTarget::Native(f) => {
            f(ctx.heap, this_arg, &call_args).map_err(Throw::Value)
        }
        v12_heap::FunctionTarget::Host(c) => {
            c.call(ctx.heap, this_arg, &call_args).map_err(Throw::Value)
        }
        _ => Err(ctx.type_error("Reflect.apply requires the interpreter for bytecode targets")),
    }
}

/// `Reflect.construct(target, argumentsList[, newTarget])`.
pub fn reflect_construct(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let target_v = args.first().copied().unwrap_or(JsValue::undefined());
    let Some(target) = target_v.as_object() else {
        return Err(ctx.type_error("Reflect.construct target is not a constructor"));
    };
    if !is_constructor(ctx.heap, target) {
        return Err(ctx.type_error("Reflect.construct target is not a constructor"));
    }
    let list = args.get(1).copied().unwrap_or(JsValue::undefined());
    let call_args = create_list_from_array_like(ctx, list)?;
    let new_target = args.get(2).copied().unwrap_or(target_v);
    let Some(new_target_obj) = new_target.as_object() else {
        return Err(ctx.type_error("Reflect.construct newTarget is not a constructor"));
    };
    if !is_constructor(ctx.heap, new_target_obj) {
        return Err(ctx.type_error("Reflect.construct newTarget is not a constructor"));
    }
    // The instance's [[Prototype]] is `newTarget.prototype`; the actual
    // [[Construct]] invocation needs the interpreter (residual blocker).
    let _ = (call_args, new_target_obj);
    Err(ctx.type_error("Reflect.construct requires the interpreter"))
}
