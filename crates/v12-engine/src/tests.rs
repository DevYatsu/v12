//! Integration tests for the engine crate.

#[cfg(test)]
mod realm_tests {
    use v12_heap::{GcPolicy, Heap};

    use crate::realm::Realm;

    #[test]
    fn realm_installs_global_and_intrinsics() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let realm = Realm::new(&mut heap);
        assert!(realm.get_intrinsic("Object").is_some());
        assert!(realm.get_intrinsic("Array").is_some());
        assert!(realm.get_intrinsic("Number").is_some());
        assert!(realm.global().index() < heap.slot_count::<v12_heap::JsObject>() as u32);
    }
}

#[cfg(test)]
mod ctx_realm_tests {
    use v12_heap::{GcPolicy, Heap, JsValue, PropKey};
    use v12_native::Throw;

    use crate::builtins::Ctx;
    use crate::realm::Realm;

    /// Reads an own data property through the object's bound shape (no
    /// prototype walk) — the same lookup production uses, so a missing
    /// `constructor` install reads `None` instead of silently resolving
    /// somewhere up the chain.
    fn own_data(
        heap: &mut Heap,
        obj: v12_heap::Handle<v12_heap::JsObject>,
        name: &str,
    ) -> Option<JsValue> {
        let key = PropKey::from_string(heap.intern_text(name));
        let slot = heap
            .lookup_property(heap.shape_of(obj), key)
            .and_then(|d| d.slot())? as usize;
        heap.get(obj).properties.get(slot).copied()
    }

    /// W3.1 regression. `helpers.rs` used to expose shims that built a
    /// throwaway `Ctx::new(heap, None, None)` and delegated to it. Three of
    /// the four shims were pure heap coercions and needed nothing — but
    /// `as_object` → `Ctx::this_object` builds a `TypeError`, and with `global:
    /// None` the `TypeError` it built was **detached from the realm**: no own
    /// `constructor` link and no `[[Prototype]]` pointing at
    /// `%TypeError.prototype%`. In a realm that had everything it needed,
    /// `err instanceof TypeError` and `err.constructor === TypeError` both
    /// read false, which is exactly what test262's `assert.throws` compares.
    ///
    /// The shim is gone, so this pins the invariant at the method its callers
    /// now reach directly: a realm-bearing `Ctx` links what it throws to the
    /// realm's own error class. Mutation check: setting `global = None` in
    /// `Ctx::make_error` (restoring the shim's behaviour) fails both asserts.
    #[test]
    fn thrown_type_error_is_linked_to_the_realm_type_error() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let realm = Realm::new(&mut heap);

        // Ground truth, read from the realm directly — deliberately *not*
        // through `Ctx::intrinsic`, so the test does not depend on the same
        // reader whose fallback it is checking.
        let type_error_ctor = realm
            .get_intrinsic("TypeError")
            .and_then(|v| v.as_object())
            .expect("realm must define the TypeError intrinsic");
        let type_error_proto = heap.get(type_error_ctor).prototype;
        assert!(
            type_error_proto.is_some(),
            "realm must define %TypeError.prototype%"
        );

        let mut ctx = Ctx::new(&mut heap, Some(realm.global()), None);
        let thrown = ctx
            .this_object(JsValue::undefined(), "Array.prototype.sort", None)
            .err()
            .expect("a non-object receiver must throw");
        let Throw::Value(value) = thrown else {
            panic!("`this_object` must throw a ready value, not a bare message");
        };
        let err = value
            .as_object()
            .expect("the thrown value must be an object");

        // [[Prototype]]: this is what makes `err instanceof TypeError` true.
        assert_eq!(
            heap.get(err).prototype,
            type_error_proto,
            "thrown error's [[Prototype]] must be the realm's %TypeError.prototype%"
        );
        // Own `constructor`: the identity test262's `assert.throws(TypeError, …)`
        // compares.
        assert_eq!(
            own_data(&mut heap, err, "constructor"),
            Some(JsValue::object(type_error_ctor)),
            "thrown error must carry an own `constructor` link to the realm TypeError"
        );
        assert_eq!(
            own_data(&mut heap, err, "name"),
            Some(JsValue::string(heap.intern_text("TypeError"))),
            "thrown error must be named TypeError"
        );
    }
}

