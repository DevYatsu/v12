//! Module namespace exotic objects and live binding-cell installation.

use v12_bccompiler::ModuleBindingPlan;
use v12_heap::{
    Attrs, Handle, Heap, JsObject, JsValue, Kind, ModuleNamespaceRecord, PropKey,
};

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
        heap.get_mut(namespace).properties.push(JsValue::object(cell));
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

/// Raw cell handle for an export name, used only by the loader's linker.
pub(crate) fn export_cell(
    heap: &mut Heap,
    namespace: Handle<JsObject>,
    name: &str,
) -> Option<Handle<JsObject>> {
    let key = PropKey::from_string(heap.intern_text(name));
    let shape = heap.shape_of(namespace);
    let slot = heap.lookup_property(shape, key)?.slot()?;
    heap.get(namespace).properties.get(slot as usize)?.as_object()
}

/// Linker helper for a module-main import/re-export plan slot.
pub(crate) fn set_env_cell(
    heap: &mut Heap,
    env: Handle<JsObject>,
    slot: u16,
    cell: Handle<JsObject>,
) {
    if let Some(value) = heap
        .get_mut(env)
        .properties
        .get_mut(usize::from(slot))
    {
        *value = JsValue::object(cell);
    }
}

/// Linker helper for `import * as ns`; the seeded cell holds the namespace
/// identity as its value.
pub(crate) fn set_env_value(
    heap: &mut Heap,
    env: Handle<JsObject>,
    slot: u16,
    value: JsValue,
) {
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
