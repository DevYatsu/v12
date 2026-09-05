//! Object built-ins.

use v12_heap::{Handle, Heap, JsObject, JsValue, PropKey, V12Str};
use v12_native::Throw;

use super::{helpers, intern_type_error};

/// `Object.create(proto)` – creates a new ordinary object with `proto` as
/// its prototype. `proto` may be an object or `null`.
pub fn object_create(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let proto = args.first().copied().unwrap_or(JsValue::null());
    let proto_handle = if proto.is_null() {
        None
    } else if let Some(h) = proto.as_object() {
        Some(h)
    } else {
        return Err((intern_type_error(
            heap,
            "TypeError: Object.create prototype must be object or null",
        ))
        .into());
    };
    let obj = heap.alloc(JsObject::environment(0, proto_handle));
    Ok(JsValue::object(obj))
}

/// `Object.getPrototypeOf(obj)` – returns the prototype.
pub fn object_get_prototype_of(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| Throw::type_error(heap, "Object.getPrototypeOf called on non-object"))?;
    match heap.get(obj).prototype {
        Some(p) => Ok(JsValue::object(p)),
        None => Ok(JsValue::null()),
    }
}

/// `Object.defineProperty(obj, key, descriptor)` – defines a property via
/// shape. The descriptor is simplified to a single value argument for this
/// stage; it creates a writable configurable enumerable data property.
pub fn object_define_property(
    heap: &mut Heap,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    if args.len() < 2 {
        return Err((intern_type_error(
            heap,
            "TypeError: Object.defineProperty requires 2 arguments",
        ))
        .into());
    }
    let obj = args[0]
        .as_object()
        .ok_or_else(|| Throw::type_error(heap, "Object.defineProperty called on non-object"))?;
    let key = property_key(heap, args[1])?;
    let value = args.get(2).copied().unwrap_or(JsValue::undefined());

    // Delegate to the ordinary [[DefineOwnProperty]] implementation: it
    // dispatches on object kind (arrays, arguments exotics) and handles the
    // shape extension + binding for new keys. A missing value argument
    // defines a writable/enumerable/configurable data property.
    crate::internal_methods::ordinary_define_own_property(
        heap,
        obj,
        key,
        crate::internal_methods::PropertyDescriptor {
            value: Some(value),
            ..Default::default()
        },
    )
    .map_err(Throw::Value)?;
    Ok(JsValue::object(obj))
}

pub fn object_keys(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| Throw::type_error(heap, "Object.keys called on non-object"))?;
    let keys = collect_own_string_keys(heap, obj);
    let arr = heap.alloc(v12_heap::JsObject::array(keys.iter().map(|&k| JsValue::string(k)).collect()));
    heap.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

pub fn object_values(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| Throw::type_error(heap, "Object.values called on non-object"))?;
    let vals = collect_own_values(heap, obj);
    let arr = heap.alloc(v12_heap::JsObject::array(vals));
    heap.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

pub fn object_entries(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| Throw::type_error(heap, "Object.entries called on non-object"))?;
    let keys = collect_own_string_keys(heap, obj);
    let vals = collect_own_values(heap, obj);
    let pairs: Vec<JsValue> = keys.into_iter().zip(vals).map(|(k, v)| {
        let ks = JsValue::string(k);
        let pair = heap.alloc(v12_heap::JsObject::array(vec![ks, v]));
        heap.add_root(JsValue::object(pair));
        JsValue::object(pair)
    }).collect();
    let arr = heap.alloc(v12_heap::JsObject::array(pairs));
    heap.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

pub fn object_has_own_property(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let this_obj = this.as_object().ok_or_else(|| Throw::type_error(heap, "Object.prototype.hasOwnProperty called on non-object"))?;
    let key = args.first().copied().unwrap_or(JsValue::undefined());
    let pk = property_key(heap, key).map_err(|e| Throw::Value(e))?;
    let found = heap.get(this_obj).property_keys.iter().any(|k| k.is_some_and(|kk| kk == pk));
    Ok(JsValue::from_bool(found))
}

pub fn object_proto_to_string(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = if this.is_object() && heap.get(this.as_object().unwrap()).kind == v12_heap::Kind::Array { "[object Array]" } else { "[object Object]" };
    Ok(JsValue::string(heap.intern_text(text)))
}

pub fn object_proto_value_of(_heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(this)
}

pub fn function_proto_to_string(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(JsValue::string(heap.intern_text("function() {}")))
}

fn collect_own_string_keys(heap: &Heap, obj: Handle<v12_heap::JsObject>) -> Vec<Handle<V12Str>> {
    heap.get(obj).property_keys.iter().filter_map(|k| k.as_ref().and_then(|pk| pk.string())).collect()
}

fn collect_own_values(heap: &Heap, obj: Handle<v12_heap::JsObject>) -> Vec<JsValue> {
    // property_keys and properties are parallel; filter to string keys
    let o = heap.get(obj);
    o.property_keys.iter().zip(o.properties.iter()).filter_map(|(k, v)| if k.as_ref().is_some_and(|pk| pk.is_string()) { Some(*v) } else { None }).collect()
}

fn property_key(heap: &mut Heap, v: JsValue) -> Result<PropKey, JsValue> {
    if let Some(h) = v.as_string() {
        return Ok(PropKey::from_string(h));
    }
    if let Some(sym) = v.as_symbol() {
        return Ok(PropKey::from_symbol(sym));
    }
    // Coerce via ToString.
    let h = to_string_handle(heap, v)?;
    Ok(PropKey::from_string(h))
}

fn to_string_handle(heap: &mut Heap, v: JsValue) -> Result<Handle<V12Str>, JsValue> {
    let text = helpers::value_text(heap, v);
    Ok(heap.intern_text(&text))
}