#[cfg(test)]
mod job_queue_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::job_queue::{JobCtx, JobQueue};
    use v12_interp::Interp;

    fn interp() -> Interp<'static> {
        // Leak a heap so the test helper can return an owned interp; the
        // leak is bounded (one per test) and the interp borrows it for the
        // test's lifetime.
        let heap: &'static mut v12_heap::Heap =
            Box::leak(Box::new(v12_heap::Heap::new(v12_heap::GcPolicy::NoGC)));
        Interp::new(heap, Vec::new(), 0, Vec::new())
    }

    #[test]
    fn enqueue_and_drain() {
        let mut q = JobQueue::new();
        let mut interp = interp();
        let count = Rc::new(RefCell::new(0));
        let c = Rc::clone(&count);
        q.enqueue(Box::new(move |_ctx: &mut JobCtx<'_, '_>| {
            *c.borrow_mut() += 1
        }));
        assert_eq!(q.len(), 1);
        assert_eq!(q.drain(&mut interp, Rc::new(RefCell::new(Vec::new()))), 1);
        assert_eq!(*count.borrow(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn queue_respects_capacity() {
        let mut q = JobQueue::new();
        for _ in 0..10_001 {
            let _ = q.enqueue(Box::new(|_ctx: &mut JobCtx<'_, '_>| {}));
        }
        // Capacity is 10_000, so some enqueues must have failed
        assert!(q.len() <= 10_000);
    }

    #[test]
    fn drain_runs_newly_enqueued_jobs() {
        let mut q = JobQueue::new();
        let mut interp = interp();
        let flag = Rc::new(RefCell::new(false));
        let f = Rc::clone(&flag);
        q.enqueue(Box::new(move |_ctx: &mut JobCtx<'_, '_>| {
            *f.borrow_mut() = true;
        }));
        q.drain(&mut interp, Rc::new(RefCell::new(Vec::new())));
        assert!(*flag.borrow());
    }
}

#[cfg(test)]
mod internal_methods_tests {
    use v12_heap::{GcPolicy, Heap, JsObject, JsValue, Kind, PropKey, V12Str};

    use crate::internal_methods::{
        ObjectKind, PropertyDescriptor, dispatch_get, dispatch_has, dispatch_set, kind_of,
        methods_for,
    };

    #[test]
    fn ordinary_dispatch_get_and_has() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let obj = heap.alloc(JsObject::default());
        heap.add_root(JsValue::object(obj));
        let key = {
            let h = heap.intern_string(V12Str::latin1(b"x".to_vec()));
            PropKey::from_string(h)
        };
        let value = JsValue::from_i32_smi(7).unwrap();
        dispatch_set(&mut heap, obj, key, value, JsValue::undefined()).expect("set");
        let got = dispatch_get(&mut heap, obj, key, JsValue::undefined()).expect("get");
        assert_eq!(got.as_smi(), Some(7));
        let has = dispatch_has(&mut heap, obj, key).expect("has");
        assert!(has);
    }

    #[test]
    fn proxy_traps_throw_type_error() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let proxy = JsObject {
            kind: Kind::Proxy,
            ..Default::default()
        };
        let obj = heap.alloc(proxy);
        let key = {
            let h = heap.intern_string(V12Str::latin1(b"y".to_vec()));
            PropKey::from_string(h)
        };
        let err = dispatch_get(&mut heap, obj, key, JsValue::undefined()).unwrap_err();
        assert!(err.is_object());
        assert_eq!(heap.get(err.as_object().unwrap()).kind, Kind::Error);
        let kind = kind_of(&heap, obj);
        assert_eq!(kind, ObjectKind::Proxy);
        assert!(methods_for(kind).call.is_none());
    }

    #[test]
    fn ordinary_define_own_property_via_descriptor() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let obj = heap.alloc(JsObject::default());
        heap.add_root(JsValue::object(obj));
        let key = {
            let h = heap.intern_string(V12Str::latin1(b"p".to_vec()));
            PropKey::from_string(h)
        };
        let desc = PropertyDescriptor {
            value: Some(JsValue::from_i32_smi(42).unwrap()),
            writable: true,
            enumerable: true,
            configurable: true,
        };
        let ok = (methods_for(ObjectKind::Ordinary).define_own_property)(&mut heap, obj, key, desc)
            .expect("define");
        assert!(ok);
    }
}

#[cfg(test)]
mod builtin_tests {
    use v12_heap::{GcPolicy, Heap, JsObject, JsValue, V12Str};

    use crate::builtins::{array, ctx, object, string};
    use crate::engine::Engine;

    #[test]
    fn object_create_with_null_prototype() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let obj = ctx::call_ctx(
            object::object_create,
            &mut heap,
            JsValue::undefined(),
            &[JsValue::null()],
        )
        .expect("create");
        let handle = obj.as_object().unwrap();
        assert!(heap.get(handle).prototype.is_none());
    }

    #[test]
    fn map_set_builtins_via_engine() {
        let mut engine = Engine::new();
        // Map: set/get/has/delete/size end to end.
        let src = "let m = new Map(); m.set('a', 1); m.set('b', 2); \
                   throw [m.get('b'), m.has('a'), m.size, m.delete('a'), m.has('a')].join(',');";
        let thrown = engine.eval(src).unwrap_err();
        assert_eq!(engine.to_display_string(thrown), "2,true,2,true,false");
        // Set: add dedups, has/delete/size work.
        let src2 = "let s = new Set(); s.add(1); s.add(2); s.add(1); \
                    throw [s.size, s.has(2), s.delete(1), s.has(1)].join(',');";
        let thrown2 = engine.eval(src2).unwrap_err();
        assert_eq!(engine.to_display_string(thrown2), "2,true,true,false");
    }

    #[test]
    fn array_push_and_pop_length() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let arr = heap.alloc(JsObject::array(Vec::new()));
        heap.add_root(JsValue::object(arr));
        let one = JsValue::from_i32_smi(1).unwrap();
        let two = JsValue::from_i32_smi(2).unwrap();
        let len = ctx::call_ctx(
            array::array_push,
            &mut heap,
            JsValue::object(arr),
            &[one, two],
        )
        .expect("push");
        assert_eq!(len.as_smi(), Some(2));
        assert_eq!(heap.get(arr).elements_array.len(), 2);
        let popped =
            ctx::call_ctx(array::array_pop, &mut heap, JsValue::object(arr), &[]).expect("pop");
        assert_eq!(popped.as_smi(), Some(2));
        assert_eq!(heap.get(arr).elements_array.len(), 1);
    }

    #[test]
    fn string_char_at_and_slice() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let h = heap.intern_string(V12Str::latin1(b"hello".to_vec()));
        let s = JsValue::string(h);
        let ch = ctx::call_ctx(
            string::string_char_at,
            &mut heap,
            s,
            &[JsValue::from_i32_smi(1).unwrap()],
        )
        .expect("charAt");
        assert!(ch.is_string());
        let tmp = crate::engine::Engine::new();
        let text = {
            // Use engine heap to read string via builtins logic reuse:
            // we directly check via heap flatten
            let ch_handle = ch.as_string().unwrap();
            heap.flatten(ch_handle);
            match &heap.get(ch_handle).storage {
                v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).to_string(),
                v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
                _ => String::new(),
            }
        };
        assert_eq!(text, "e");
        let sliced = ctx::call_ctx(
            string::string_slice,
            &mut heap,
            s,
            &[
                JsValue::from_i32_smi(1).unwrap(),
                JsValue::from_i32_smi(3).unwrap(),
            ],
        )
        .expect("slice");
        let handle = sliced.as_string().unwrap();
        heap.flatten(handle);
        let out = match &heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).to_string(),
            v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
            _ => String::new(),
        };
        assert_eq!(out, "el");
        let _ = tmp;
    }

    #[test]
    fn number_and_math_builtins() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let nan = JsValue::from_f64(f64::NAN);
        let mut ctx = crate::builtins::Ctx::new(&mut heap, None, None);
        let is_nan =
            crate::builtins::number::number_is_nan(&mut ctx, JsValue::undefined(), &[nan]).unwrap();
        assert_eq!(is_nan.as_bool(), Some(true));
        let abs = crate::builtins::math::math_abs(
            &mut ctx,
            JsValue::undefined(),
            &[JsValue::from_i32_smi(-5).unwrap()],
        )
        .unwrap();
        assert_eq!(abs.as_smi(), Some(5));
    }
}

