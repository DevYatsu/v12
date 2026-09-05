//! Realm and global object for the engine.
//!
//! A realm owns one global object and the intrinsics table. The global
//! object is an ordinary object whose properties host the built-in
//! constructors and prototypes.

use std::collections::HashMap;

use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue, PropKey, V12Str};
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
        let global = heap.alloc(JsObject::default());
        // Root the global so it survives collection before the engine publishes
        // its heap roots.
        heap.add_root(JsValue::object(global));

        let mut intrinsics = HashMap::with_capacity(MAX_INTRINSICS);

        for &name in INTRINSIC_NAMES {
            // `globalThis` is the global object itself per spec; other
            // intrinsics are placeholders (functions except `Math`/`JSON`/`console`
            // which are ordinary objects).
            if name == "globalThis" {
                let key = intern_key(heap, name);
                let _ = key;
                intrinsics.insert(name.to_string(), JsValue::object(global));
                continue;
            }
            let kind = if matches!(name, "Math" | "JSON" | "console") {
                v12_heap::Kind::Ordinary
            } else {
                v12_heap::Kind::Function
            };
            let ctor = heap.alloc(JsObject {
                kind,
                // Placeholder: no real callable yet. `u32::MAX` is beyond any
                // program function count, so calling/constructing it routes to
                // the native seam, which reports "not registered" — the same
                // rejection a pre-FunctionTarget empty `elements[0]` gave.
                callable: v12_heap::FunctionTarget::Bytecode(u32::MAX),
                ..JsObject::default()
            });
            // Publish the placeholder immediately to honor the allocation contract.
            heap.add_root(JsValue::object(ctor));
            let key = intern_key(heap, name);
            let _ = key;
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
            let handle = intern_key(heap, name);
            let _ = handle;
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
        let promise_proto = heap.alloc(JsObject::default());
        heap.add_root(JsValue::object(promise_proto));
        let promise_ctor = intrinsics.get("Promise").and_then(|v| v.as_object());
        if let Some(promise_ctor) = promise_ctor {
            heap.get_mut(promise_ctor).prototype = Some(promise_proto);
        }
        // `String(x)` must be callable (ES ToString): point the placeholder's
        // callable at the native constructor index (out-of-range bytecode →
        // native seam → engine registry dispatches `string_construct`).
        let string_ctor = intrinsics.get("String").and_then(|v| v.as_object());
        if let Some(string_ctor) = string_ctor {
            heap.get_mut(string_ctor).callable =
                v12_heap::FunctionTarget::Bytecode(u32::from(NativeId::StringConstruct));
        }
        // `Error(x)` / `new Error(x)` are constructible: point the placeholder
        // at the native error creator.
        let error_ctor = intrinsics.get("Error").and_then(|v| v.as_object());
        if let Some(error_ctor) = error_ctor {
            heap.get_mut(error_ctor).callable =
                v12_heap::FunctionTarget::Bytecode(u32::from(v12_native::NativeId::ErrorCreate));
        }
        // `Boolean(x)` / `new Boolean(x)` are constructible.
        let boolean_ctor = intrinsics.get("Boolean").and_then(|v| v.as_object());
        if let Some(boolean_ctor) = boolean_ctor {
            heap.get_mut(boolean_ctor).callable = v12_heap::FunctionTarget::Bytecode(u32::from(
                v12_native::NativeId::BooleanConstruct,
            ));
        }
        // `Map` / `Set` are constructible.
        let map_ctor = intrinsics.get("Map").and_then(|v| v.as_object());
        if let Some(map_ctor) = map_ctor {
            heap.get_mut(map_ctor).callable = v12_heap::FunctionTarget::Bytecode(u32::from(
                v12_native::NativeId::MapConstruct,
            ));
        }
        let set_ctor = intrinsics.get("Set").and_then(|v| v.as_object());
        if let Some(set_ctor) = set_ctor {
            heap.get_mut(set_ctor).callable = v12_heap::FunctionTarget::Bytecode(u32::from(
                v12_native::NativeId::SetConstruct,
            ));
        }
        // `RegExp` is constructible.
        let regexp_ctor = intrinsics.get("RegExp").and_then(|v| v.as_object());
        if let Some(regexp_ctor) = regexp_ctor {
            heap.get_mut(regexp_ctor).callable = v12_heap::FunctionTarget::Bytecode(u32::from(
                v12_native::NativeId::RegExpConstruct,
            ));
        }
        // `eval` is callable: route through the native registry (the
        // interpreter special-cases NativeId::Eval to run the source re-entrantly).
        let eval_global = intrinsics.get("eval").and_then(|v| v.as_object());
        if let Some(eval_global) = eval_global {
            heap.get_mut(eval_global).callable =
                v12_heap::FunctionTarget::Bytecode(u32::from(v12_native::NativeId::Eval));
        }

        // Materialize the standard prototypes the built-in installs target.
        // Each is an ordinary object rooted here (like `promise_proto` above);
        // constructors link to them via their `prototype` field.
        let object_proto = {
            let h = heap.alloc(JsObject::default());
            heap.add_root(JsValue::object(h));
            h
        };
        let array_proto = {
            let h = heap.alloc(JsObject::default());
            heap.add_root(JsValue::object(h));
            h
        };
        let string_proto = {
            let h = heap.alloc(JsObject::default());
            heap.add_root(JsValue::object(h));
            h
        };
        let number_proto = {
            let h = heap.alloc(JsObject::default());
            heap.add_root(JsValue::object(h));
            h
        };
        let function_proto = {
            let h = heap.alloc(JsObject::default());
            heap.add_root(JsValue::object(h));
            h
        };

        // Link the intrinsic constructors to their prototypes, mirroring the
        // Promise wiring above, and make `Number(x)` callable.
        let link_ctor = |heap: &mut Heap,
                         intrinsics: &HashMap<String, JsValue>,
                         name: &str,
                         proto: Handle<JsObject>| {
            if let Some(o) = intrinsics.get(name).and_then(|v| v.as_object()) {
                heap.get_mut(o).prototype = Some(proto);
            }
        };
        link_ctor(heap, &intrinsics, "Object", object_proto);
        link_ctor(heap, &intrinsics, "Array", array_proto);
        link_ctor(heap, &intrinsics, "String", string_proto);
        link_ctor(heap, &intrinsics, "Number", number_proto);
        if let Some(number_ctor) = intrinsics.get("Number").and_then(|v| v.as_object()) {
            heap.get_mut(number_ctor).callable = v12_heap::FunctionTarget::Bytecode(u32::from(
                v12_native::NativeId::NumberConstruct,
            ));
        }

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

fn intern_key(heap: &mut Heap, name: &str) -> PropKey {
    let h = if name.is_ascii() {
        heap.intern_string(V12Str::latin1(name.as_bytes().to_vec()))
    } else {
        heap.intern_string(V12Str::utf16(name.encode_utf16().collect()))
    };
    PropKey::from_string(h)
}
