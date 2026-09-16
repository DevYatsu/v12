//! Realm and global object for the engine.
//!
//! A realm owns one global object and the intrinsics table. The global
//! object is an ordinary object whose properties host the built-in
//! constructors and prototypes.

use std::collections::HashMap;

use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue};
use v12_heap::FunctionTarget;
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
        // Publish this global on the heap's realm registry: multiple realms
        // share one heap, and the interpreter consults the registry to serve
        // intrinsic-prefix reads and biased var slots on non-primary globals
        // (cross-realm `$262.createRealm().global` access).
        heap.register_realm_global(global);

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
        // Enforcement (plan §3.4): global `properties[0..INTRINSIC_COUNT]`
        // order == `GLOBAL_INTRINSICS` order (the `GLOBAL_VAR_OFFSET`
        // slot contract). Complements the interpreter-side
        // `intrinsic_slot_guard`.
        for (i, &name) in INTRINSIC_NAMES.iter().enumerate() {
            debug_assert_eq!(
                heap.get(global).properties.get(i).copied(),
                intrinsics.get(name).copied(),
                "intrinsic push order must match GLOBAL_INTRINSICS"
            );
        }
        debug_assert_eq!(heap.get(global).properties.len(), INTRINSIC_COUNT);

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
            // Unified install family: field link + spec-attr `prototype` prop
            // + `constructor` back-link (plan §3). The back-link is new
            // observable surface (`Promise.prototype.constructor === Promise`,
            // spec-mandated); the intrinsic order above is untouched.
            crate::builtins::install_ctor(heap, promise_ctor, promise_proto);
        }
        // The Promise constructor itself: `new Promise(executor)` routes to
        // the stateful native seam (the capability needs the job sink).
        wire_callable(heap, &intrinsics, "Promise", NativeId::PromiseConstruct);
        // Error class prototypes: `Error.prototype` carries `name: "Error"`
        // (and the spec's own `message: ""`); each subclass prototype chains
        // to it and carries its class `name`. Every error instance (user
        // constructed or internally thrown) links to its class prototype via
        // the constructor's `prototype` field, so `instanceof` and `name`
        // reads resolve per spec.
        let error_proto = alloc_root(heap);
        let error_name_h = heap.intern_text("Error");
        crate::builtins::builtin_install_prop(heap, error_proto, "name", JsValue::string(error_name_h));
        let empty_msg_h = heap.intern_text("");
        crate::builtins::builtin_install_prop(heap, error_proto, "message", JsValue::string(empty_msg_h));
        if let Some(e) = intrinsics.get("Error").and_then(|v| v.as_object()) {
            crate::builtins::install_ctor(heap, e, error_proto);
        }
        wire_callable(heap, &intrinsics, "Error", NativeId::ErrorCreate);
        for (name, native) in [
            ("TypeError", NativeId::TypeErrorCreate),
            ("RangeError", NativeId::RangeErrorCreate),
            ("ReferenceError", NativeId::ReferenceErrorCreate),
            ("SyntaxError", NativeId::SyntaxErrorCreate),
        ] {
            let Some(ctor) = intrinsics.get(name).and_then(|v| v.as_object()) else {
                continue;
            };
            let proto = alloc_root(heap);
            heap.get_mut(proto).prototype = Some(error_proto);
            let name_h = heap.intern_text(name);
            crate::builtins::builtin_install_prop(heap, proto, "name", JsValue::string(name_h));
            crate::builtins::install_ctor(heap, ctor, proto);
            wire_callable(heap, &intrinsics, name, native);
        }
        // Point the placeholder constructors that are already callable at
        // their native seam (out-of-range bytecode → native registry).
        wire_callable(heap, &intrinsics, "Object", NativeId::ObjectConstruct);
        wire_callable(heap, &intrinsics, "Array", NativeId::ArrayConstruct);
        wire_callable(heap, &intrinsics, "String", NativeId::StringConstruct);
        wire_callable(heap, &intrinsics, "Boolean", NativeId::BooleanConstruct);
        wire_callable(heap, &intrinsics, "Map", NativeId::MapConstruct);
        wire_callable(heap, &intrinsics, "Set", NativeId::SetConstruct);
        wire_callable(heap, &intrinsics, "Symbol", NativeId::SymbolConstruct);
        wire_callable(heap, &intrinsics, "RegExp", NativeId::RegExpConstruct);
        // `Proxy`: the constructor allocates a proxy exotic object (target
        // validation + slots; trap dispatch is a later phase).
        //
        // Proxy intentionally has NO `.prototype` (test262
        // `built-ins/Proxy/proxy-no-prototype.js`): proxy exotic objects have
        // no `[[Prototype]]` slot that construction initializes, so the spec
        // gives `%Proxy%` no `prototype` property. `install_ctor` and
        // `install_ctor_link` are therefore NOT called for Proxy, and no
        // `proxy_proto` object is allocated. The `prototype` field on the
        // placeholder function object stays `None`, which the interpreter's
        // `prepare_construct` path tolerates (it is a native, so it never
        // allocates an instance).
        wire_callable(heap, &intrinsics, "Proxy", NativeId::ProxyConstruct);
        // `Proxy.length` is 2 and `Proxy.name` is `"Proxy"` (ES `CreateBuiltinFunction`).
        // The intrinsic placeholders carry neither property (no other intrinsic
        // ctor does yet — `Array.length` reads `undefined` today), and the
        // interpreter has no name surface for them, so stamp both here with the
        // spec attrs `{ writable: false, enumerable: false, configurable: true }`
        // in `length`-then-`name` order (test262 `built-ins/Proxy/length.js`,
        // `name.js`).
        if let Some(proxy_ctor) = intrinsics.get("Proxy").and_then(|v| v.as_object()) {
            // Intern everything the ctx needs before constructing it (the ctx
            // holds the only `&mut Heap` borrow).
            let proxy_name = heap.intern_text("Proxy");
            let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
            ctx.define_data_prop_with_attrs(
                proxy_ctor,
                "length",
                JsValue::from_i32_smi(2).expect("2 fits Smi"),
                v12_heap::Attrs::new(false, false, true),
            );
            ctx.define_data_prop_with_attrs(
                proxy_ctor,
                "name",
                JsValue::string(proxy_name),
                v12_heap::Attrs::new(false, false, true),
            );
        }
        // `eval` is realm-bound: a `RealmEval` function carrying THIS realm's
        // global, so eval'd code — direct or detached like
        // `$262.createRealm().global.eval` — executes against this realm's
        // global object and intrinsic slots. (The `NativeId::Eval` registry
        // seam remains as the fallback for embedder globals without realms.)
        let eval_idx = INTRINSIC_NAMES
            .iter()
            .position(|&n| n == "eval")
            .expect("eval intrinsic present");
        let eval_fn = crate::builtins::helpers::alloc_obj(
            heap,
            JsObject::function(v12_heap::FunctionTarget::RealmEval(global), None),
        );
        heap.get_mut(global).properties[eval_idx] = JsValue::object(eval_fn);
        if let Some(slot) = intrinsics.get_mut("eval") {
            *slot = JsValue::object(eval_fn);
        }
        // `Number(x)` is callable too (wired below with the prototype links).

        // Materialize the standard prototypes the built-in installs target.
        // Each is an ordinary object rooted here (like `promise_proto` above);
        // constructors link to them via their `prototype` field.
        let object_proto = alloc_root(heap);
        let array_proto = alloc_root(heap);
        let string_proto = alloc_root(heap);
        let number_proto = alloc_root(heap);
        // `Function.prototype` is itself a callable function object (spec:
        // `%Function.prototype%` is a built-in function). Its callable is the
        // native-seam placeholder `u32::MAX` — calling it does nothing and
        // returns `undefined`. Allocating it as `Kind::Function` is what makes
        // the interpreter's `function_method_surface` (gated on that kind)
        // serve `call`/`apply`/`bind`/`toString` for it.
        let function_proto = crate::builtins::helpers::alloc_obj(
            heap,
            JsObject::function(FunctionTarget::Bytecode(u32::MAX), None),
        );
        let boolean_proto = alloc_root(heap);
        let symbol_proto = alloc_root(heap);

        // Link the intrinsic constructors to their prototypes through the
        // unified install family (`install_ctor`: `prototype` field +
        // spec-attr `prototype` prop + `constructor` back-link). This retires
        // the per-site `builtin_install_prop("prototype", …)` installs: every
        // ctor/prototype pair funnels through one helper. `Constructor.prototype`
        // stays a readable property (code like `Array.prototype.map.call(...)`
        // reads it); the field alone is invisible to property lookups.
        for (name, proto) in [
            ("Object", object_proto),
            ("Array", array_proto),
            ("String", string_proto),
            ("Number", number_proto),
            ("Boolean", boolean_proto),
            ("Symbol", symbol_proto),
        ] {
            if let Some(o) = intrinsics.get(name).and_then(|v| v.as_object()) {
                crate::builtins::install_ctor(heap, o, proto);
            }
        }
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
            string: intrinsics.get("String").and_then(|v| v.as_object()),
            string_proto,
            array: intrinsics.get("Array").and_then(|v| v.as_object()),
            array_proto,
            object: intrinsics.get("Object").and_then(|v| v.as_object()),
            object_proto,
            function_proto,
            json: intrinsics.get("JSON").and_then(|v| v.as_object()),
            boolean_proto,
            symbol: intrinsics.get("Symbol").and_then(|v| v.as_object()),
            symbol_proto,
            proxy: intrinsics.get("Proxy").and_then(|v| v.as_object()),
        };
        crate::builtins::install_builtins(heap, &targets);

        // The `Function` constructor: not a `GLOBAL_INTRINSICS` slot (so the
        // compiler still refuses a bare `Function` identifier), but installed
        // as an ordinary global property so `globalThis.Function` and member
        // reads resolve. The interpreter intercepts `NativeId::Function` and
        // routes it through the registry's `function_construct` seam, which
        // compiles a real program (see `builtins/registry.rs`).
        // Capture the installed handle to link it to the callable
        // `Function.prototype` allocated above: field link + spec-attr
        // `prototype` property + `constructor` back-link (so
        // `Function.prototype.constructor === Function`).
        if let Some(ctor) =
            crate::builtins::install_native(heap, Some(global), "Function", NativeId::Function)
        {
            crate::builtins::install_ctor(heap, ctor, function_proto);
        }
        // The derived constructors (`AsyncFunction`, `GeneratorFunction`,
        // `AsyncGeneratorFunction`): minimal natives sharing the `Function`
        // seam. The install stamps the correct `name`, and calls/constructs
        // route to `function_construct`, so hashbang bodies reject with a
        // real SyntaxError exactly like `Function`. They exist so
        // `(async function(){}).constructor`-style reads and `ctor.name`
        // resolve instead of throwing on `undefined`.
        for name in [
            "AsyncFunction",
            "GeneratorFunction",
            "AsyncGeneratorFunction",
        ] {
            crate::builtins::install_native(heap, Some(global), name, NativeId::Function);
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

/// Builds the `$262`-shaped object for a freshly created realm: a new
/// [`Realm`] allocated in `heap` (the same heap as the creating realm — all
/// values are shared, so cross-realm property access and identity checks
/// work through the ordinary object machinery) plus the host methods the
/// Test262 agent API exposes:
///
/// - `global` — the new realm's global object itself;
/// - `eval(source)` — compiles and runs `source` bound to the new realm's
///   global (a `FunctionTarget::RealmEval` function; the interpreter routes
///   it through the eval seam so its program registers in the caller's
///   cross-program table);
/// - `createRealm()` — recursive, same builder;
/// - `destroy`/`gc`/`getReport`/`detachArrayBuffer` — single-realm no-ops.
///
/// Every allocated object is rooted, so the realm outlives the call.
#[must_use]
pub fn build_realm_object(heap: &mut Heap) -> Handle<JsObject> {
    let realm = Realm::new(heap);
    let global = realm.global();

    let obj = crate::builtins::helpers::alloc_obj(heap, JsObject::default());
    let eval_fn = crate::builtins::helpers::alloc_obj(
        heap,
        JsObject::function(v12_heap::FunctionTarget::RealmEval(global), None),
    );
    let create_realm_fn = crate::builtins::helpers::alloc_obj(
        heap,
        JsObject::function(
            v12_heap::FunctionTarget::Host(v12_heap::HostClosure::new(|heap, _this, _args| {
                Ok(JsValue::object(build_realm_object(heap)))
            })),
            None,
        ),
    );
    let destroy_fn = crate::builtins::helpers::alloc_obj(
        heap,
        JsObject::function(
            v12_heap::FunctionTarget::Host(v12_heap::HostClosure::new(|_heap, _this, _args| {
                Ok(JsValue::undefined())
            })),
            None,
        ),
    );
    let detach_fn = crate::builtins::helpers::alloc_obj(
        heap,
        JsObject::function(
            v12_heap::FunctionTarget::Host(v12_heap::HostClosure::new(|_heap, _this, args| {
                Ok(args.first().copied().unwrap_or_else(JsValue::undefined))
            })),
            None,
        ),
    );
    let gc_fn = crate::builtins::helpers::alloc_obj(
        heap,
        JsObject::function(
            v12_heap::FunctionTarget::Host(v12_heap::HostClosure::new(|_heap, _this, _args| {
                Ok(JsValue::undefined())
            })),
            None,
        ),
    );
    let get_report_fn = crate::builtins::helpers::alloc_obj(
        heap,
        JsObject::function(
            v12_heap::FunctionTarget::Host(v12_heap::HostClosure::new(|_heap, _this, _args| {
                Ok(JsValue::null())
            })),
            None,
        ),
    );

    crate::builtins::builtin_install_prop(heap, obj, "global", JsValue::object(global));
    crate::builtins::builtin_install_prop(heap, obj, "eval", JsValue::object(eval_fn));
    crate::builtins::builtin_install_prop(
        heap,
        obj,
        "createRealm",
        JsValue::object(create_realm_fn),
    );
    crate::builtins::builtin_install_prop(heap, obj, "destroy", JsValue::object(destroy_fn));
    crate::builtins::builtin_install_prop(
        heap,
        obj,
        "detachArrayBuffer",
        JsValue::object(detach_fn),
    );
    crate::builtins::builtin_install_prop(heap, obj, "gc", JsValue::object(gc_fn));
    crate::builtins::builtin_install_prop(heap, obj, "getReport", JsValue::object(get_report_fn));

    obj
}

/// Links an intrinsic constructor's `prototype` field to `proto`.
#[allow(dead_code)]
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