#[cfg(test)]
mod promise_job_tests {
    use crate::engine::Engine;

    #[test]
    fn promise_enqueue_and_run_jobs() {
        let mut engine = Engine::new();
        let flag = std::rc::Rc::new(std::cell::RefCell::new(false));
        let f = std::rc::Rc::clone(&flag);
        // Simulate Promise.resolve enqueuing a microtask
        engine.enqueue_job(move |_ctx: &mut crate::job_queue::JobCtx<'_, '_>| {
            *f.borrow_mut() = true;
        });
        let count = engine.run_jobs();
        assert_eq!(count, 1);
        assert!(*flag.borrow());
        // run_jobs also used as checkpoint after eval
        let _ = engine.eval("let x = 1;");
        assert_eq!(engine.jobs_mut().len(), 0);
    }

    #[test]
    fn eval_checkpoint_drains_jobs() {
        let mut engine = Engine::new();
        let flag = std::rc::Rc::new(std::cell::RefCell::new(0i32));
        let c = std::rc::Rc::clone(&flag);
        engine.enqueue_job(move |_ctx: &mut crate::job_queue::JobCtx<'_, '_>| *c.borrow_mut() += 1);
        let _ = engine.eval("throw 1;");
        assert_eq!(*flag.borrow(), 1);
    }
}

#[cfg(test)]
mod display_tests {
    use crate::engine::Engine;

    /// Display text of a value thrown by `source` (the engine's diagnostic
    /// path, `Engine::to_display_string`).
    fn thrown_text(source: &str) -> String {
        let mut engine = Engine::new();
        let thrown = engine.eval(source).expect_err("source must throw");
        engine.to_display_string(thrown)
    }

    #[test]
    fn map_results_render_as_arrays_not_opaque_objects() {
        assert_eq!(thrown_text("throw [1,2,3].map(x => x * 2);"), "2,4,6");
    }

    #[test]
    fn collections_render_their_entries() {
        assert_eq!(
            thrown_text("let s = new Set(); s.add(1); s.add('x'); throw s;"),
            "Set(2) {1, x}"
        );
        assert_eq!(
            thrown_text("let m = new Map(); m.set('a', 1); throw m;"),
            "Map(1) {a => 1}"
        );
    }

    #[test]
    fn bigints_render_decimal_with_suffix() {
        assert_eq!(thrown_text("throw 255n;"), "255n");
        assert_eq!(thrown_text("throw 0n;"), "0n");
        // 2^64, well past the f64 exact-integer range: decimal must be exact.
        assert_eq!(
            thrown_text("throw 18446744073709551616n;"),
            "18446744073709551616n"
        );
        assert_eq!(
            thrown_text("throw 123456789012345678901234567890n;"),
            "123456789012345678901234567890n"
        );
    }

    #[test]
    fn symbols_render_like_symbol_to_string() {
        assert_eq!(thrown_text("throw Symbol.iterator;"), "Symbol()");
    }

    #[test]
    fn regexps_render_source_and_flags() {
        assert_eq!(thrown_text("throw /ab+/gi;"), "/ab+/gi");
    }

    #[test]
    fn settled_promises_render_state_and_payload() {
        assert_eq!(thrown_text("throw Promise.resolve(42);"), "Promise { 42 }");
        assert_eq!(
            thrown_text("throw Promise.reject('bad');"),
            "Promise { <rejected> bad }"
        );
        assert_eq!(
            thrown_text("throw new Promise(function () {});"),
            "Promise { <pending> }"
        );
    }

    #[test]
    fn functions_render_their_name() {
        assert_eq!(
            thrown_text("function foo() {} throw foo;"),
            "[Function: foo]"
        );
        assert_eq!(thrown_text("throw function bar() {};"), "[Function: bar]");
        assert_eq!(
            thrown_text("throw function () {};"),
            "[Function (anonymous)]"
        );
        assert_eq!(thrown_text("throw () => {};"), "[Function (anonymous)]");
    }

    #[test]
    fn generators_and_iterators_render_their_kind() {
        assert_eq!(
            thrown_text("function* g() { yield 1; } throw g();"),
            "[Generator]"
        );
        assert_eq!(
            thrown_text("throw [1,2][Symbol.iterator]();"),
            "[Array Iterator]"
        );
        assert_eq!(
            thrown_text("let s = new Set(); s.add(1); throw s[Symbol.iterator]();"),
            "[Set Iterator]"
        );
        assert_eq!(
            thrown_text("let m = new Map(); m.set('a', 1); throw m[Symbol.iterator]();"),
            "[Map Iterator]"
        );
    }

    #[test]
    fn cyclic_collections_terminate_instead_of_overflowing() {
        // A Map holding itself would recurse forever without the depth cap.
        let text = thrown_text("let m = new Map(); m.set(m, 1); throw m;");
        assert!(text.starts_with("Map(1) {"), "got {text:?}");
    }

    #[test]
    fn plain_objects_stay_opaque() {
        assert_eq!(thrown_text("throw {a: 1};"), "[object Object]");
    }
}

#[cfg(test)]
mod module_namespace_tests {
    use crate::engine::Engine;

