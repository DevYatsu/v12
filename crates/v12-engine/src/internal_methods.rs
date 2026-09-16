//! Internal methods dispatch.
//!
//! Each object kind provides a table of the 13 internal methods. Ordinary
//! objects use shape-guarded fast paths. Proxy objects trap via a stub that
//! reports a `TypeError`. The engine never assumes an object's kind without
//! checking it, so proxy-blind fast paths remain correct.
//!
//! **Dispatch-table convention:** this module is the engine's internal-method
//! vtable — a flat function-pointer table indexed by [`ObjectKind`], resolved
//! by a `match`, never `dyn`/reflection. New object kinds add a row to the
//! table (and to the `match` in the dispatch helpers); the table stays
//! predictable and trace-friendly, which is what lets the JIT tiers emit
//! shape-guarded direct calls instead of runtime lookups. The remaining
//! `dyn` sites in the codebase are confined to the control-path seam
//! (`JitExecFn` in `v12-codegen`); the engine's hot dispatch is always flat
//! `match` over a concrete enum.

use v12_heap::{DictEntry, Handle, Heap, JsObject, JsValue, PropKey, ShapeHandle};

/// Object kinds understood by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// Ordinary object with default internal methods.
    Ordinary,
    /// Proxy object whose traps must be consulted.
    Proxy,
}

/// Own-property count past which growth spills from shapes to the
/// per-object dictionary rung (the named-property mirror of
/// `ElementsKind::Dictionary`): shapes stop growing, reads stay O(1)
/// via the map. Well above the inline tier (8), well below the 1024
/// cliff it replaces — growth past it is unbounded by design now.

/// Maximum number of own properties per object before dictionary mode.
const NAMED_DICT_THRESHOLD: usize = 128;

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
    // Rewiring invalidates chain assumptions: bump the object's own cell
    // and the heap proto epoch so stub-cache proto entries re-verify.
    // (Entries additionally verify the recorded links, so rewiring through
    // any other call site stays correct too.)
    let cell = heap.validity_cell_of(obj);
    heap.bump_validity(cell);
    heap.bump_proto_generation();
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
    let kind = heap.get(obj).kind;
    if (kind == v12_heap::Kind::Arguments || kind == v12_heap::Kind::Array)
        && let Some(idx) = prop_key_as_index(heap, key)
        && let Some(v) = heap.get(obj).get_element(idx)
    {
        return Ok(Some(PropertyDescriptor {
            value: Some(v),
            writable: true,
            enumerable: true,
            configurable: true,
        }));
    }
    // Dictionary rung first: overflow keys live only here (disjoint from
    // the frozen base shape). Indexed shapes serve any size now, so the
    // old length cliff is gone with them.
    if let Some((entry, value)) = dict_lookup(heap, obj, key) {
        return Ok(Some(dict_property_descriptor(value, entry)));
    }
    let shape = shape_of(heap, obj);
    let Some(desc) = heap.get(shape).descriptors.find(key).copied() else {
        return Ok(None);
    };
    match desc {
        v12_heap::Descriptor::Data { slot, attrs, .. } => {
            let value = heap.get(obj).properties.get(slot as usize).copied();
            Ok(Some(PropertyDescriptor {
                value,
                writable: attrs.writable(),
                enumerable: attrs.enumerable(),
                configurable: attrs.configurable(),
            }))
        }
        v12_heap::Descriptor::Accessor { attrs, .. } => Ok(Some(PropertyDescriptor {
            // Accessor's own property report has no direct value; callers
            // should use `dispatch_get` to invoke the getter.
            value: None,
            writable: false,
            enumerable: attrs.enumerable(),
            configurable: attrs.configurable(),
        })),
    }
}

