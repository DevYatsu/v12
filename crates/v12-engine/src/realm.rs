//! Realm and global object for the engine.
//!
//! A realm owns one global object and the intrinsics table. The global
//! object is an ordinary object whose properties host the built-in
//! constructors and prototypes.

use std::collections::HashMap;

use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue, PropKey, V12Str};

/// Maximum number of intrinsics a realm may host.
const MAX_INTRINSICS: usize = 64;

/// Names of the standard intrinsics installed at realm creation.
const INTRINSIC_NAMES: &[&str] = &[
    "Object",
    "Array",
    "String",
    "Number",
    "Math",
    "Boolean",
    "Error",
    "TypeError",
    "RangeError",
    "Promise",
];

/// A single realm: global object plus the intrinsic registry.
#[derive(Debug)]
pub struct Realm {
    global: Handle<JsObject>,
    intrinsics: HashMap<String, JsValue>,
}

impl Realm {
    /// Creates a new realm, allocating its global object in `heap` and
    /// populating the intrinsic table with placeholder objects.
    pub fn new(heap: &mut Heap) -> Self {
        let global = heap.alloc(JsObject::default());
        // Root the global so it survives collection before the engine publishes
        // its heap roots.
        heap.add_root(JsValue::object(global));

        let mut intrinsics = HashMap::with_capacity(MAX_INTRINSICS);

        for &name in INTRINSIC_NAMES {
            let ctor = heap.alloc(JsObject::default());
            // Publish the placeholder immediately to honor the allocation contract.
            heap.add_root(JsValue::object(ctor));
            let key = intern_key(heap, name);
            let _ = key;
            intrinsics.insert(name.to_string(), JsValue::object(ctor));
        }

        // Install intrinsics as properties of the global object. Each property
        // gets a distinct shape transition; the root shape is pinned by the heap.
        // To keep the implementation simple and avoid external shape bookkeeping,
        // the installation uses direct property storage on the global's vector
        // in insertion order, which is sufficient for the skeleton stage.
        // The shape-aware path is exercised in the built-ins stage via the
        // interpreter's `Heap::add_property` mirroring.
        for (name, value) in &intrinsics {
            let handle = intern_key(heap, name);
            let _ = handle;
            // Push property value; shape tracking is handled lazily in the
            // interpreter layer where shapes are published per-store.
            heap.get_mut(global).properties.push(*value);
        }

        Self { global, intrinsics }
    }

    /// Handle to the realm's global object.
    #[must_use]
    pub fn global(&self) -> Handle<JsObject> {
        self.global
    }

    /// Intrinsic table for inspection and native registry wiring.
    #[must_use]
    pub fn intrinsics(&self) -> &HashMap<String, JsValue> {
        &self.intrinsics
    }

    /// Looks up an intrinsic by name.
    #[must_use]
    pub fn get_intrinsic(&self, name: &str) -> Option<JsValue> {
        self.intrinsics.get(name).copied()
    }
}

impl Default for Realm {
    fn default() -> Self {
        let mut heap = Heap::new(GcPolicy::NoGC);
        Self::new(&mut heap)
    }
}

fn intern_key(heap: &mut Heap, name: &str) -> PropKey {
    let h = if name.is_ascii() {
        heap.intern_string(V12Str::latin1(name.as_bytes().to_vec()))
    } else {
        heap.intern_string(V12Str::utf16(name.encode_utf16().collect()))
    };
    PropKey::from_string(h)
}
