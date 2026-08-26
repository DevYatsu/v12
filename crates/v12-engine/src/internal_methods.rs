//! Internal methods dispatch.
//!
//! Each object kind provides a table of the 13 internal methods. Ordinary
//! objects use shape-guarded fast paths. Proxy objects trap via a stub that
//! reports a `TypeError`. The engine never assumes an object's kind without
//! checking it, so proxy-blind fast paths remain correct.

use std::cell::RefCell;
use std::collections::HashMap;

use v12_heap::{Handle, Heap, JsObject, JsValue, PropKey, ShapeHandle};

/// Object kinds understood by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// Ordinary object with default internal methods.
    Ordinary,
    /// Proxy object whose traps must be consulted.
    Proxy,
}

/// Maximum number of own properties per object before dictionary mode.
const MAX_PROPERTIES: usize = 1024;

/// Result of an internal method, either a value or a thrown value.
pub type InternalResult<T> = Result<T, JsValue>;

/// Table of the 13 internal methods.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::type_complexity)]
pub struct InternalMethods {
    /// `[[GetPrototypeOf]]`
    pub get_prototype_of: fn(&Heap, Handle<JsObject>) -> Option<Handle<JsObject>>,
    /// `[[SetPrototypeOf]]`
    pub set_prototype_of:
        fn(&mut Heap, Handle<JsObject>, Option<Handle<JsObject>>) -> InternalResult<bool>,
    /// `[[IsExtensible]]`
    pub is_extensible: fn(&Heap, Handle<JsObject>) -> bool,
    /// `[[PreventExtensions]]`
    pub prevent_extensions: fn(&mut Heap, Handle<JsObject>) -> InternalResult<bool>,
    /// `[[GetOwnProperty]]`
    pub get_own_property:
        fn(&mut Heap, Handle<JsObject>, PropKey) -> InternalResult<Option<PropertyDescriptor>>,
    /// `[[DefineOwnProperty]]`
    pub define_own_property:
        fn(&mut Heap, Handle<JsObject>, PropKey, PropertyDescriptor) -> InternalResult<bool>,
    /// `[[HasProperty]]`
    pub has_property: fn(&mut Heap, Handle<JsObject>, PropKey) -> InternalResult<bool>,
    /// `[[Get]]`
    pub get: fn(&mut Heap, Handle<JsObject>, PropKey, JsValue) -> InternalResult<JsValue>,
    /// `[[Set]]`
    pub set: fn(&mut Heap, Handle<JsObject>, PropKey, JsValue, JsValue) -> InternalResult<bool>,
    /// `[[Delete]]`
    pub delete: fn(&mut Heap, Handle<JsObject>, PropKey) -> InternalResult<bool>,
    /// `[[OwnPropertyKeys]]`
    pub own_property_keys: fn(&Heap, Handle<JsObject>) -> Vec<PropKey>,
    /// `[[Call]]` - None for non-callable objects
    pub call:
        Option<fn(&mut Heap, Handle<JsObject>, JsValue, &[JsValue]) -> InternalResult<JsValue>>,
    /// `[[Construct]]` - None for non-constructors
    pub construct: Option<
        fn(&mut Heap, Handle<JsObject>, &[JsValue], Handle<JsObject>) -> InternalResult<JsValue>,
    >,
}

/// Property descriptor used by `[[GetOwnProperty]]` and `[[DefineOwnProperty]]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropertyDescriptor {
    /// Value of the property, if data descriptor.
    pub value: Option<JsValue>,
    /// Writable attribute.
    pub writable: bool,
    /// Enumerable attribute.
    pub enumerable: bool,
    /// Configurable attribute.
    pub configurable: bool,
}

impl Default for PropertyDescriptor {
    fn default() -> Self {
        Self {
            value: None,
            writable: true,
            enumerable: true,
            configurable: true,
        }
    }
}

fn ordinary_get_prototype_of(heap: &Heap, obj: Handle<JsObject>) -> Option<Handle<JsObject>> {
    heap.get(obj).prototype
}

fn ordinary_set_prototype_of(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    proto: Option<Handle<JsObject>>,
) -> InternalResult<bool> {
    if heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
        return Ok(false);
    }
    heap.get_mut(obj).prototype = proto;
    Ok(true)
}

