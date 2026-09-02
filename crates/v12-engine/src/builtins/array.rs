//! Array built-ins: push, pop, and length handling.

use v12_heap::{Handle, Heap, JsObject, JsValue, V12Str};
use v12_native::Throw;

use super::{helpers, intern_type_error};

/// Maximum array length (2^32 - 1).
const MAX_ARRAY_LENGTH: u32 = u32::MAX;

/// Length property key, interned lazily via the heap.
fn length_prop(heap: &mut Heap) -> v12_heap::PropKey {
    let h = heap.intern_string(V12Str::latin1(b"length".to_vec()));
    v12_heap::PropKey::from_string(h)
}

/// `Array.prototype.push(...items)` – appends elements and updates `length`.
pub fn array_push(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.push", None)?;
    let len = array_length(heap, obj);
    if len as usize + args.len() > MAX_ARRAY_LENGTH as usize {
        return Err((intern_type_error(heap, "RangeError: invalid array length")).into());
    }
    for &item in args {
        let obj_mut = heap.get_mut(obj);
        if obj_mut.kind == v12_heap::Kind::Array {
            obj_mut.elements_array.push(item);
        } else {
            obj_mut.elements.push(item);
        }
    }
    let new_len = if heap.get(obj).kind == v12_heap::Kind::Array {
        heap.get(obj).elements_array.len() as u32
    } else {
        heap.get(obj).elements.len() as u32
    };
    sync_length(heap, obj, new_len);
    Ok(helpers::smi_or_f64(i64::from(new_len)))
}

/// `Array.prototype.pop()` – removes the last element.
pub fn array_pop(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = helpers::as_object(heap, this, "Array.prototype.pop", None)?;
    let popped = if heap.get(obj).kind == v12_heap::Kind::Array {
        heap.get_mut(obj)
            .elements_array
            .pop()
            .unwrap_or(JsValue::undefined())
    } else {
        heap.get_mut(obj)
            .elements
            .pop()
            .unwrap_or(JsValue::undefined())
    };
    let value = if popped.is_hole() {
        JsValue::undefined()
    } else {
        popped
    };
    let new_len = if heap.get(obj).kind == v12_heap::Kind::Array {
        heap.get(obj).elements_array.len() as u32
    } else {
        heap.get(obj).elements.len() as u32
    };
    sync_length(heap, obj, new_len);
    Ok(value)
}

fn array_length(heap: &mut Heap, obj: Handle<JsObject>) -> u32 {
    let key = length_prop(heap);
    let shape = heap.root_shape();
    if let Some(desc) = heap.lookup_property(shape, key)
        && let Some(slot) = desc.slot()
    {
        let idx = slot as usize;
        let v = heap
            .get(obj)
            .properties
            .get(idx)
            .copied()
            .unwrap_or(JsValue::undefined());
        if let Some(n) = v.as_smi()
            && let Ok(u) = u32::try_from(n)
        {
            return u;
        }
        if let Some(n) = v.as_f64()
            && n.is_finite()
            && n >= 0.0
            && n.fract() == 0.0
        {
            return n as u32;
        }
    }
    // Fallback to elements length.
    heap.get(obj).elements.len() as u32
}

/// `Array.isArray(value)` – true if value is an Array exotic object.
pub fn array_is_array(_heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let v = args.first().copied().unwrap_or(JsValue::undefined());
    let is = v
        .as_object()
        .is_some_and(|h| _heap.get(h).kind == v12_heap::Kind::Array);
    Ok(if is { JsValue::true_() } else { JsValue::false_() })
}

fn sync_length(heap: &mut Heap, obj: Handle<JsObject>, len: u32) {
    let key = length_prop(heap);
    let shape = heap.root_shape();
    if let Some(desc) = heap.lookup_property(shape, key)
        && let Some(slot) = desc.slot()
    {
        let idx = slot as usize;
        if idx < heap.get(obj).properties.len() {
            heap.get_mut(obj).properties[idx] = JsValue::from_f64(f64::from(len));
            return;
        }
    }
    // No length slot yet: create one via shape extension if needed.
    if heap.get(obj).properties.len() < 1024 {
        let _child = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
        heap.get_mut(obj)
            .properties
            .push(JsValue::from_f64(f64::from(len)));
    }
}
