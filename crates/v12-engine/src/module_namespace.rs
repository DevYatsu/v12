//! Module namespace exotic objects and live binding-cell installation.

use v12_bccompiler::ModuleBindingPlan;
use v12_heap::{Attrs, Handle, Heap, JsObject, JsValue, Kind, ModuleNamespaceRecord, PropKey};

/// Allocates an empty namespace object: `[[Prototype]]` null and
/// `[[Extensible]]` false from birth.
pub(crate) fn alloc_namespace(heap: &mut Heap) -> Handle<JsObject> {
    let obj = heap.alloc(JsObject {
        kind: Kind::ModuleNamespace,
        prototype: None,
        flags: JsObject::FLAG_NOT_EXTENSIBLE,
        ..JsObject::default()
    });
    heap.add_module_root(obj);
    obj
}

/// Allocates the namespace, seeded main environment, and one cell per
/// compiler binding-plan entry. Import/re-export cells are uninitialized
/// placeholders until the linker installs the source cell.
pub(crate) fn alloc_linked_module(
    heap: &mut Heap,
    owner_module_id: u32,
    env_slots: u16,
    plan: &[ModuleBindingPlan],
) -> (Handle<JsObject>, Handle<JsObject>) {
    let namespace = alloc_namespace(heap);
    let env = heap.alloc(JsObject::environment(usize::from(env_slots), None));
    heap.add_module_root(env);

    let mut cells = Vec::with_capacity(plan.len());
    let mut exports = Vec::new();
    for binding in plan {
        let cell = heap.alloc(JsObject::binding_cell(
            JsValue::undefined(),
            binding.initially_initialized,
            owner_module_id,
        ));
        heap.add_module_root(cell);
        cells.push((binding.env_slot, cell));
        if usize::from(binding.env_slot) < heap.get(env).properties.len() {
            heap.get_mut(env).properties[usize::from(binding.env_slot)] = JsValue::object(cell);
        }
        for export in &binding.exports {
            exports.push((export.clone(), cell));
        }
    }

    exports.sort_by(|a, b| a.0.cmp(&b.0));
    exports.dedup_by(|a, b| a.0 == b.0);
    let attrs = Attrs::new(true, true, false);
    let mut record_cells = Vec::with_capacity(exports.len());
    for (name, cell) in exports {
        let key = PropKey::from_string(heap.intern_text(&name));
        let shape = heap.shape_of(namespace);
        if let Some(desc) = heap.get(shape).descriptors.find(key).copied() {
            if let Some(slot) = desc.slot() {
                heap.get_mut(namespace).properties[slot as usize] = JsValue::object(cell);
                record_cells.push(cell);
            }
            continue;
        }
        let child = heap.add_property(shape, key, attrs);
        heap.bind_shape(namespace, child);
        heap.get_mut(namespace)
            .properties
            .push(JsValue::object(cell));
        heap.get_mut(namespace).property_keys.push(Some(key));
        record_cells.push(cell);
    }
    heap.get_mut(namespace).module_namespace = Some(ModuleNamespaceRecord {
        owner_module_id,
        cells: record_cells,
    });
    (namespace, env)
}

/// Installs a source cell into an explicit re-export or imported seed slot.
pub(crate) fn set_export_cell(
    heap: &mut Heap,
    namespace: Handle<JsObject>,
    name: &str,
    cell: Handle<JsObject>,
) {
    let key = PropKey::from_string(heap.intern_text(name));
    let shape = heap.shape_of(namespace);
    let Some(desc) = heap.get(shape).descriptors.find(key).copied() else {
        return;
    };
    if let Some(slot) = desc.slot() {
        heap.get_mut(namespace).properties[slot as usize] = JsValue::object(cell);
    }
}

/// Installs a cell for an export name, adding the compatibility star alias
/// when the name was not part of the initial local export plan.
pub(crate) fn set_or_add_export_cell(
    heap: &mut Heap,
    namespace: Handle<JsObject>,
    name: &str,
    cell: Handle<JsObject>,
) {
    let key = PropKey::from_string(heap.intern_text(name));
    let shape = heap.shape_of(namespace);
    if let Some(desc) = heap.get(shape).descriptors.find(key).copied()
        && let Some(slot) = desc.slot()
    {
        heap.get_mut(namespace).properties[slot as usize] = JsValue::object(cell);
        return;
    }
    let child = heap.add_property(shape, key, Attrs::new(true, true, false));
    heap.bind_shape(namespace, child);
    heap.get_mut(namespace)
        .properties
        .push(JsValue::object(cell));
    heap.get_mut(namespace).property_keys.push(Some(key));
}