    /// Evaluates a module that captures the namespace into a global so the
    /// test can then eval assertions against it on the same engine.
    fn namespace_engine() -> Engine {
        let mut engine = Engine::new();
        let src = "import * as ns from './self.js';\n\
                   export var local1 = 201;\n\
                   var local2 = 207;\n\
                   export { local2 as renamed };\n\
                   export default 302;\n\
                   globalThis.__ns = ns;\n";
        let dir = std::env::temp_dir().join("v12_modns_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("self.js");
        engine
            .eval_module_source_at(src, &dir, Some(&path))
            .expect("module evaluates");
        engine
    }

    fn assert_eval_true(engine: &mut Engine, src: &str) {
        let v = engine.eval(src).expect("eval");
        assert_eq!(v.as_bool(), Some(true), "assertion failed: {src}");
    }

    #[test]
    fn namespace_has_null_prototype() {
        let mut e = namespace_engine();
        assert_eval_true(&mut e, "Object.getPrototypeOf(globalThis.__ns) === null");
    }

    #[test]
    fn namespace_is_not_extensible() {
        let mut e = namespace_engine();
        assert_eval_true(&mut e, "Object.isExtensible(globalThis.__ns) === false");
        // Already non-extensible: preventExtensions reports success.
        assert_eval_true(
            &mut e,
            "Object.preventExtensions(globalThis.__ns) === globalThis.__ns",
        );
    }

    #[test]
    fn namespace_export_descriptor_shape() {
        let mut e = namespace_engine();
        // Spec quirk: exports report writable:true, enumerable:true,
        // configurable:false.
        assert_eval_true(
            &mut e,
            "var d = Object.getOwnPropertyDescriptor(globalThis.__ns, 'local1');\n\
             d.value === 201 && d.writable === true && \
             d.enumerable === true && d.configurable === false",
        );
    }

    #[test]
    fn namespace_reports_sorted_export_keys() {
        let mut e = namespace_engine();
        // local1, renamed, default are the exports; sort order is
        // [default, local1, renamed] (byte order: 'd' < 'l' < 'r').
        assert_eval_true(
            &mut e,
            "Object.getOwnPropertyNames(globalThis.__ns).join(',') === \
             'default,local1,renamed'",
        );
    }

    #[test]
    fn namespace_self_import_resolves_to_same_object() {
        let mut e = namespace_engine();
        // The loader registered the entry namespace before dependencies ran,
        // so the self-import returned that object (not a fresh placeholder).
        assert_eval_true(&mut e, "globalThis.__ns.local1 === 201");
    }

    #[test]
    fn namespace_absent_key_reads_undefined() {
        let mut e = namespace_engine();
        assert_eval_true(&mut e, "globalThis.__ns.notExported === undefined");
        assert_eval_true(&mut e, "('notExported' in globalThis.__ns) === false");
    }
}

/// W3.3 — `number.rs` covered by **direct handler calls**.
///
/// Every test here constructs a heap plus a complete realm through
/// [`BuiltinTestHarness`] and calls a `number::` handler as a plain
/// `fn(&mut Ctx, JsValue, &[JsValue])`. There is no `Engine`, no `eval`, and
/// no bytecode in this module: the unit under test is the builtin body, not
/// the compiler, the dispatcher, or the operand decoder.
///
/// Why these live in `tests.rs` and not inline in `number.rs`: both
/// `scripts/freeze-lint.sh` and `scripts/unwrap-lint.sh` scope by *path*
/// (`--exclude-dir=tests --exclude=tests.rs`) and deliberately keep an inline
/// `#[cfg(test)] mod` inside a production file **in** scope. A test module full
/// of `.expect()` inline in `number.rs` would therefore need 40-odd new audited
/// allowlist entries — the opposite of the ratchet those two scripts exist to
/// enforce. `tests.rs` is the file the exclusions were written for.
#[cfg(test)]
mod number_direct_tests {
    use v12_heap::{Handle, Heap, JsObject, JsValue, PropKey};
    use v12_native::Throw;

    use crate::builtins::helpers::heap_string_text;
    use crate::builtins::{BuiltinFn, number};
    use crate::test_harness::BuiltinTestHarness;

    /// An owned Smi, panicking with a clear message rather than unwrapping.
    fn smi(n: i32) -> JsValue {
        JsValue::from_i32_smi(n).expect("test literal must fit a Smi")
    }

    /// Numeric value of a `JsValue` result, Smi or double.
    fn as_f64(v: JsValue) -> f64 {
        v.as_smi()
            .map(f64::from)
            .or(v.as_f64())
            .expect("handler must return a number")
    }

    /// Reads an own data property through the object's bound shape (no
    /// prototype walk), so a missing install reads `None` instead of silently
    /// resolving somewhere up the chain.
    fn own_data(heap: &mut Heap, obj: Handle<JsObject>, name: &str) -> Option<JsValue> {
        let key = PropKey::from_string(heap.intern_text(name));
        let slot = heap
            .lookup_property(heap.shape_of(obj), key)
            .and_then(|d| d.slot())? as usize;
        heap.get(obj).properties.get(slot).copied()
    }

    /// The object of a thrown `Throw::Value`, panicking on a bare message
    /// throw.
    fn thrown_object(t: Throw) -> Handle<JsObject> {
        let Throw::Value(v) = t else {
            panic!("handler must throw a ready value, not a bare message");
        };
        v.as_object().expect("thrown value must be an object")
    }

    /// String text of a handler's string result.
    ///
    /// A macro rather than a closure or `fn(&mut Ctx, …)`: the handler call
    /// and the heap read both want `&mut ctx`, and two overlapping `&mut`
    /// borrows of one variable cannot be written as nested call arguments. A
    /// macro expands inline, so the borrows are sequential — `$expr` is
    /// evaluated *first*, then the heap is read.
    macro_rules! text_of {
        ($ctx:expr, $expr:expr) => {{
            let v: JsValue = $expr.expect("handler must not throw");
            heap_string_text(
                $ctx.heap,
                v.as_string().expect("handler must return a string"),
            )
        }};
    }