pub(crate) fn ordinary_define_own_property(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
    descriptor: PropertyDescriptor,
) -> InternalResult<bool> {
    // Growth past the spill threshold goes to the dictionary rung; the
    // old 1024-throw cliff is gone by design (unbounded growth, O(1)
    // reads throughout).
    let kind = heap.get(obj).kind;
    if (kind == v12_heap::Kind::Arguments || kind == v12_heap::Kind::Array)
        && let Some(idx) = prop_key_as_index(heap, key)
    {
        if let Some(v) = descriptor.value {
            heap.get_mut(obj).set_element(idx, v);
        }
        return Ok(true);
    }
    let shape = shape_of(heap, obj);
    // Dictionary hit: an existing overflow key updates in place — value
    // and attributes stay per-object; shapes untouched.
    if let Some(entry) = heap
        .get(obj)
        .dictionary
        .as_ref()
        .and_then(|m| m.get(&key))
        .copied()
    {
        if entry.is_accessor {
            // Redefining an accessor via data descriptor is a no-op for
            // v1: preserve accessor shape.
            return Ok(true);
        }
        if let Some(v) = descriptor.value {
            if !entry.attrs.writable() {
                return Ok(false);
            }
            let idx = entry.slot as usize;
            let obj_mut = heap.get_mut(obj);
            if obj_mut.properties.len() <= idx {
                obj_mut.properties.resize(idx + 1, JsValue::hole());
            }
            obj_mut.properties[idx] = v;
        }
        let new_attrs = v12_heap::Attrs::new(
            descriptor.writable,
            descriptor.enumerable,
            descriptor.configurable,
        );
        if new_attrs != entry.attrs
            && let Some(map) = heap.get_mut(obj).dictionary.as_mut()
            && let Some(slot_entry) = map.get_mut(&key)
        {
            slot_entry.attrs = new_attrs;
        }
        return Ok(true);
    }
    if let Some(existing) = heap.get(shape).descriptors.find(key).copied() {
        match existing {
            v12_heap::Descriptor::Data { slot, attrs, .. } => {
                let idx = slot as usize;
                if let Some(v) = descriptor.value {
                    if !attrs.writable() {
                        return Ok(false);
                    }
                    let obj_mut = heap.get_mut(obj);
                    if obj_mut.properties.len() <= idx {
                        obj_mut.properties.resize(idx + 1, JsValue::hole());
                    }
                    obj_mut.properties[idx] = v;
                }
                let new_attrs = v12_heap::Attrs::new(
                    descriptor.writable,
                    descriptor.enumerable,
                    descriptor.configurable,
                );
                if new_attrs != attrs {
                    let next_shape = heap.update_data_attrs(shape, key, new_attrs);
                    bind_shape(heap, obj, next_shape);
                }
                return Ok(true);
            }
            v12_heap::Descriptor::Accessor { .. } => {
                // Redefining an accessor via data descriptor is a no-op for
                // v1: preserve accessor shape.
                return Ok(true);
            }
        }
    }
    if heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
        return Ok(false);
    }
    // Extend: dictionary-rung objects absorb new keys into the map;
    // shape-backed objects spill past the threshold (shapes stop
    // growing); small objects extend the shape exactly as before.
    let value = descriptor.value.unwrap_or(JsValue::undefined());
    let attrs = v12_heap::Attrs::new(
        descriptor.writable,
        descriptor.enumerable,
        descriptor.configurable,
    );
    if heap.get(obj).dictionary.is_some() {
        dict_insert(heap, obj, key, value, attrs);
        return Ok(true);
    }
    if heap.get(shape).num_own as usize >= NAMED_DICT_THRESHOLD {
        convert_to_dictionary(heap, obj);
        dict_insert(heap, obj, key, value, attrs);
        return Ok(true);
    }
    // Extend shape: allocate new shape and publish it onto the object.
    let next_shape = heap.add_property(shape, key, attrs);
    bind_shape(heap, obj, next_shape);
    heap.get_mut(obj).properties.push(value);
    Ok(true)
}

fn prop_key_as_index(heap: &mut Heap, key: PropKey) -> Option<u32> {
    if key.is_symbol() {
        return None;
    }
    let h = key.string()?;
    heap.flatten(h);
    let text: String = match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
        _ => return None,
    };
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.as_bytes()[0] == b'0' {
        return None;
    }
    text.parse::<u32>().ok()
}

/// Dictionary-rung hit for an overflow key: the entry plus its current
/// value (`undefined` when the slot runs past embedder-short storage,
/// mirroring the shape path's OOB fallthrough). `None` in shape-backed
/// mode or for base keys (which stay shape-addressed — the two stores
/// never overlap). Shared by the internal methods and the builtins'
/// shape-walk sites that must see overflow keys.
pub(crate) fn dict_lookup(
    heap: &Heap,
    obj: Handle<JsObject>,
    key: PropKey,
) -> Option<(DictEntry, JsValue)> {
    let entry = heap.get(obj).dictionary.as_ref()?.get(&key).copied()?;
    let value = heap
        .get(obj)
        .properties
        .get(entry.slot as usize)
        .copied()
        .unwrap_or(JsValue::undefined());
    Some((entry, value))
}