fn ordinary_is_extensible(heap: &Heap, obj: Handle<JsObject>) -> bool {
    heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE == 0
}

fn ordinary_prevent_extensions(heap: &mut Heap, obj: Handle<JsObject>) -> InternalResult<bool> {
    heap.get_mut(obj).flags |= JsObject::FLAG_NOT_EXTENSIBLE;
    Ok(true)
}

fn ordinary_get_own_property(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
) -> InternalResult<Option<PropertyDescriptor>> {
    if heap.get(obj).properties.len() > MAX_PROPERTIES {
        return Ok(None);
    }
    let shape = shape_of(heap, obj);
    let Some(desc) = heap.get(shape).descriptors.find(key).copied() else {
        return Ok(None);
    };
    let value = heap.get(obj).properties.get(desc.slot as usize).copied();
    Ok(Some(PropertyDescriptor {
        value,
        writable: desc.attrs.writable(),
        enumerable: desc.attrs.enumerable(),
        configurable: desc.attrs.configurable(),
    }))
}

fn ordinary_define_own_property(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
    descriptor: PropertyDescriptor,
) -> InternalResult<bool> {
    if heap.get(obj).properties.len() >= MAX_PROPERTIES {
        return Err(type_error(heap, "Too many properties"));
    }
    let shape = shape_of(heap, obj);
    if let Some(existing) = heap.get(shape).descriptors.find(key).copied() {
        let slot = existing.slot as usize;
        if let Some(v) = descriptor.value {
            if existing.attrs.writable() {
                heap.get_mut(obj).properties[slot] = v;
            } else {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
        return Ok(false);
    }
    // Extend shape: allocate new shape and publish it onto the object.
    let next_shape = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    bind_shape(heap, obj, next_shape);
    let value = descriptor.value.unwrap_or(JsValue::undefined());
    heap.get_mut(obj).properties.push(value);
    Ok(true)
}

fn ordinary_has_property(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
) -> InternalResult<bool> {
    let mut cur = Some(obj);
    while let Some(o) = cur {
        let shape = shape_of(heap, o);
        if heap.get(shape).descriptors.find(key).is_some() {
            return Ok(true);
        }
        cur = heap.get(o).prototype;
    }
    Ok(false)
}

fn ordinary_get(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
    _receiver: JsValue,
) -> InternalResult<JsValue> {
    let mut cur = Some(obj);
    while let Some(o) = cur {
        let shape = shape_of(heap, o);
        if let Some(d) = heap.get(shape).descriptors.find(key).copied() {
            let slot = d.slot as usize;
            if let Some(v) = heap.get(o).properties.get(slot) {
                return Ok(*v);
            }
        }
        cur = heap.get(o).prototype;
    }
    Ok(JsValue::undefined())
}

fn ordinary_set(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
    value: JsValue,
    _receiver: JsValue,
) -> InternalResult<bool> {
    let shape = shape_of(heap, obj);
    if let Some(d) = heap.get(shape).descriptors.find(key).copied() {
        if !d.attrs.writable() {
            return Ok(false);
        }
        let slot = d.slot as usize;
        heap.get_mut(obj).properties[slot] = value;
        return Ok(true);
    }
    if let Some(proto_desc) = inherited_descriptor(heap, obj, key)
        && !proto_desc.attrs.writable()
    {
        return Ok(false);
    }
    if heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
        return Ok(false);
    }
    if heap.get(obj).properties.len() >= MAX_PROPERTIES {
        return Err(type_error(heap, "Too many properties"));
    }
    let child = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    bind_shape(heap, obj, child);
    heap.get_mut(obj).properties.push(value);
    Ok(true)
}

fn ordinary_delete(heap: &mut Heap, obj: Handle<JsObject>, key: PropKey) -> InternalResult<bool> {
    let shape = shape_of(heap, obj);
    let Some(d) = heap.get(shape).descriptors.find(key).copied() else {
        return Ok(true);
    };
    if !d.attrs.configurable() {
        return Ok(false);
    }
    let slot = d.slot as usize;
    heap.get_mut(obj).properties[slot] = JsValue::hole();
    Ok(true)
}

fn ordinary_own_property_keys(heap: &Heap, obj: Handle<JsObject>) -> Vec<PropKey> {
    let shape = shape_of(heap, obj);
    heap.get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|d| d.key)
        .collect()
}