    /// `message` own property of a thrown error object. Same macro rationale as
    /// [`text_of`]; `$expr` is the `Result` of a handler that must throw.
    macro_rules! thrown_message {
        ($ctx:expr, $expr:expr) => {{
            let err = thrown_object($expr.expect_err("handler must throw"));
            let m = own_data($ctx.heap, err, "message")
                .and_then(|v| v.as_string())
                .expect("thrown error must carry a string `message` own property");
            heap_string_text($ctx.heap, m)
        }};
    }

    // -- 1. the no-coercion statics vs. the coercing globals ----------------

    /// `Number.isNaN`/`isFinite` perform **no** `ToNumber`; the global
    /// `isNaN`/`isFinite` do. Nothing but a direct call can tell those apart
    /// for a non-number argument: `Number.isFinite("1")` is `false` while
    /// `isFinite("1")` is `true`.
    #[test]
    fn statics_do_not_coerce_but_the_globals_do() {
        let mut h = BuiltinTestHarness::new();
        h.with_ctx(|ctx| {
            let mut call = |f: BuiltinFn, args: &[JsValue]| {
                f(ctx, JsValue::undefined(), args).expect("must not throw")
            };

            // Statics: no coercion.
            assert_eq!(
                call(number::number_is_nan, &[JsValue::from_f64(f64::NAN)]).as_bool(),
                Some(true)
            );
            assert_eq!(
                call(number::number_is_nan, &[JsValue::from_f64(0.0)]).as_bool(),
                Some(false)
            );
            // A Smi is never NaN even though it has no `as_f64` reading.
            assert_eq!(
                call(number::number_is_nan, &[smi(0)]).as_bool(),
                Some(false)
            );
            assert_eq!(
                call(number::number_is_nan, &[JsValue::from_bool(true)]).as_bool(),
                Some(false)
            );

            // `Number.isFinite` answers true for a Smi (the `as_smi` arm runs
            // before the `as_f64` one) and false for a boolean.
            assert_eq!(
                call(number::number_is_finite, &[smi(7)]).as_bool(),
                Some(true)
            );
            assert_eq!(
                call(
                    number::number_is_finite,
                    &[JsValue::from_f64(f64::INFINITY)]
                )
                .as_bool(),
                Some(false)
            );
            assert_eq!(
                call(number::number_is_finite, &[JsValue::from_bool(true)]).as_bool(),
                Some(false)
            );

            // Globals: coerce first, so the same values answer differently.
            // `global_is_nan(true)` is false because ToNumber(true) = 1.
            assert_eq!(
                call(number::global_is_nan, &[JsValue::from_bool(true)]).as_bool(),
                Some(false)
            );
            assert_eq!(
                call(number::global_is_nan, &[JsValue::null()]).as_bool(),
                Some(false)
            );
            assert_eq!(
                call(number::global_is_nan, &[JsValue::undefined()]).as_bool(),
                Some(true)
            );
            assert_eq!(
                call(number::global_is_finite, &[JsValue::null()]).as_bool(),
                Some(true)
            );
            assert_eq!(
                call(
                    number::global_is_finite,
                    &[JsValue::from_f64(f64::INFINITY)]
                )
                .as_bool(),
                Some(false)
            );
            // The one pair a corpus test only finds if it happens to write it:
            // `global_is_finite` of a Smi is true, `number_is_finite` of the
            // same Smi is true too — but of a *string* they differ, and this
            // is the only place the difference is observable. Built after
            // `call` is dropped: a closure holding `*ctx` and an argument
            // expression reading `ctx.heap` cannot be live at the same time.
            drop(call);
            let one = JsValue::string(ctx.heap.intern_text("1"));
            assert_eq!(
                number::global_is_finite(ctx, JsValue::undefined(), &[one])
                    .expect("must not throw")
                    .as_bool(),
                Some(true),
                "the global coerces the string \"1\" to 1"
            );
        });
    }

    /// `parseInt` called as a plain function on a hand-built string value: the
    /// radix matrix, the `0x` prefix on the sign-less remainder, and the
    /// out-of-range radix that must be NaN rather than a panic.
    ///
    /// `parseInt("0x10", 16)` is the case that could not be asserted before:
    /// spec 19.2.5 steps 11-12 strip the prefix whenever the **effective**
    /// radix is 16, which includes an explicit radix argument. The body only
    /// stripped it on the `radix == 0` default, so an explicit `16` scanned
    /// the `0`, stopped at `x`, and returned `0`.
    #[test]
    fn parse_int_radix_matrix_directly() {
        let mut h = BuiltinTestHarness::new();
        h.with_ctx(|ctx| {
            // One closure over `ctx`, so the interned string and the handler
            // call are sequential borrows rather than two closures each
            // demanding unique access.
            let mut p = |s: &'static str, radix: Option<JsValue>| {
                let first = JsValue::string(ctx.heap.intern_text(s));
                let args: Vec<JsValue> = match radix {
                    Some(r) => vec![first, r],
                    None => vec![first],
                };
                as_f64(
                    number::global_parse_int(ctx, JsValue::undefined(), &args).expect("parseInt"),
                )
            };

            // Radix 0 (absent) means 10, except a `0x` prefix means 16.
            assert_eq!(p("0x10", None), 16.0);
            assert_eq!(p("0X1f", None), 31.0);
            assert_eq!(p("10", None), 10.0);
            assert_eq!(p("10", Some(smi(2))), 2.0);
            assert_eq!(p("z", Some(smi(36))), 35.0);
            // Spec 19.2.5 steps 11-12: the prefix is stripped for an *explicit*
            // radix 16 too, and only then.
            assert_eq!(p("0x10", Some(smi(16))), 16.0);
            assert_eq!(p("0X1f", Some(smi(16))), 31.0);
            // An explicit non-16 radix is never re-interpreted as hex.
            assert_eq!(p("0x10", Some(smi(10))), 0.0);
            // Sign is stripped before the prefix check (spec: -0x10 === -16).
            assert_eq!(p("  -0x10", None), -16.0);
            assert_eq!(p("+7", None), 7.0);
            // Scans the maximal valid prefix.
            assert_eq!(p("12abc", None), 12.0);
            // Out-of-range radix is NaN (not 1, not a panic).
            assert!(p("10", Some(smi(1))).is_nan());
            assert!(p("10", Some(smi(37))).is_nan());
            // No digits at all is NaN; an empty string included.
            assert!(p("", None).is_nan());
            assert!(p("abc", None).is_nan());
            // Missing argument: the default empty string stays NaN.
            assert!(
                as_f64(number::global_parse_int(ctx, JsValue::undefined(), &[]).expect("parseInt"))
                    .is_nan()
            );
        });
    }

