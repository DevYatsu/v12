//! Object built-ins.

use v12_heap::{Attrs, Handle, Heap, JsObject, JsValue, PropKey, V12Str};
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

    // Attempt to find existing descriptor via shape.
    let shape = heap.root_shape();
    // For the engine's own heap objects we maintain shapes in the interpreter
    // layer; here we directly push to the property vector when the shape does not
    // already contain the key, mirroring the interpreter's shape-blind path for
    // the skeleton.
    if heap.get(obj).properties.len() >= 1024 {
        return Err((intern_type_error(heap, "TypeError: too many properties")).into());
    }
    // Check if property already exists by scanning heap lookup (best effort).
    let exists = heap.lookup_property(shape, key).is_some();
    if exists {
        // Overwrite slot 0 for skeleton simplicity.
        if !heap.get(obj).properties.is_empty() {
            heap.get_mut(obj).properties[0] = value;
        }
    } else {
        let child = heap.add_property(shape, key, Attrs::DEFAULT);
        // Publishing the child shape would normally bind via validity cell side
        // table; for this heap-local use we rely on the shape being anchored via
        // the heap's root transitions until the next allocation,
        // satisfying the allocation contract before the property store.
        let _ = child;
        heap.get_mut(obj).properties.push(value);
    }
    Ok(JsValue::object(obj))
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
    Ok(helpers::intern_text(heap, &text))
}
