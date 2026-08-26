//! Array built-ins: push, pop, and length handling.

use v12_heap::{Handle, Heap, JsObject, JsValue, V12Str};

use super::intern_type_error;

/// Maximum array length (2^32 - 1).
const MAX_ARRAY_LENGTH: u32 = u32::MAX;

/// Length property key, interned lazily via the heap.
fn length_prop(heap: &mut Heap) -> v12_heap::PropKey {
    let h = heap.intern_string(V12Str::latin1(b"length".to_vec()));
    v12_heap::PropKey::from_string(h)
}

/// `Array.prototype.push(...items)` – appends elements and updates `length`.
pub fn array_push(heap: &mut Heap, this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let Some(obj) = this.as_object() else {
        return Err(intern_type_error(
            heap,
            "TypeError: Array.prototype.push called on non-object",
        ));
    };
    let len = array_length(heap, obj);
    if len as usize + args.len() > MAX_ARRAY_LENGTH as usize {
        return Err(intern_type_error(heap, "RangeError: invalid array length"));
    }
    for &item in args {
        heap.get_mut(obj).elements.push(item);
    }
    let new_len = heap.get(obj).elements.len() as u32;
    sync_length(heap, obj, new_len);
    let len_value =
        JsValue::from_i32_smi(new_len as i32).unwrap_or(JsValue::from_f64(f64::from(new_len)));
    Ok(len_value)
}

/// `Array.prototype.pop()` – removes the last element.
pub fn array_pop(heap: &mut Heap, this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
    let Some(obj) = this.as_object() else {
        return Err(intern_type_error(
            heap,
            "TypeError: Array.prototype.pop called on non-object",
        ));
    };
    let popped = heap
        .get_mut(obj)
        .elements
        .pop()
        .unwrap_or(JsValue::undefined());
    let value = if popped.is_hole() {
        JsValue::undefined()
    } else {
        popped
    };
    let new_len = heap.get(obj).elements.len() as u32;
    sync_length(heap, obj, new_len);
    Ok(value)
}

fn array_length(heap: &mut Heap, obj: Handle<JsObject>) -> u32 {
    let key = length_prop(heap);
    let shape = heap.root_shape();
    if let Some(desc) = heap.lookup_property(shape, key) {
        let slot = desc.slot as usize;
        let v = heap
            .get(obj)
            .properties
            .get(slot)
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

fn sync_length(heap: &mut Heap, obj: Handle<JsObject>, len: u32) {
    let key = length_prop(heap);
    let shape = heap.root_shape();
    if let Some(desc) = heap.lookup_property(shape, key) {
        let slot = desc.slot as usize;
        if slot < heap.get(obj).properties.len() {
            heap.get_mut(obj).properties[slot] = JsValue::from_f64(f64::from(len));
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
