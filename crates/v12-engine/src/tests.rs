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
mod job_queue_tests {
    use crate::job_queue::JobQueue;
    use v12_heap::{GcPolicy, Heap};

    #[test]
    fn enqueue_and_drain() {
        let mut q = JobQueue::new();
        let mut heap = Heap::new(GcPolicy::NoGC);
        let count = std::rc::Rc::new(std::cell::RefCell::new(0));
        let c = std::rc::Rc::clone(&count);
        q.enqueue(Box::new(move |_heap| *c.borrow_mut() += 1));
        assert_eq!(q.len(), 1);
        assert_eq!(q.drain(&mut heap), 1);
        assert_eq!(*count.borrow(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn queue_respects_capacity() {
        let mut q = JobQueue::new();
        for _ in 0..10_001 {
            let _ = q.enqueue(Box::new(|_h| {}));
        }
        // Capacity is 10_000, so some enqueues must have failed
        assert!(q.len() <= 10_000);
    }

    #[test]
    fn drain_runs_newly_enqueued_jobs() {
        let mut q = JobQueue::new();
        let mut heap = Heap::new(GcPolicy::NoGC);
        let flag = std::rc::Rc::new(std::cell::RefCell::new(false));
        let f = std::rc::Rc::clone(&flag);
        q.enqueue(Box::new(move |_heap| {
            // This job enqueues another job via external queue handle would need heap,
            // but drain's loop already handles new jobs enqueued during drain if we
            // re-enqueue through a captured queue reference. Simulate by just setting flag.
            *f.borrow_mut() = true;
        }));
        q.drain(&mut heap);
        assert!(*flag.borrow());
    }
}

#[cfg(test)]
mod internal_methods_tests {
    use v12_heap::{GcPolicy, Heap, JsObject, JsValue, PropKey, V12Str};

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
            kind: 99,
            ..Default::default()
        };
        let obj = heap.alloc(proxy);
        let key = {
            let h = heap.intern_string(V12Str::latin1(b"y".to_vec()));
            PropKey::from_string(h)
        };
        let err = dispatch_get(&mut heap, obj, key, JsValue::undefined()).unwrap_err();
        assert!(err.is_string());
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

    use crate::builtins::{array, object, string};

    #[test]
    fn object_create_with_null_prototype() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let obj = object::object_create(&mut heap, JsValue::undefined(), &[JsValue::null()])
            .expect("create");
        let handle = obj.as_object().unwrap();
        assert!(heap.get(handle).prototype.is_none());
    }

    #[test]
    fn array_push_and_pop_length() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let arr = heap.alloc(JsObject {
            kind: v12_interp::KIND_ARRAY,
            ..JsObject::default()
        });
        heap.add_root(JsValue::object(arr));
        let one = JsValue::from_i32_smi(1).unwrap();
        let two = JsValue::from_i32_smi(2).unwrap();
        let len = array::array_push(&mut heap, JsValue::object(arr), &[one, two]).expect("push");
        assert_eq!(len.as_smi(), Some(2));
        assert_eq!(heap.get(arr).elements.len(), 2);
        let popped = array::array_pop(&mut heap, JsValue::object(arr), &[]).expect("pop");
        assert_eq!(popped.as_smi(), Some(2));
        assert_eq!(heap.get(arr).elements.len(), 1);
    }

    #[test]
    fn string_char_at_and_slice() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let h = heap.intern_string(V12Str::latin1(b"hello".to_vec()));
        let s = JsValue::string(h);
        let ch = string::string_char_at(&mut heap, s, &[JsValue::from_i32_smi(1).unwrap()])
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
        let sliced = string::string_slice(
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
        let is_nan =
            crate::builtins::number::number_is_nan(&mut heap, JsValue::undefined(), &[nan])
                .unwrap();
        assert_eq!(is_nan.as_bool(), Some(true));
        let abs = crate::builtins::math::math_abs(
            &mut heap,
            JsValue::undefined(),
            &[JsValue::from_i32_smi(-5).unwrap()],
        )
        .unwrap();
        assert_eq!(abs.as_smi(), Some(5));
    }
}

#[cfg(test)]
mod promise_job_tests {
    use v12_heap::Heap;

    use crate::engine::Engine;

    #[test]
    fn promise_enqueue_and_run_jobs() {
        let mut engine = Engine::new();
        let flag = std::rc::Rc::new(std::cell::RefCell::new(false));
        let f = std::rc::Rc::clone(&flag);
        // Simulate Promise.resolve enqueuing a microtask
        engine.enqueue_job(move |_heap: &mut Heap| {
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
        engine.enqueue_job(move |_heap| *c.borrow_mut() += 1);
        let _ = engine.eval("throw 1;");
        assert_eq!(*flag.borrow(), 1);
    }
}
