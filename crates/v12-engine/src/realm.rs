//! Realm and global object for the engine.
//!
//! A realm owns one global object and the intrinsics table. The global
//! object is an ordinary object whose properties host the built-in
//! constructors and prototypes.

use std::collections::HashMap;

use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue};
use v12_native::NativeId;

/// Maximum number of intrinsics a realm may host.
const MAX_INTRINSICS: usize = 64;

// Names of the standard intrinsics installed at realm creation. Canonical
// copy lives in `v12-bytecode` (`GLOBAL_INTRINSICS`); the realm and the
// interpreter both read it from there, so the `GLOBAL_VAR_OFFSET` slot-order
// contract is enforced by sharing rather than by hand-synced duplicates.
use v12_bytecode::GLOBAL_INTRINSICS as INTRINSIC_NAMES;

/// `INTRINSIC_NAMES.len()` — the engine-side name for
/// [`v12_bytecode::GLOBAL_VAR_OFFSET`].
pub const INTRINSIC_COUNT: usize = v12_bytecode::GLOBAL_VAR_OFFSET;

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
        // Rooted immediately: the global must survive collection before the
        // engine publishes its heap roots.
        let global = alloc_root(heap);

        let mut intrinsics = HashMap::with_capacity(MAX_INTRINSICS);

        for &name in INTRINSIC_NAMES {
            // `globalThis` is the global object itself per spec; other
            // intrinsics are placeholders (functions except `Math`/`JSON`/`console`
            // which are ordinary objects).
            if name == "globalThis" {
                intrinsics.insert(name.to_string(), JsValue::object(global));
                continue;
            }
            let kind = if matches!(name, "Math" | "JSON" | "console") {
                v12_heap::Kind::Ordinary
            } else {
                v12_heap::Kind::Function
            };
            // Placeholder: no real callable yet. `u32::MAX` is beyond any
            // program function count, so calling/constructing it routes to
            // the native seam, which reports "not registered" — the same
            // rejection a pre-FunctionTarget empty `elements[0]` gave.
            let ctor = crate::builtins::helpers::alloc_obj(
                heap,
                JsObject {
                    kind,
                    callable: v12_heap::FunctionTarget::Bytecode(u32::MAX),
                    ..JsObject::default()
                },
            );
            intrinsics.insert(name.to_string(), JsValue::object(ctor));
        }

        // Install intrinsics as properties of the global object in the
        // deterministic order of `INTRINSIC_NAMES`. The interpreter's fast
        // path for `GetGlobal` indexes `global.properties` directly by the
        // position in its own `INTRINSICS` table, so the two tables must stay
        // in sync (see `GLOBAL_VAR_OFFSET` in `v12-interp`). Shape tracking
        // is handled lazily in the interpreter; here we only need the vector
        // in the correct order.
        for &name in INTRINSIC_NAMES {
            let value = intrinsics
                .get(name)
                .copied()
                .expect("intrinsic must have been inserted");
            heap.get_mut(global).properties.push(value);
        }

        // Minimal Promise wiring: the Promise constructor's `prototype` link
        // hosts `Promise.prototype` (an ordinary object). Promise instances
        // created by the built-ins link to it, and the interpreter's
        // `get_property` fast path serves `then` on objects recognized by
        // that prototype identity (natives cannot attach shape-bound
        // properties). The intrinsic order above is untouched — only the
        // placeholder's prototype field is filled — preserving the
        // `GLOBAL_VAR_OFFSET` contract.
        let promise_proto = alloc_root(heap);
        let promise_ctor = intrinsics.get("Promise").and_then(|v| v.as_object());
        if let Some(promise_ctor) = promise_ctor {
            heap.get_mut(promise_ctor).prototype = Some(promise_proto);
        }
        // Point the placeholder constructors that are already callable at
        // their native seam (out-of-range bytecode → native registry).
        wire_callable(heap, &intrinsics, "String", NativeId::StringConstruct);
        wire_callable(heap, &intrinsics, "Error", NativeId::ErrorCreate);
        wire_callable(heap, &intrinsics, "Boolean", NativeId::BooleanConstruct);
        wire_callable(heap, &intrinsics, "Map", NativeId::MapConstruct);
        wire_callable(heap, &intrinsics, "Set", NativeId::SetConstruct);
        wire_callable(heap, &intrinsics, "RegExp", NativeId::RegExpConstruct);
        // `eval` routes through the native registry (the interpreter
        // special-cases NativeId::Eval to run the source re-entrantly).
        wire_callable(heap, &intrinsics, "eval", NativeId::Eval);
        // `Number(x)` is callable too (wired below with the prototype links).

        // Materialize the standard prototypes the built-in installs target.
        // Each is an ordinary object rooted here (like `promise_proto` above);
        // constructors link to them via their `prototype` field.
        let object_proto = alloc_root(heap);
        let array_proto = alloc_root(heap);
        let string_proto = alloc_root(heap);
        let number_proto = alloc_root(heap);
        let function_proto = alloc_root(heap);

        // Link the intrinsic constructors to their prototypes and make
        // `Number(x)` callable.
        wire_prototype(heap, &intrinsics, "Object", object_proto);
        wire_prototype(heap, &intrinsics, "Array", array_proto);
        wire_prototype(heap, &intrinsics, "String", string_proto);
        wire_prototype(heap, &intrinsics, "Number", number_proto);
        wire_callable(heap, &intrinsics, "Number", NativeId::NumberConstruct);

        // Install the compile-time builtin table (isNaN, Math.floor, Array.push,
        // …) as shape-bound properties on the global and the constructors/
        // prototypes. Must run after the 18 intrinsic slots are pushed so the
        // global's shape slot `n` maps to `properties[GLOBAL_VAR_OFFSET + n]`.
        let targets = crate::builtins::BuiltinTargets {
            global,
            math: intrinsics.get("Math").and_then(|v| v.as_object()),
            number: intrinsics.get("Number").and_then(|v| v.as_object()),
            number_proto,
            string_proto,
            array: intrinsics.get("Array").and_then(|v| v.as_object()),
            array_proto,
            object: intrinsics.get("Object").and_then(|v| v.as_object()),
            object_proto,
            function_proto,
        };
        crate::builtins::install_builtins(heap, &targets);

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

/// Allocates an ordinary object and roots it so it survives collection until
/// the engine publishes its heap roots (same contract as the natives'
/// `helpers::alloc_obj`).
fn alloc_root(heap: &mut Heap) -> Handle<JsObject> {
    crate::builtins::helpers::alloc_obj(heap, JsObject::default())
}

/// Points an intrinsic constructor's placeholder callable at a native:
/// out-of-range bytecode routes to the native seam, which dispatches by
/// `native`. A missing intrinsic is silently skipped (optional constructors).
fn wire_callable(heap: &mut Heap, intrinsics: &HashMap<String, JsValue>, name: &str, native: NativeId) {
    if let Some(o) = intrinsics.get(name).and_then(|v| v.as_object()) {
        heap.get_mut(o).callable = v12_heap::FunctionTarget::Bytecode(u32::from(native));
    }
}

/// Links an intrinsic constructor's `prototype` field to `proto`.
fn wire_prototype(
    heap: &mut Heap,
    intrinsics: &HashMap<String, JsValue>,
    name: &str,
    proto: Handle<JsObject>,
) {
    if let Some(o) = intrinsics.get(name).and_then(|v| v.as_object()) {
        heap.get_mut(o).prototype = Some(proto);
    }
}