/// Rebuilds export slots in string-key order after compatibility star aliases
/// are installed. This keeps the namespace's sorted-key invariant without
/// attempting full star-export resolution.
pub(crate) fn sort_exports(heap: &mut Heap, namespace: Handle<JsObject>) {
    let shape = heap.shape_of(namespace);
    let mut entries: Vec<(String, PropKey, JsValue)> = Vec::new();
    let descriptors: Vec<(PropKey, Option<u32>)> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|descriptor| (descriptor.key(), descriptor.slot()))
        .collect();
    for (key, slot) in descriptors {
        let Some(slot) = slot else {
            continue;
        };
        let Some(handle) = key.string() else {
            continue;
        };
        heap.flatten(handle);
        let text = match &heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
            _ => continue,
        };
        let value = heap
            .get(namespace)
            .properties
            .get(slot as usize)
            .copied()
            .unwrap_or_else(JsValue::undefined);
        entries.push((text, key, value));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup_by(|a, b| a.1 == b.1);

    let owner_module_id = heap
        .get(namespace)
        .module_namespace
        .as_ref()
        .map_or(0, |record| record.owner_module_id);
    heap.bind_shape(namespace, heap.root_shape());
    let object = heap.get_mut(namespace);
    object.properties.clear();
    object.property_keys.clear();
    object.module_namespace = None;

    let mut cells = Vec::with_capacity(entries.len());
    for (_, key, value) in entries {
        let shape = heap.shape_of(namespace);
        let child = heap.add_property(shape, key, Attrs::new(true, true, false));
        heap.bind_shape(namespace, child);
        heap.get_mut(namespace).properties.push(value);
        heap.get_mut(namespace).property_keys.push(Some(key));
        if let Some(cell) = value.as_object() {
            cells.push(cell);
        }
    }
    heap.get_mut(namespace).module_namespace = Some(ModuleNamespaceRecord {
        owner_module_id,
        cells,
    });
}

/// Export names currently linked on a namespace, used only for compatibility
/// validation of `export *` ambiguity.
pub(crate) fn export_names(heap: &mut Heap, namespace: Handle<JsObject>) -> Vec<String> {
    let shape = heap.shape_of(namespace);
    let descriptors: Vec<PropKey> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .map(|descriptor| descriptor.key())
        .collect();
    descriptors
        .into_iter()
        .filter_map(|key| {
            let handle = key.string()?;
            heap.flatten(handle);
            match &heap.get(handle).storage {
                v12_heap::StrStorage::Latin1(bytes) => {
                    Some(String::from_utf8_lossy(bytes).into_owned())
                }
                v12_heap::StrStorage::Utf16(units) => Some(String::from_utf16_lossy(units)),
                _ => None,
            }
        })
        .collect()
}

/// Raw cell handle for an export name, used only by the loader's linker.
pub(crate) fn export_cell(
    heap: &mut Heap,
    namespace: Handle<JsObject>,
    name: &str,
) -> Option<Handle<JsObject>> {
    let key = PropKey::from_string(heap.intern_text(name));
    let shape = heap.shape_of(namespace);
    let slot = heap.lookup_property(shape, key)?.slot()?;
    heap.get(namespace)
        .properties
        .get(slot as usize)?
        .as_object()
}

/// Linker helper for a module-main import/re-export plan slot.
pub(crate) fn set_env_cell(
    heap: &mut Heap,
    env: Handle<JsObject>,
    slot: u16,
    cell: Handle<JsObject>,
) {
    if let Some(value) = heap.get_mut(env).properties.get_mut(usize::from(slot)) {
        *value = JsValue::object(cell);
    }
}

/// Linker helper for `import * as ns`; the seeded cell holds the namespace
/// identity as its value.
pub(crate) fn set_env_value(heap: &mut Heap, env: Handle<JsObject>, slot: u16, value: JsValue) {
    let Some(stored) = heap
        .get(env)
        .properties
        .get(usize::from(slot))
        .copied()
        .and_then(|v| v.as_object())
    else {
        return;
    };
    if heap.get(stored).kind == Kind::BindingCell
        && let Some(binding) = heap.get_mut(stored).binding_cell.as_mut()
    {
        binding.value = value;
        binding.initialized = true;
    }
}
