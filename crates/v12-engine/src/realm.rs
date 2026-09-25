//! Realm and global object for the engine.
//!
//! A realm owns one global object and the intrinsics table. The global
//! object is an ordinary object whose properties host the built-in
//! constructors and prototypes.

use std::collections::HashMap;

use v12_heap::FunctionTarget;
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
    symbol_iterator: Handle<v12_heap::V12Symbol>,
    symbol_to_string_tag: Handle<v12_heap::V12Symbol>,
    symbol_species: Handle<v12_heap::V12Symbol>,
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
        // The prototype method set (`then`/`catch`/`finally`) and the
        // statics (`all`/`race`/`resolve`/`reject`) install further down,
        // next to the canonical well-known symbols: `Symbol.species` is only
        // canonical once this realm's symbols exist.
        // Error class prototypes: `Error.prototype` carries `name: "Error"`
        // (and the spec's own `message: ""`); each subclass prototype chains
        // to it and carries its class `name`. Every error instance (user
        // constructed or internally thrown) links to its class prototype via
        // the constructor's `prototype` field, so `instanceof` and `name`
        // reads resolve per spec.
        let error_proto = alloc_root(heap);
        let error_name_h = heap.intern_text("Error");
        crate::builtins::builtin_install_prop(
            heap,
            error_proto,
            "name",
            JsValue::string(error_name_h),
        );
        let empty_msg_h = heap.intern_text("");
        crate::builtins::builtin_install_prop(
            heap,
            error_proto,
            "message",
            JsValue::string(empty_msg_h),
        );
        if let Some(e) = intrinsics.get("Error").and_then(|v| v.as_object()) {
            crate::builtins::install_ctor(heap, e, error_proto);
        }
        crate::builtins::install_native(
            heap,
            Some(error_proto),
            "toString",
            NativeId::ErrorProtoToString,
        );
        wire_callable(heap, &intrinsics, "Error", NativeId::ErrorCreate);
        // `Error.isError` static (ES2026): shape-bound install on the Error
        // constructor (found by ordinary property lookup — no interpreter
        // surface change needed).
        if let Some(e) = intrinsics.get("Error").and_then(|v| v.as_object()) {
            crate::builtins::install_native(heap, Some(e), "isError", NativeId::ErrorIsError);
        }
        for (name, native) in [
            ("TypeError", NativeId::TypeErrorCreate),
            ("RangeError", NativeId::RangeErrorCreate),
            ("ReferenceError", NativeId::ReferenceErrorCreate),
            ("SyntaxError", NativeId::SyntaxErrorCreate),
            ("EvalError", NativeId::EvalErrorCreate),
            ("URIError", NativeId::UriErrorCreate),
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
        // `Symbol.species` is the third canonical well-known symbol this
        // realm needs (the macro's Symbol group installed a *handler
        // function* for it; the Promise surface below replaces that slot
        // with the real symbol, exactly as `toPrimitive` does for Date).
        let (symbol_iterator, symbol_to_string_tag, symbol_species) = {
            let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
            (ctx.fresh_symbol(), ctx.fresh_symbol(), ctx.fresh_symbol())
        };
        // `RegExp.prototype`: the spec's `%RegExp.prototype%` is an ordinary
        // object; the interpreter synthesizes the method surface from
        // `Kind::RegExp`, so no own props are stamped here. Linking it at
        // realm build (rather than lazily on first `.prototype` read) is what
        // gives every RegExp instance a real `[[Prototype]]`, so
        // `Object.getPrototypeOf(/1/) === RegExp.prototype` holds — including
        // for instances built inside `eval`, which never touches the
        // interpreter's lazy surface.
        let regexp_proto = alloc_root(heap);
        // `%Date.prototype%`: an ordinary object. `Date` is deliberately NOT a
        // `GLOBAL_INTRINSICS` slot: appending it there would require a
        // runtime `intrinsic_slot` arm in `v12-interp`, which this lane does
        // not own. Installed instead as an ordinary shape-bound global
        // property (same mechanism as the `Function` constructor), which the
        // compiler's unbound-identifier path resolves through `GetGlobal`.
        let date_proto = alloc_root(heap);
        let map_proto = alloc_root(heap);
        heap.get_mut(map_proto).prototype = Some(object_proto);
        let set_proto = alloc_root(heap);
        heap.get_mut(set_proto).prototype = Some(object_proto);

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
            ("Map", map_proto),
            ("Set", set_proto),
            ("RegExp", regexp_proto),
        ] {
            if let Some(o) = intrinsics.get(name).and_then(|v| v.as_object()) {
                crate::builtins::install_ctor(heap, o, proto);
            }
        }
        wire_callable(heap, &intrinsics, "Number", NativeId::NumberConstruct);

        // Install the compile-time builtin table (isNaN, Math.floor, Array.push,
        // …) as shape-bound properties on the global and the constructors/
        // prototypes. Must run after the intrinsic slots are pushed so the
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
            map: intrinsics.get("Map").and_then(|v| v.as_object()),
            map_proto,
            set: intrinsics.get("Set").and_then(|v| v.as_object()),
            set_proto,
            proxy: intrinsics.get("Proxy").and_then(|v| v.as_object()),
            date: None,
            date_proto,
            reflect: None,
        };
        crate::builtins::install_builtins(heap, &targets);

        // Replace the macro's placeholder functions with the realm's canonical
        // well-known symbols. Both properties are non-writable,
        // non-enumerable, and non-configurable; the same handles install the
        // collection iterator aliases below.
        {
            let symbol_ctor = intrinsics
                .get("Symbol")
                .and_then(|v| v.as_object())
                .expect("Symbol intrinsic");
            let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
            assert!(ctx.redefine_data_prop_with_attrs(
                symbol_ctor,
                "iterator",
                JsValue::symbol(symbol_iterator),
                v12_heap::Attrs::new(false, false, false),
            ));
            assert!(ctx.redefine_data_prop_with_attrs(
                symbol_ctor,
                "toStringTag",
                JsValue::symbol(symbol_to_string_tag),
                v12_heap::Attrs::new(false, false, false),
            ));
        }

        // The Promise surface. Installed here rather than next to the
        // `install_ctor` call above because `Symbol.species` is only canonical
        // once this realm's well-known symbols exist. From here on
        // `Promise.resolve`/`Promise.reject` and `Promise.prototype.then`/
        // `catch` are ordinary properties: `Object.getOwnPropertyNames`,
        // descriptors, overrides and deletions all observe them, and the
        // interpreter's `promise_surface` synthesized read defers to them
        // (it only synthesizes for receivers with no `Promise.prototype` link
        // — bare-heap interpreters and interpreter-created async results).
        //
        // The handlers are the same stateful natives dispatched through the
        // registry's pending-sink seam; no reaction scheduling changes.
        if let Some(promise_ctor) = intrinsics.get("Promise").and_then(|v| v.as_object()) {
            let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
            // `Promise.length` is 1 and `Promise.name` is `"Promise"`
            // (`CreateBuiltinFunction`); the intrinsic placeholder carries
            // neither, and `install_ctor` does not stamp them.
            let promise_name = ctx.heap.intern_text("Promise");
            ctx.define_data_prop_with_attrs(
                promise_ctor,
                "length",
                JsValue::from_i32_smi(1).expect("1 fits Smi"),
                v12_heap::Attrs::new(false, false, true),
            );
            ctx.define_data_prop_with_attrs(
                promise_ctor,
                "name",
                JsValue::string(promise_name),
                v12_heap::Attrs::new(false, false, true),
            );
            for (name, id, arity) in [
                ("all", NativeId::PromiseAll, 1u32),
                ("race", NativeId::PromiseRace, 1),
                ("resolve", NativeId::PromiseResolve, 1),
                ("reject", NativeId::PromiseReject, 1),
            ] {
                ctx.define_method(Some(promise_ctor), name, id, Some(arity));
            }
            for (name, id, arity) in [
                ("then", NativeId::PromiseThen, 2u32),
                ("catch", NativeId::PromiseCatch, 1),
                ("finally", NativeId::PromiseFinally, 1),
            ] {
                ctx.define_method(Some(promise_proto), name, id, Some(arity));
            }
            // `Promise[Symbol.species] === Promise`: a non-writable,
            // non-enumerable, configurable data property (ES 27.2.4). The
            // accessor it replaces in the spec (`this`) is a later phase —
            // cross-realm species selection is out of scope for v1's single
            // realm.
            ctx.define_symbol_data_prop_with_attrs(
                promise_ctor,
                symbol_species,
                JsValue::object(promise_ctor),
                v12_heap::Attrs::new(false, false, true),
            );
            // `Promise.prototype[Symbol.toStringTag] === "Promise"`, same
            // attributes, reusing this realm's canonical tag symbol.
            let proto_tag = ctx.heap.intern_text("Promise");
            ctx.define_symbol_data_prop_with_attrs(
                promise_proto,
                symbol_to_string_tag,
                JsValue::string(proto_tag),
                v12_heap::Attrs::new(false, false, true),
            );
            // `Symbol.species`: the macro installed a handler *function* in
            // that slot, so overwrite it with this realm's canonical symbol
            // (the `toPrimitive` fix below does the same for its own well-known
            // symbol).
            if let Some(symbol_ctor) = intrinsics.get("Symbol").and_then(|v| v.as_object()) {
                ctx.overwrite_data_prop(symbol_ctor, "species", JsValue::symbol(symbol_species));
            }
        }
        // `%Promise.prototype%`'s [[Prototype]] is `%Object.prototype%`; the
        // prototype was allocated before `object_proto` existed, so the link
        // is closed here.
        heap.get_mut(promise_proto).prototype = Some(object_proto);

        for (proto, method, tag_name, size_id) in [
            (map_proto, "entries", "Map", NativeId::MapSize),
            (set_proto, "values", "Set", NativeId::SetSize),
        ] {
            let method_key = v12_heap::PropKey::from_string(heap.intern_text(method));
            let method_shape = heap.shape_of(proto);
            let method_slot = heap
                .get(method_shape)
                .find_descriptor(method_key)
                .and_then(|d| d.slot())
                .expect("collection iterator method installed");
            let iterator = heap.get(proto).properties[method_slot as usize];
            let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
            ctx.define_symbol_data_prop_with_attrs(
                proto,
                symbol_iterator,
                iterator,
                v12_heap::Attrs::BUILTIN,
            );
            ctx.define_size_getter(proto, "get size", size_id, 0);
            let tag = ctx.heap.intern_text(tag_name);
            ctx.define_symbol_data_prop_with_attrs(
                proto,
                symbol_to_string_tag,
                JsValue::string(tag),
                v12_heap::Attrs::new(false, false, true),
            );
        }

        // `Date`: created and installed as an ordinary global property (see
        // the `date_proto` note above). Statics install on the constructor,
        // prototype methods on `date_proto`, and the returned handle is
        // relinked below so `new Date(...)` finds `%Date.prototype%`.
        let date_ctor =
            crate::builtins::install_native(heap, Some(global), "Date", NativeId::DateConstruct);
        if let Some(date_ctor) = date_ctor {
            crate::builtins::install_ctor(heap, date_ctor, date_proto);
            // `Date.length` is 7 (test262 `built-ins/Date/length.js`). The
            // unified install path stamped `name` from the install name and
            // left `length` absent because `builtin_length` has no
            // `DateConstruct` entry (it is a bare dispatch id), so stamp
            // `length` with spec attrs.
            let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
            ctx.define_data_prop_with_attrs(
                date_ctor,
                "length",
                JsValue::from_i32_smi(7).expect("7 fits Smi"),
                v12_heap::Attrs::new(false, false, true),
            );
            // Statics: `Date.now`/`UTC`/`parse` install on the constructor;
            // the prototype method set on `date_proto`. `install_builtins`
            // already ran with `date: None`, so these are the only installs.
            for (name, id, len) in [
                ("now", NativeId::DateNow, 0u32),
                ("UTC", NativeId::DateUtc, 7),
                ("parse", NativeId::DateParse, 1),
            ] {
                crate::builtins::install_native_with_length(
                    heap,
                    Some(date_ctor),
                    name,
                    id,
                    Some(len),
                );
            }
            // The prototype method set (including arities) is installed by
            // `install_builtins` through its `DateProto` group; only the two
            // entries that group cannot express are wired here:
            //
            // `toGMTString` must be the SAME function object as
            // `toUTCString` (Annex B), so capture the installed handle and
            // re-install it under the second name.
            let utc_name = heap.intern_text("toUTCString");
            let utc_key = v12_heap::PropKey::from_string(utc_name);
            let utc_shape = heap.shape_of(date_proto);
            let utc_slot = heap
                .lookup_property(utc_shape, utc_key)
                .and_then(|d| d.slot())
                .expect("toUTCString installed by the DateProto group");
            let utc_fn = heap.get(date_proto).properties[utc_slot as usize];
            crate::builtins::builtin_install_prop(heap, date_proto, "toGMTString", utc_fn);
            // `Date.prototype.toTemporalInstant`: a v1 stub (no Temporal).
            crate::builtins::install_native_with_length(
                heap,
                Some(date_proto),
                "toTemporalInstant",
                NativeId::DateProtoToTemporalInstant,
                Some(0),
            );
            // `Date.prototype[Symbol.toPrimitive]`: needs a symbol key, so it
            // cannot ride the macro's string install. The realm's
            // `Symbol.toPrimitive` value must itself be a symbol; the macro
            // installed a handler *function* for it, so overwrite that slot
            // with a fresh well-known symbol first.
            {
                let symbol_ctor = intrinsics.get("Symbol").and_then(|v| v.as_object());
                if let Some(symbol_ctor) = symbol_ctor {
                    let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
                    let to_prim_sym = ctx.fresh_symbol();
                    ctx.overwrite_data_prop(
                        symbol_ctor,
                        "toPrimitive",
                        JsValue::symbol(to_prim_sym),
                    );
                    // Allocate the method function and stamp spec `length` +
                    // `name` (the symbol spelling), then install it under the
                    // symbol key.
                    let func = ctx.alloc_obj(JsObject {
                        kind: v12_heap::Kind::Function,
                        callable: FunctionTarget::Bytecode(u32::from(
                            NativeId::DateProtoToPrimitive,
                        )),
                        ..Default::default()
                    });
                    let prim_name = ctx.heap.intern_text("[Symbol.toPrimitive]");
                    ctx.define_data_prop_with_attrs(
                        func,
                        "length",
                        JsValue::from_i32_smi(1).expect("1 fits Smi"),
                        v12_heap::Attrs::new(false, false, true),
                    );
                    ctx.define_data_prop_with_attrs(
                        func,
                        "name",
                        JsValue::string(prim_name),
                        v12_heap::Attrs::new(false, false, true),
                    );
                    ctx.define_symbol_data_prop_with_attrs(
                        date_proto,
                        to_prim_sym,
                        JsValue::object(func),
                        v12_heap::Attrs::new(false, false, true),
                    );
                }
            }
        }

        // `Reflect`: an ordinary, non-callable object global (like `Math`/
        // `JSON`, NOT a `GLOBAL_INTRINSICS` slot). Its [[Prototype]] is
        // `%Object.prototype%`; the 13 statics install on it via
        // `install_builtins`'s `Reflect` group. `install_builtins` already ran
        // with `reflect: None`, so allocate first, then run a second grouped
        // install pass that targets it.
        let reflect = alloc_root(heap);
        heap.get_mut(reflect).prototype = Some(object_proto);
        // `Symbol.toStringTag` = "Reflect": non-writable, non-enumerable,
        // configurable (ES 17), reusing this realm's canonical symbol.
        {
            let mut ctx = crate::builtins::Ctx::new(heap, Some(global), None);
            let reflect_name = ctx.heap.intern_text("Reflect");
            ctx.define_symbol_data_prop_with_attrs(
                reflect,
                symbol_to_string_tag,
                JsValue::string(reflect_name),
                v12_heap::Attrs::new(false, false, true),
            );
        }
        let reflect_targets = crate::builtins::BuiltinTargets {
            global,
            math: None,
            number: None,
            number_proto,
            string: None,
            string_proto,
            array: None,
            array_proto,
            object: None,
            object_proto,
            function_proto,
            json: None,
            boolean_proto,
            symbol: None,
            symbol_proto,
            map: None,
            map_proto,
            set: None,
            set_proto,
            proxy: None,
            date: None,
            date_proto,
            reflect: Some(reflect),
        };
        crate::builtins::install_reflect_builtins(heap, &reflect_targets);
        // `Reflect` is a global property with the standard builtin attrs
        // (writable, non-enumerable, configurable).
        crate::builtins::builtin_install_prop(heap, global, "Reflect", JsValue::object(reflect));

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

        Self {
            global,
            intrinsics,
            symbol_iterator,
            symbol_to_string_tag,
            symbol_species,
        }
    }

    /// Handle to the realm's global object.
    #[must_use]
    pub fn global(&self) -> Handle<JsObject> {
        self.global
    }

    /// This realm's canonical `Symbol.iterator` handle.
    #[must_use]
    pub fn symbol_iterator(&self) -> Handle<v12_heap::V12Symbol> {
        self.symbol_iterator
    }

    /// This realm's canonical `Symbol.toStringTag` handle.
    #[must_use]
    pub fn symbol_to_string_tag(&self) -> Handle<v12_heap::V12Symbol> {
        self.symbol_to_string_tag
    }

    /// This realm's canonical `Symbol.species` handle.
    #[must_use]
    pub fn symbol_species(&self) -> Handle<v12_heap::V12Symbol> {
        self.symbol_species
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
fn wire_callable(
    heap: &mut Heap,
    intrinsics: &HashMap<String, JsValue>,
    name: &str,
    native: NativeId,
) {
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