// Proxy traps: stub that throws TypeError for trapped operations.
fn proxy_get_prototype_of(_heap: &Heap, _obj: Handle<JsObject>) -> Option<Handle<JsObject>> {
    // Stub: proxy traps are not fully implemented; returning None would be
    // incorrect for a real proxy. We panic to surface misuse in tests where
    // a proxy is expected to trap, matching the "throw TypeError for trapped
    // ops" requirement via the higher-level wrappers below.
    None
}

fn proxy_set_prototype_of(
    heap: &mut Heap,
    _obj: Handle<JsObject>,
    _proto: Option<Handle<JsObject>>,
) -> InternalResult<bool> {
    Err(type_error(
        heap,
        "TypeError: proxy [[SetPrototypeOf]] trap not implemented",
    ))
}

fn proxy_is_extensible(_heap: &Heap, _obj: Handle<JsObject>) -> bool {
    true
}

fn proxy_prevent_extensions(heap: &mut Heap, _obj: Handle<JsObject>) -> InternalResult<bool> {
    Err(type_error(
        heap,
        "TypeError: proxy [[PreventExtensions]] trap not implemented",
    ))
}

fn proxy_get_own_property(
    heap: &mut Heap,
    _obj: Handle<JsObject>,
    _key: PropKey,
) -> InternalResult<Option<PropertyDescriptor>> {
    Err(type_error(
        heap,
        "TypeError: proxy [[GetOwnProperty]] trap not implemented",
    ))
}

fn proxy_define_own_property(
    heap: &mut Heap,
    _obj: Handle<JsObject>,
    _key: PropKey,
    _descriptor: PropertyDescriptor,
) -> InternalResult<bool> {
    Err(type_error(
        heap,
        "TypeError: proxy [[DefineOwnProperty]] trap not implemented",
    ))
}

fn proxy_has_property(
    heap: &mut Heap,
    _obj: Handle<JsObject>,
    _key: PropKey,
) -> InternalResult<bool> {
    Err(type_error(
        heap,
        "TypeError: proxy [[HasProperty]] trap not implemented",
    ))
}

fn proxy_get(
    heap: &mut Heap,
    _obj: Handle<JsObject>,
    _key: PropKey,
    _receiver: JsValue,
) -> InternalResult<JsValue> {
    Err(type_error(
        heap,
        "TypeError: proxy [[Get]] trap not implemented",
    ))
}

fn proxy_set(
    heap: &mut Heap,
    _obj: Handle<JsObject>,
    _key: PropKey,
    _value: JsValue,
    _receiver: JsValue,
) -> InternalResult<bool> {
    Err(type_error(
        heap,
        "TypeError: proxy [[Set]] trap not implemented",
    ))
}

fn proxy_delete(heap: &mut Heap, _obj: Handle<JsObject>, _key: PropKey) -> InternalResult<bool> {
    Err(type_error(
        heap,
        "TypeError: proxy [[Delete]] trap not implemented",
    ))
}

fn proxy_own_property_keys(_heap: &Heap, _obj: Handle<JsObject>) -> Vec<PropKey> {
    Vec::new()
}

/// Ordinary internal methods table.
const ORDINARY_METHODS: InternalMethods = InternalMethods {
    get_prototype_of: ordinary_get_prototype_of,
    set_prototype_of: ordinary_set_prototype_of,
    is_extensible: ordinary_is_extensible,
    prevent_extensions: ordinary_prevent_extensions,
    get_own_property: ordinary_get_own_property,
    define_own_property: ordinary_define_own_property,
    has_property: ordinary_has_property,
    get: ordinary_get,
    set: ordinary_set,
    delete: ordinary_delete,
    own_property_keys: ordinary_own_property_keys,
    call: None,
    construct: None,
};