/// Spills `obj` to dictionary mode: files an empty dictionary rung and
/// seeds its insertion sequence past the frozen base shape. Base
/// properties stay shape-addressed (their layout is frozen and indexed);
/// only post-spill keys enter the map, so the two stores never overlap.
/// Idempotent.
fn convert_to_dictionary(heap: &mut Heap, obj: Handle<JsObject>) {
    if heap.get(obj).dictionary.is_some() {
        return;
    }
    let base = heap.get(heap.shape_of(obj)).num_own;
    let o = heap.get_mut(obj);
    o.dictionary = Some(Box::new(rustc_hash::FxHashMap::default()));
    o.dict_seq = base;
}

/// Inserts a fresh data property into `obj`'s dictionary rung (the caller
/// guarantees dict mode and a absent key): appends the value to storage
/// and files the entry with the next insertion sequence. O(1).
fn dict_insert(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
    value: JsValue,
    attrs: v12_heap::Attrs,
) {
    debug_assert!(heap.get(obj).dictionary.is_some());
    let (slot, seq) = {
        let o = heap.get(obj);
        (o.properties.len() as u32, o.dict_seq)
    };
    heap.get_mut(obj).properties.push(value);
    let entry = DictEntry {
        slot,
        attrs,
        getter: None,
        setter: None,
        is_accessor: false,
        seq,
    };
    if let Some(map) = heap.get_mut(obj).dictionary.as_mut() {
        map.insert(key, entry);
    }
    heap.get_mut(obj).dict_seq = seq + 1;
}

/// `[[GetOwnProperty]]` report for a dictionary entry, mirroring the
/// shape-descriptor arms exactly (accessors report no value and are
/// never writable through this path).
fn dict_property_descriptor(value: JsValue, entry: DictEntry) -> PropertyDescriptor {
    if entry.is_accessor {
        PropertyDescriptor {
            value: None,
            writable: false,
            enumerable: entry.attrs.enumerable(),
            configurable: entry.attrs.configurable(),
        }
    } else {
        PropertyDescriptor {
            value: Some(value),
            writable: entry.attrs.writable(),
            enumerable: entry.attrs.enumerable(),
            configurable: entry.attrs.configurable(),
        }
    }
}

/// Invokes an accessor getter found on `owner` (shared by the shape walk
/// and the dictionary rung): native/host callables run directly, bytecode
/// bodies report `undefined` (the interpreter's `get_property` provides
/// the eval path), and a missing getter reads `undefined`.
fn invoke_accessor_getter(
    heap: &mut Heap,
    owner: Handle<JsObject>,
    getter: Option<Handle<JsObject>>,
) -> InternalResult<JsValue> {
    if let Some(getter) = getter {
        // The getter is a function object. Native/host handlers can be
        // invoked directly with only the heap; a bytecode getter needs
        // the interpreter, which the engine's `get_property` path
        // provides — this internal-method fallback reports `undefined`
        // for it.
        match heap.get(getter).callable {
            v12_heap::FunctionTarget::Native(f) => {
                return f(heap, JsValue::object(owner), &[]);
            }
            v12_heap::FunctionTarget::Host(c) => {
                return c.call(heap, JsValue::object(owner), &[]);
            }
            v12_heap::FunctionTarget::Bytecode(_)
            | v12_heap::FunctionTarget::RealmEval(_)
            | v12_heap::FunctionTarget::Bound(_) => {
                return Ok(JsValue::undefined());
            }
        }
    }
    Ok(JsValue::undefined())
}