    /// `parseFloat` scans only a `StrDecimalLiteral` prefix, including the two
    /// rewinds (an exponent with no digits rewinds past the `e`) and the
    /// leading-`.` case.
    #[test]
    fn parse_float_takes_the_numeric_prefix_directly() {
        let mut h = BuiltinTestHarness::new();
        h.with_ctx(|ctx| {
            let mut p = |s: &'static str| {
                let v = JsValue::string(ctx.heap.intern_text(s));
                as_f64(
                    number::global_parse_float(ctx, JsValue::undefined(), &[v])
                        .expect("parseFloat"),
                )
            };
            assert_eq!(p("3.14abc"), 3.14);
            assert_eq!(p("  -2.5e3xyz"), -2500.0);
            // Exponent with no digits rewinds past the `e`: `1e` is 1.
            assert_eq!(p("1e"), 1.0);
            assert_eq!(p("1.5e+"), 1.5);
            // Leading `.` with no integer digits is still a literal.
            assert_eq!(p(".5"), 0.5);
            // No digits anywhere is NaN.
            assert!(p("abc").is_nan());
            assert!(p("e5").is_nan());
            // Non-string input goes through ToString.
            assert_eq!(
                as_f64(
                    number::global_parse_float(ctx, JsValue::undefined(), &[smi(42)])
                        .expect("parseFloat")
                ),
                42.0
            );
            assert!(
                as_f64(
                    number::global_parse_float(ctx, JsValue::undefined(), &[]).expect("parseFloat")
                )
                .is_nan()
            );
        });
    }

    /// `Number(value)` — the callable intrinsic, reached directly. The
    /// `StringNumericLiteral` arms of `Ctx::to_number` (`0x`/`0b`/`0o`, and
    /// surrounding whitespace) are only observable here without also going
    /// through the compiler's own literal parser.
    #[test]
    fn number_intrinsic_coerces_its_argument_directly() {
        let mut h = BuiltinTestHarness::new();
        h.with_ctx(|ctx| {
            // One closure over `ctx` at a time: two closures each demanding
            // unique access to `*ctx` cannot coexist, so each phase builds its
            // own and lets it drop before the next.
            let mut ns = |s: &'static str| {
                let v = JsValue::string(ctx.heap.intern_text(s));
                as_f64(number::number_construct(ctx, JsValue::undefined(), &[v]).expect("Number()"))
            };
            assert_eq!(ns("0x10"), 16.0);
            assert_eq!(ns("0b101"), 5.0);
            assert_eq!(ns("  12  "), 12.0);
            assert_eq!(ns(""), 0.0);
            assert!(ns("x").is_nan());
            drop(ns);

            // Non-string arguments, called without a closure so the argument
            // expressions can read `ctx` too.
            let mut n = |args: &[JsValue]| {
                as_f64(number::number_construct(ctx, JsValue::undefined(), args).expect("Number()"))
            };
            assert_eq!(n(&[]), 0.0);
            assert_eq!(n(&[JsValue::null()]), 0.0);
            assert_eq!(n(&[JsValue::from_bool(true)]), 1.0);
            assert_eq!(n(&[JsValue::from_bool(false)]), 0.0);
            // No argument is +0, but an explicit `undefined` is NaN.
            assert!(n(&[JsValue::undefined()]).is_nan());
            assert_eq!(n(&[smi(7)]), 7.0);
        });
    }

    // -- 2. the realm-dependent assertions ---------------------------------

    /// **The seam's own invariant, tested through it.**
    ///
    /// `number_proto_to_string` rejects an out-of-range radix with
    /// `ctx.range_error`, and `Ctx::make_error` links that error object to the
    /// realm only when `Ctx.global` is `Some`. The expectations are read
    /// through [`BuiltinTestHarness::realm_intrinsic`] — `Realm::get_intrinsic`,
    /// the ground-truth reader — deliberately *not* through `Ctx::intrinsic`,
    /// whose `realm_globals()` fallback is one of the things that could mask a
    /// missing link.
    ///
    /// Mutation check: `with_ctx` passing `None` instead of
    /// `Some(self.realm.global())` — i.e. exactly the `helpers.rs` shim bug
    /// W3.1 closed — fails on the `[[Prototype]]` assert (`left: None`).
    #[test]
    fn proto_to_string_rejects_a_bad_radix_with_a_realm_linked_error() {
        let mut h = BuiltinTestHarness::new();
        // Captured before the closure so the asserts can run while `ctx` holds
        // the heap's only mutable borrow.
        let range_ctor = h
            .realm_intrinsic_object("RangeError")
            .expect("realm must define the RangeError intrinsic");
        let range_proto = h.heap().get(range_ctor).prototype;
        assert!(
            range_proto.is_some(),
            "realm must define %RangeError.prototype%"
        );

        let thrown = h.with_ctx(|ctx| {
            // Radix 1 is below the spec's 2..=36 window.
            number::number_proto_to_string(ctx, smi(255), &[smi(1)])
                .expect_err("radix 1 must throw")
        });
        let err = thrown_object(thrown);

        h.with_ctx(|ctx| {
            assert_eq!(
                ctx.heap.get(err).prototype,
                range_proto,
                "thrown RangeError's [[Prototype]] must be the realm's \
                 %RangeError.prototype%; `None` means the context carried no \
                 realm and the error was detached"
            );
            assert_eq!(
                own_data(ctx.heap, err, "constructor"),
                Some(JsValue::object(range_ctor)),
                "thrown RangeError must carry an own `constructor` link to the \
                 realm's RangeError"
            );
            assert_eq!(
                own_data(ctx.heap, err, "name"),
                Some(JsValue::string(ctx.heap.intern_text("RangeError"))),
                "thrown error must be named RangeError"
            );
        });
    }

    /// The same realm linkage on a `TypeError` from a different handler and a
    /// different failure mode: `Number.prototype.valueOf` called on a
    /// non-number receiver (`this_number`'s check), rather than a bad
    /// argument. Two handlers, one invariant — plus the accepting half.
    #[test]
    fn proto_value_of_rejects_a_non_number_receiver_with_a_realm_linked_error() {
        let mut h = BuiltinTestHarness::new();
        let type_ctor = h
            .realm_intrinsic_object("TypeError")
            .expect("realm must define the TypeError intrinsic");
        let type_proto = h.heap().get(type_ctor).prototype;

        let thrown = h.with_ctx(|ctx| {
            let receiver = JsValue::string(ctx.heap.intern_text("nope"));
            number::number_proto_value_of(ctx, receiver, &[])
                .expect_err("a string receiver must throw")
        });
        let err = thrown_object(thrown);

        h.with_ctx(|ctx| {
            assert_eq!(ctx.heap.get(err).prototype, type_proto);
            assert_eq!(
                own_data(ctx.heap, err, "constructor"),
                Some(JsValue::object(type_ctor))
            );
            // The receiver check names the method, per `this_number`.
            assert_eq!(
                thrown_message!(ctx, Err::<JsValue, _>(Throw::Value(JsValue::object(err)))),
                "Number.prototype.valueOf requires that 'this' be a Number",
                "message must name the method that rejected the receiver"
            );
        });

        // The accepting half: a number receiver returns the receiver itself.
        h.with_ctx(|ctx| {
            assert_eq!(
                number::number_proto_value_of(ctx, smi(42), &[])
                    .expect("valueOf on a number")
                    .as_smi(),
                Some(42)
            );
        });
    }

    // -- 3. formatting ------------------------------------------------------

    /// `Number.prototype.toString(radix)` directly: radix rendering, the
    /// `radix == 10` short-circuit that reuses `number_to_string`, and the
    /// integral-double spelling a corpus only reaches by writing the literal.
    #[test]
    fn proto_to_string_renders_each_radix_directly() {
        let mut h = BuiltinTestHarness::new();
        h.with_ctx(|ctx| {
            let mut s = |this: JsValue, args: &[JsValue]| {
                let v = number::number_proto_to_string(ctx, this, args).expect("toString");
                heap_string_text(&mut *ctx.heap, v.as_string().expect("a string"))
            };
            assert_eq!(s(smi(255), &[smi(16)]), "ff");
            assert_eq!(s(smi(255), &[smi(2)]), "11111111");
            assert_eq!(s(smi(123), &[smi(3)]), "11120");
            assert_eq!(s(smi(255), &[JsValue::undefined()]), "255");
            assert_eq!(s(smi(35), &[smi(36)]), "z");
            // Negative: sign restored after the magnitude is rendered.
            assert_eq!(s(smi(-255), &[smi(16)]), "-ff");
            // A fractional receiver: the 14-significant-digit approximation.
            assert_eq!(s(JsValue::from_f64(0.5), &[smi(2)]), "0.1");
            // Radix 10 goes through `number_to_string`, not the radix loop.
            assert_eq!(s(JsValue::from_f64(1e21), &[]), "1e+21");
            assert_eq!(s(JsValue::from_f64(1e-7), &[]), "1e-7");
            assert_eq!(s(JsValue::from_f64(0.1), &[]), "0.1");
            assert_eq!(s(JsValue::from_f64(123.456), &[]), "123.456");
            assert_eq!(s(JsValue::from_f64(f64::NAN), &[]), "NaN");
            assert_eq!(s(JsValue::from_f64(f64::NEG_INFINITY), &[]), "-Infinity");
            // An explicitly given radix 10 takes the same branch.
            assert_eq!(s(smi(42), &[smi(10)]), "42");
        });
    }

    /// `toFixed` / `toPrecision` / `toExponential` directly, including their
    /// three independent `RangeError` windows (0..100, 1..100, 0..100) and the
    /// two spec steps that only a direct call reaches:
    ///
    /// * `toFixed(undefined)` — spec 21.1.3.3 consults the argument only via
    ///   `ToIntegerOrInfinity`, which `undefined` never reaches, so it is the
    ///   same as absent. Coercing to `Number` first made it NaN, which fell
    ///   outside the `0..=100` window and threw a spurious `RangeError`.
    /// * `toPrecision(3)` on `123` — spec 21.1.3.5's `e == p - 1` case returns
    ///   the `p` digits with **no** decimal point. Rust's `{:e}` mantissa
    ///   still carries one (`1.23e2`), and the body returned it verbatim.
    #[test]
    fn fixed_precision_and_exponential_directly() {
        let mut h = BuiltinTestHarness::new();
        h.with_ctx(|ctx| {
            let f = number::number_to_fixed;
            let p = number::number_to_precision;
            let e = number::number_to_exponential;

            assert_eq!(text_of!(ctx, f(ctx, smi(1234), &[])), "1234");
            assert_eq!(text_of!(ctx, f(ctx, smi(1234), &[smi(2)])), "1234.00");
            assert_eq!(
                text_of!(ctx, f(ctx, JsValue::from_f64(123.456), &[smi(2)])),
                "123.46"
            );
            assert_eq!(
                text_of!(ctx, f(ctx, JsValue::from_f64(0.0), &[smi(3)])),
                "0.000"
            );
            // |x| >= 1e21 falls back to plain ToString.
            assert_eq!(
                text_of!(ctx, f(ctx, JsValue::from_f64(1e21), &[smi(2)])),
                "1e+21"
            );
            assert_eq!(
                text_of!(ctx, f(ctx, JsValue::from_f64(f64::NAN), &[])),
                "NaN"
            );
            // Explicit `undefined` digits is 0, not a RangeError.
            assert_eq!(text_of!(ctx, f(ctx, smi(7), &[JsValue::undefined()])), "7");

            // Absent / undefined precision is plain ToString.
            assert_eq!(
                text_of!(ctx, p(ctx, JsValue::from_f64(123.456), &[])),
                "123.456"
            );
            assert_eq!(
                text_of!(
                    ctx,
                    p(ctx, JsValue::from_f64(123.456), &[JsValue::undefined()])
                ),
                "123.456"
            );
            assert_eq!(
                text_of!(ctx, p(ctx, JsValue::from_f64(123.456), &[smi(4)])),
                "123.5"
            );
            assert_eq!(
                text_of!(ctx, p(ctx, JsValue::from_f64(0.000123), &[smi(2)])),
                "0.00012"
            );
            // `e == p - 1`: the p digits, no decimal point.
            assert_eq!(text_of!(ctx, p(ctx, smi(123), &[smi(3)])), "123");
            // `e < -6` is out of the fixed-notation window, so exponential.
            assert_eq!(
                text_of!(ctx, p(ctx, JsValue::from_f64(0.0000001), &[smi(2)])),
                "1.0e-7"
            );
            assert_eq!(
                text_of!(ctx, p(ctx, JsValue::from_f64(f64::NAN), &[smi(2)])),
                "NaN"
            );

            assert_eq!(
                text_of!(ctx, e(ctx, JsValue::from_f64(123.456), &[])),
                "1.23456e+2"
            );
            assert_eq!(
                text_of!(ctx, e(ctx, JsValue::from_f64(123.456), &[smi(2)])),
                "1.23e+2"
            );
            assert_eq!(
                text_of!(ctx, e(ctx, JsValue::from_f64(0.000001), &[smi(1)])),
                "1.0e-6"
            );
            assert_eq!(
                text_of!(ctx, e(ctx, JsValue::from_f64(f64::INFINITY), &[smi(2)])),
                "Infinity"
            );
            assert_eq!(
                text_of!(ctx, e(ctx, JsValue::from_f64(f64::NAN), &[smi(2)])),
                "NaN"
            );
            // Explicit `undefined` fraction digits is the shortest form.
            assert_eq!(
                text_of!(
                    ctx,
                    e(ctx, JsValue::from_f64(123.456), &[JsValue::undefined()])
                ),
                "1.23456e+2"
            );

            // Three distinct RangeError windows, distinguished by message.
            assert_eq!(
                thrown_message!(ctx, f(ctx, smi(1), &[smi(101)])),
                "toFixed() digits argument must be between 0 and 100"
            );
            assert_eq!(
                thrown_message!(ctx, p(ctx, smi(1), &[smi(0)])),
                "toPrecision() argument must be between 1 and 100"
            );
            assert_eq!(
                thrown_message!(ctx, p(ctx, smi(1), &[smi(101)])),
                "toPrecision() argument must be between 1 and 100"
            );
            assert_eq!(
                thrown_message!(ctx, e(ctx, smi(1), &[smi(101)])),
                "toExponential() argument must be between 0 and 100"
            );
        });
    }

    /// The integral predicates and the `Number.*` constants, read straight off
    /// the handlers. `number_is_safe_integer` is the only place the
    /// ±(2^53−1) bound is written at all, so nothing but a direct call pins
    /// it — and it is reachable without constructing a `Number` instance.
    #[test]
    fn integer_predicates_and_constants_directly() {
        let mut h = BuiltinTestHarness::new();
        h.with_ctx(|ctx| {
            let mut b = |f: BuiltinFn, v: JsValue| {
                f(ctx, JsValue::undefined(), &[v])
                    .expect("must not throw")
                    .as_bool()
            };
            assert_eq!(b(number::number_is_integer, smi(3)), Some(true));
            assert_eq!(
                b(number::number_is_integer, JsValue::from_f64(3.5)),
                Some(false)
            );
            assert_eq!(
                b(number::number_is_integer, JsValue::from_f64(f64::INFINITY)),
                Some(false)
            );
            assert_eq!(b(number::number_is_safe_integer, smi(3)), Some(true));
            assert_eq!(
                b(
                    number::number_is_safe_integer,
                    JsValue::from_f64(9_007_199_254_740_992.0)
                ),
                Some(false)
            );
            assert_eq!(
                b(
                    number::number_is_safe_integer,
                    JsValue::from_f64(9_007_199_254_740_991.0)
                ),
                Some(true)
            );
            drop(b);
            // No coercion: a numeric string is not an integer value. Built
            // after `b` is dropped — a closure holding `*ctx` and an argument
            // expression reading `ctx.heap` cannot be live at the same time.
            let three = JsValue::string(ctx.heap.intern_text("3"));
            assert_eq!(
                number::number_is_integer(ctx, JsValue::undefined(), &[three])
                    .expect("must not throw")
                    .as_bool(),
                Some(false)
            );

            let mut c = |f: BuiltinFn| as_f64(f(ctx, JsValue::undefined(), &[]).expect("constant"));
            assert_eq!(
                c(number::number_const_max_safe_integer),
                9_007_199_254_740_991.0
            );
            assert_eq!(
                c(number::number_const_min_safe_integer),
                -9_007_199_254_740_991.0
            );
            assert_eq!(c(number::number_const_epsilon), f64::EPSILON);
            assert_eq!(c(number::number_const_max_value), f64::MAX);
            assert_eq!(c(number::number_const_min_value), 5e-324);
            assert!(c(number::number_const_positive_infinity).is_infinite());
            assert_eq!(c(number::number_const_negative_infinity), f64::NEG_INFINITY);
            assert!(c(number::number_const_nan).is_nan());
        });
    }
}