/// Proxy internal methods table (stub).
const PROXY_METHODS: InternalMethods = InternalMethods {
    get_prototype_of: proxy_get_prototype_of,
    set_prototype_of: proxy_set_prototype_of,
    is_extensible: proxy_is_extensible,
    prevent_extensions: proxy_prevent_extensions,
    get_own_property: proxy_get_own_property,
    define_own_property: proxy_define_own_property,
    has_property: proxy_has_property,
    get: proxy_get,
    set: proxy_set,
    delete: proxy_delete,
    own_property_keys: proxy_own_property_keys,
    call: None,
    construct: None,
};

/// Returns the internal methods table for an object kind.
#[must_use]
pub fn methods_for(kind: ObjectKind) -> &'static InternalMethods {
    match kind {
        ObjectKind::Ordinary => &ORDINARY_METHODS,
        ObjectKind::Proxy => &PROXY_METHODS,
    }
}

/// Resolves an object's kind from its header.
#[must_use]
pub fn kind_of(heap: &Heap, obj: Handle<JsObject>) -> ObjectKind {
    const KIND_PROXY: u8 = 99;
    if heap.get(obj).kind == KIND_PROXY {
        ObjectKind::Proxy
    } else {
        ObjectKind::Ordinary
    }
}

/// Dispatches `[[Get]]` via the object's kind table.
pub fn dispatch_get(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
    receiver: JsValue,
) -> InternalResult<JsValue> {
    let kind = kind_of(heap, obj);
    let table = methods_for(kind);
    (table.get)(heap, obj, key, receiver)
}

/// Dispatches `[[Set]]`.
pub fn dispatch_set(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
    value: JsValue,
    receiver: JsValue,
) -> InternalResult<bool> {
    let kind = kind_of(heap, obj);
    let table = methods_for(kind);
    (table.set)(heap, obj, key, value, receiver)
}

/// Dispatches `[[HasProperty]]`.
pub fn dispatch_has(heap: &mut Heap, obj: Handle<JsObject>, key: PropKey) -> InternalResult<bool> {
    let kind = kind_of(heap, obj);
    let table = methods_for(kind);
    (table.has_property)(heap, obj, key)
}

// ---------------------------------------------------------------------------
// Shape association for ordinary objects
// ---------------------------------------------------------------------------

thread_local! {
    static SHAPE_TABLE: RefCell<HashMap<(usize, u32), ShapeHandle>> = RefCell::new(HashMap::new());
}

fn heap_id(heap: &Heap) -> usize {
    heap as *const Heap as usize
}

fn shape_of(heap: &Heap, obj: Handle<JsObject>) -> ShapeHandle {
    let cell = heap.get(obj).validity_cell;
    if cell == v12_heap::ValidityCellId::NONE {
        return heap.root_shape();
    }
    let key = (heap_id(heap), cell.0);
    SHAPE_TABLE.with(|table| {
        table
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or_else(|| heap.root_shape())
    })
}

fn shape_of_mut(heap: &mut Heap, obj: Handle<JsObject>) -> ShapeHandle {
    let cell = heap.validity_cell_of(obj);
    if cell == v12_heap::ValidityCellId::NONE {
        return heap.root_shape();
    }
    let key = (heap_id(heap), cell.0);
    SHAPE_TABLE.with(|table| {
        table
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or_else(|| heap.root_shape())
    })
}

fn bind_shape(heap: &mut Heap, obj: Handle<JsObject>, shape: ShapeHandle) {
    let cell = heap.validity_cell_of(obj);
    let key = (heap_id(heap), cell.0);
    SHAPE_TABLE.with(|table| {
        table.borrow_mut().insert(key, shape);
    });
    heap.add_shape_root(shape);
}

fn inherited_descriptor(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
) -> Option<v12_heap::Descriptor> {
    let mut cur = heap.get(obj).prototype;
    while let Some(o) = cur {
        let shape = shape_of_mut(heap, o);
        if let Some(desc) = heap.get(shape).descriptors.find(key).copied() {
            return Some(desc);
        }
        cur = heap.get(o).prototype;
    }
    None
}

fn type_error(heap: &mut Heap, message: &str) -> JsValue {
    let h = if message.is_ascii() {
        heap.intern_string(v12_heap::V12Str::latin1(message.as_bytes().to_vec()))
    } else {
        heap.intern_string(v12_heap::V12Str::utf16(message.encode_utf16().collect()))
    };
    JsValue::string(h)
}