fn ordinary_has_property(
    heap: &mut Heap,
    obj: Handle<JsObject>,
    key: PropKey,
) -> InternalResult<bool> {
    let kind = heap.get(obj).kind;
    if (kind == v12_heap::Kind::Arguments || kind == v12_heap::Kind::Array)
        && let Some(idx) = prop_key_as_index(heap, key)
        && heap.get(obj).get_element(idx).is_some()
    {
        return Ok(true);
    }
    let shape = shape_of(heap, obj);
    // Guarded stub fast path: a recorded `Data` location answers `true`
    // in O(1); anything else (including accessor-held keys, which stubs
    // never record) falls through to the walk below.
    if heap.stub_lookup_proto(obj, shape, key).is_some() {
        return Ok(true);
    }
    let mut cur = Some(obj);
    while let Some(o) = cur {
        let o_shape = shape_of(heap, o);
        // Dictionary rung: overflow keys live only here. Data hits record
        // for future O(1) probes (accessors have no servable slot and
        // always re-walk, but still count as present).
        if let Some(entry) = heap
            .get(o)
            .dictionary
            .as_ref()
            .and_then(|m| m.get(&key))
            .copied()
        {
            if !entry.is_accessor {
                heap.stub_record_proto(obj, shape, key, o, o_shape, entry.slot);
            }
            return Ok(true);
        }
        if let Some(d) = heap.get(o_shape).descriptors.find(key).copied() {
            // Record `Data` locations for future O(1) probes (accessors
            // have no servable slot and always re-walk).
            if let v12_heap::Descriptor::Data { slot, .. } = d {
                heap.stub_record_proto(obj, shape, key, o, o_shape, slot);
            }
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
    let kind = heap.get(obj).kind;
    if (kind == v12_heap::Kind::Arguments || kind == v12_heap::Kind::Array)
        && let Some(idx) = prop_key_as_index(heap, key)
        && let Some(v) = heap.get(obj).get_element(idx)
    {
        return Ok(v);
    }
    let recv_shape = shape_of(heap, obj);
    // Guarded stub fast path (O(1)): serves recorded `Data` locations —
    // own and proto alike. OOB storage falls through to the walk, which
    // resolves exactly as before (including continuing past holes).
    if let Some((holder, hsl)) = heap.stub_lookup_proto(obj, recv_shape, key)
        && let Some(v) = heap.get(holder).properties.get(hsl as usize)
    {
        return Ok(*v);
    }
    let mut cur = Some(obj);
    while let Some(o) = cur {
        let o_shape = shape_of(heap, o);
        // Dictionary rung: overflow keys live only here (disjoint from
        // the frozen base shape). Data hits record like shape hits;
        // accessor hits invoke through the shared helper below.
        if let Some(entry) = heap
            .get(o)
            .dictionary
            .as_ref()
            .and_then(|m| m.get(&key))
            .copied()
        {
            if entry.is_accessor {
                return invoke_accessor_getter(heap, o, entry.getter);
            }
            heap.stub_record_proto(obj, recv_shape, key, o, o_shape, entry.slot);
            if let Some(v) = heap.get(o).properties.get(entry.slot as usize) {
                return Ok(*v);
            }
            // OOB storage (embedder-short object): fall through to the
            // shape walk, mirroring the Data arm below.
        }
        if let Some(d) = heap.get(o_shape).descriptors.find(key).copied() {
            match d {
                v12_heap::Descriptor::Data { slot, .. } => {
                    // Record the location (own or proto, depth ≤ 2) for
                    // future O(1) probes, then serve exactly as before.
                    heap.stub_record_proto(obj, recv_shape, key, o, o_shape, slot);
                    if let Some(v) = heap.get(o).properties.get(slot as usize) {
                        return Ok(*v);
                    }
                }
                v12_heap::Descriptor::Accessor { getter, .. } => {
                    return invoke_accessor_getter(heap, o, getter);
                }
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
    let kind = heap.get(obj).kind;
    if (kind == v12_heap::Kind::Arguments || kind == v12_heap::Kind::Array)
        && let Some(idx) = prop_key_as_index(heap, key)
    {
        heap.get_mut(obj).set_element(idx, value);
        return Ok(true);
    }
    let shape = shape_of(heap, obj);
    // Dictionary rung: overflow keys update in place (mirrors the shape
    // Data/Accessor arms below, including the embedder-short resize).
    if let Some(entry) = heap
        .get(obj)
        .dictionary
        .as_ref()
        .and_then(|m| m.get(&key))
        .copied()
    {
        if entry.is_accessor {
            if entry.setter.is_some() {
                // v1: setter invocation is a no-op beyond acknowledgement.
                return Ok(true);
            }
            return Ok(false);
        }
        if !entry.attrs.writable() {
            return Ok(false);
        }
        let idx = entry.slot as usize;
        let obj_mut = heap.get_mut(obj);
        if obj_mut.properties.len() <= idx {
            obj_mut.properties.resize(idx + 1, JsValue::hole());
        }
        obj_mut.properties[idx] = value;
        return Ok(true);
    }
    if let Some(d) = heap.get(shape).descriptors.find(key).copied() {
        match d {
            v12_heap::Descriptor::Data { slot, attrs, .. } => {
                if !attrs.writable() {
                    return Ok(false);
                }
                heap.get_mut(obj).properties[slot as usize] = value;
                return Ok(true);
            }
            v12_heap::Descriptor::Accessor { setter, .. } => {
                if setter.is_some() {
                    // v1: setter invocation is a no-op beyond acknowledgement;
                    // the interpreter's `set_property` provides the eval path.
                    let _ = value;
                    return Ok(true);
                }
                return Ok(false);
            }
        }
    }
    if let Some(proto_desc) = inherited_descriptor(heap, obj, key) {
        match proto_desc {
            v12_heap::Descriptor::Data { attrs, .. } if !attrs.writable() => return Ok(false),
            v12_heap::Descriptor::Accessor { setter, .. } if setter.is_none() => return Ok(false),
            v12_heap::Descriptor::Accessor { .. } => return Ok(true),
            _ => {}
        }
    }
    if heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
        return Ok(false);
    }
    // Growth past the spill threshold goes to the dictionary rung (the
    // old 1024-throw cliff is gone by design).
    if heap.get(obj).dictionary.is_some() {
        dict_insert(heap, obj, key, value, v12_heap::Attrs::DEFAULT);
        return Ok(true);
    }
    if heap.get(shape).num_own as usize >= NAMED_DICT_THRESHOLD {
        convert_to_dictionary(heap, obj);
        dict_insert(heap, obj, key, value, v12_heap::Attrs::DEFAULT);
        return Ok(true);
    }
    let child = heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    bind_shape(heap, obj, child);
    heap.get_mut(obj).properties.push(value);
    Ok(true)
}

fn ordinary_delete(heap: &mut Heap, obj: Handle<JsObject>, key: PropKey) -> InternalResult<bool> {
    let kind = heap.get(obj).kind;
    if (kind == v12_heap::Kind::Arguments || kind == v12_heap::Kind::Array)
        && let Some(idx) = prop_key_as_index(heap, key)
    {
        heap.get_mut(obj).delete_element(idx);
        return Ok(true);
    }
    // Dictionary rung: remove the entry and hole its storage slot (slot
    // numbering stays stable for live entries, mirroring the shape arm).
    if let Some(entry) = heap
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
        if let Some(map) = heap.get_mut(obj).dictionary.as_mut() {
            map.remove(&key);
        }
        if let Some(v) = heap.get_mut(obj).properties.get_mut(slot) {
            *v = JsValue::hole();
        }
        // Entry removal (unlike shape `delete`, which keeps the
        // descriptor) changes what a walk resolves — bump the proto
        // epoch so guarded stubs re-verify instead of serving the
        // holed slot.
        heap.bump_proto_generation();
        return Ok(true);
    }
    let shape = shape_of(heap, obj);
    let Some(d) = heap.get(shape).descriptors.find(key).copied() else {
        return Ok(true);
    };
    if !d.attrs().configurable() {
        return Ok(false);
    }
    match d {
        v12_heap::Descriptor::Data { slot, .. } => {
            heap.get_mut(obj).properties[slot as usize] = JsValue::hole();
        }
        v12_heap::Descriptor::Accessor { .. } => {
            // Accessor descriptor: no slot to hole; deletion is acknowledged
            // via shape transition pruning (v1 leaves descriptor in place).
        }
    }
    Ok(true)
}

fn ordinary_own_property_keys(heap: &Heap, obj: Handle<JsObject>) -> Vec<PropKey> {
    let shape = shape_of(heap, obj);
    let mut keys: Vec<PropKey> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|d| d.key())
        .collect();
    // Overflow keys append in insertion order (`seq`), matching the base
    // path's insertion-order convention. The two stores never overlap
    // (post-spill keys enter only the map), so nothing double-reports.
    if let Some(map) = heap.get(obj).dictionary.as_ref() {
        let mut overflow: Vec<(u32, PropKey)> = map.iter().map(|(k, e)| (e.seq, *k)).collect();
        overflow.sort_by_key(|&(seq, _)| seq);
        keys.extend(overflow.into_iter().map(|(_, k)| k));
    }
    keys
}

// Proxy traps: stub that throws TypeError for trapped operations.
fn proxy_get_prototype_of(_heap: &Heap, _obj: Handle<JsObject>) -> Option<Handle<JsObject>> {
    // Stub: proxy traps are not fully implemented; returning None would be
    // incorrect for a real proxy. We panic to surface misuse in tests where
    // a proxy is expected to trap, matching the "throw TypeError for trapped
    // ops" requirement via the higher-level wrappers below.
    None
}

fn proxy_is_extensible(_heap: &Heap, _obj: Handle<JsObject>) -> bool {
    true
}

/// Stub trap bodies: every unimplemented proxy trap throws the same
/// "not implemented" TypeError naming its internal-method slot; only the
/// signature varies. Non-throwing stubs (`get_prototype_of`,
/// `is_extensible`, `own_property_keys`) stay hand-written.
macro_rules! proxy_trap_stub {
    ($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty, $trap:literal) => {
        fn $name(heap: &mut Heap, $($arg: $ty),*) -> InternalResult<$ret> {
            $(let _ = $arg;)*
            Err(type_error(
                heap,
                concat!("TypeError: proxy [[", $trap, "]] trap not implemented"),
            ))
        }
    };
}

proxy_trap_stub!(proxy_set_prototype_of(_obj: Handle<JsObject>, _proto: Option<Handle<JsObject>>) -> bool, "SetPrototypeOf");
proxy_trap_stub!(proxy_prevent_extensions(_obj: Handle<JsObject>) -> bool, "PreventExtensions");
proxy_trap_stub!(proxy_get_own_property(_obj: Handle<JsObject>, _key: PropKey) -> Option<PropertyDescriptor>, "GetOwnProperty");
proxy_trap_stub!(proxy_define_own_property(_obj: Handle<JsObject>, _key: PropKey, _descriptor: PropertyDescriptor) -> bool, "DefineOwnProperty");
proxy_trap_stub!(proxy_has_property(_obj: Handle<JsObject>, _key: PropKey) -> bool, "HasProperty");
proxy_trap_stub!(proxy_get(_obj: Handle<JsObject>, _key: PropKey, _receiver: JsValue) -> JsValue, "Get");
proxy_trap_stub!(proxy_set(_obj: Handle<JsObject>, _key: PropKey, _value: JsValue, _receiver: JsValue) -> bool, "Set");
proxy_trap_stub!(proxy_delete(_obj: Handle<JsObject>, _key: PropKey) -> bool, "Delete");

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
    if heap.get(obj).kind == v12_heap::Kind::Proxy {
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
//
// Object → shape binding now lives in `Heap::shape_of` / `Heap::bind_shape`.
// The old `thread_local SHAPE_TABLE` keyed by raw `Heap` pointer (the source
// of the address-reuse correctness bug P1.2) is gone: a fresh heap cannot
// observe another engine's bindings, and the table's lifetime tracks the
// heap's automatically.
// ---------------------------------------------------------------------------

fn shape_of(heap: &Heap, obj: Handle<JsObject>) -> ShapeHandle {
    heap.shape_of(obj)
}

fn shape_of_mut(heap: &mut Heap, obj: Handle<JsObject>) -> ShapeHandle {
    heap.shape_of_mut(obj)
}

fn bind_shape(heap: &mut Heap, obj: Handle<JsObject>, shape: ShapeHandle) {
    heap.bind_shape(obj, shape);
}

/// Public wrapper for `bind_shape` so native handlers can bind shapes.
pub fn bind_shape_public(heap: &mut Heap, obj: Handle<JsObject>, shape: ShapeHandle) {
    bind_shape(heap, obj, shape);
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
    let (kind, msg) = v12_native::parse_error_text(message, "TypeError");
    v12_native::error_object(heap, kind, msg)
}
