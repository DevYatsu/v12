//! Focused unit tests over hand-built bytecode: the wide-operand encodings,
//! handler delivery depth, the call-depth guard, the native seam, and
//! fall-off-the-end completion. The differential suite in
//! `tests/differential.rs` covers compiled Tier-1 programs end to end.

use v12_bytecode::{Const, ConstantPool, FunctionBytecode, HandlerRange, Instr, Opcode, WideOp};
use v12_heap::{Heap, JsValue};

use crate::{Interp, JSException, NativeRegistry};

/// Runs `interp`, expecting an uncaught throw; returns the thrown value.
fn expect_throw(interp: &mut Interp) -> JsValue {
    match interp.run() {
        Err(JSException(v)) => v,
        Ok(()) => panic!("expected an uncaught exception"),
    }
}

fn empty_fn(max_regs: u16, instrs: Vec<Instr>, consts: ConstantPool) -> FunctionBytecode {
    let spans = vec![(0, 0); instrs.len()];
    FunctionBytecode {
        name_hint: None,
        max_regs,
        instrs,
        consts,
        handlers: Vec::new(),
        spans,
        pc_map: Vec::new(),
        is_strict: false,
    }
}

fn program_of(main: FunctionBytecode) -> Interp {
    Interp::new(vec![main], 0, Vec::new())
}

#[test]
fn wide_operands_execute_with_documented_layouts() {
    // r2 = const 1000 (16-bit pool id)
    // SetEnvSlotW src=r2 depth=0 slot=0   (wide env store)
    // GetEnvSlotW dst=r1 depth=0 slot=0   (wide env load)
    // LoadIntW r3 = 300                   (wide i64 immediate)
    // Add r4 = r1 + r3                    → 1300
    // Throw r4                            (surface the result)
    let mut pool = ConstantPool::new();
    let k1000 = pool.insert(Const::F64(1000.0)).expect("fits");

    let mut instrs = vec![Instr::new(Opcode::NewEnvironment, 0, 1, 0)];
    instrs.push(Instr::new_imm16(Opcode::LoadConst, 2, k1000));
    instrs.extend(
        WideOp::SetEnvSlotW {
            src: 2,
            depth: 0,
            slot: 0,
        }
        .encode(),
    );
    instrs.extend(
        WideOp::GetEnvSlotW {
            dst: 1,
            depth: 0,
            slot: 0,
        }
        .encode(),
    );
    instrs.extend(WideOp::LoadIntW { dst: 3, value: 300 }.encode());
    instrs.push(Instr::new(Opcode::Add, 4, 1, 3));
    instrs.push(Instr::new(Opcode::Throw, 4, 0, 0));

    let mut interp = program_of(empty_fn(5, instrs, pool));
    let thrown = expect_throw(&mut interp);
    assert_eq!(thrown.as_smi(), Some(1300));
}

#[test]
fn handler_delivery_lands_in_register_stack_depth() {
    // Range [0,1) guarded with target pc 1 and depth 0: the exception value
    // must arrive in r0 exactly when the window truncates to zero registers.
    let instrs = vec![
        Instr::new(Opcode::Throw, 0, 0, 0),  // pc 0
        Instr::new(Opcode::Return, 0, 0, 0), // pc 1: handler target
    ];
    let mut fb = empty_fn(1, instrs, ConstantPool::new());
    fb.handlers.push(HandlerRange {
        start: 0,
        end: 1,
        target: 1,
        stack_depth: 0,
    });
    let mut interp = program_of(fb);
    interp.run().expect("handler absorbs the throw");
}

#[test]
fn falling_off_the_end_completes_normally() {
    let mut interp = Interp::from_source("let x = 1;").expect("compiles");
    interp.run().expect("implicit undefined completion");
}

#[test]
fn runaway_recursion_surfaces_range_error() {
    let mut interp = Interp::from_source("function f() { return f(); } f();").expect("compiles");
    let thrown = expect_throw(&mut interp);
    let msg = interp.to_display_string(thrown);
    assert!(msg.starts_with("RangeError:"), "{msg}");
}

#[test]
fn recursion_below_the_limit_still_works() {
    let mut interp = Interp::from_source(
        "function sumTo(n) { return n === 0 ? 0 : n + sumTo(n - 1); } throw sumTo(500);",
    )
    .expect("compiles");
    assert_eq!(expect_throw(&mut interp).as_smi(), Some(125_250));
}

/// Native seam probe: reports `argc * 10 + index`, proving both directions
/// of the ABI handoff for indices beyond the compiled program.
struct ProbeNatives;

impl NativeRegistry for ProbeNatives {
    fn call_native(
        &mut self,
        _heap: &mut Heap,
        _this: JsValue,
        args: &[JsValue],
        index: u32,
    ) -> Result<JsValue, JsValue> {
        assert_eq!(index, 255);
        Ok(crate::ops::box_number(
            f64::from(u32::try_from(args.len()).expect("small")) * 10.0 + f64::from(index),
        ))
    }
}

#[test]
fn calls_beyond_the_program_route_through_the_native_seam() {
    let mut interp = Interp::from_source("let f = function (a, b) { return a; }; throw f(7, 8);")
        .expect("compiles");
    // Retarget every closure to function index 255, which lies beyond the
    // compiled program and therefore names a native.
    for f in interp.functions_mut_for_test() {
        for instr in &mut f.instrs {
            if instr.op() == Some(Opcode::Closure) {
                *instr = Instr::new(Opcode::Closure, instr.a(), 255, 0);
            }
        }
    }
    interp.set_natives(Box::new(ProbeNatives));
    assert_eq!(expect_throw(&mut interp).as_smi(), Some(275));
}

#[test]
fn unregistered_natives_report_type_error() {
    let mut interp =
        Interp::from_source("let f = function () { return 1; }; try { f(); } catch (e) {}")
            .expect("compiles");
    // Default registry: retargeted calls throw; here the catch swallows it.
    for f in interp.functions_mut_for_test() {
        for instr in &mut f.instrs {
            if instr.op() == Some(Opcode::Closure) {
                *instr = Instr::new(Opcode::Closure, instr.a(), 255, 0);
            }
        }
    }
    interp.run().expect("catch handles the native TypeError");
}

#[test]
fn monomorphic_ic_stays_correct_across_shape_changes() {
    // One hot pair of sites read under two different layouts; results must be
    // exact whether each access hits or misses the cache.
    let mut interp = Interp::from_source(
        "
        let total = 0;
        let a = {x: 1};
        let b = {y: 9, x: 2};
        for (let i = 0; i < 4; i += 1) {
            total += a.x;
            total += b.x;
        }
        if (total !== 12) { throw 'bad'; }
        ",
    )
    .expect("compiles");
    interp.run().expect("IC validated across shapes");
}

#[test]
fn tier_up_counters_cross_the_threshold_once() {
    use crate::feedback::FEEDBACK_TIER_UP_THRESHOLD;

    let loops = usize::from(FEEDBACK_TIER_UP_THRESHOLD) + 50;
    let src = format!(
        "
        let s = 0;
        for (let i = 0; i < {loops}; i += 1) {{ s += 1; }}
        "
    );
    let mut interp = Interp::from_source(&src).expect("compiles");
    // Re-run through a hook we can inspect after the fact by installing a
    // shared recorder before execution.
    let recorder = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u32>>>);
    impl crate::TierHooks for Shared {
        fn on_tier_up(&mut self, function_index: u32) {
            self.0.borrow_mut().push(function_index);
        }
    }
    interp.set_hooks(Box::new(Shared(std::rc::Rc::clone(&recorder))));
    interp.run().expect("hot loop completes");
    assert_eq!(*recorder.borrow(), vec![0], "main crosses exactly once");
}

#[test]
fn gc_roots_keep_frames_alive_under_collection_stress() {
    let mut interp = Interp::from_source(
        "
        function make(n) {
            let o = {n: n};
            return o;
        }
        let acc = 0;
        for (let i = 0; i < 200; i += 1) {
            acc += make(i).n;
        }
        throw acc;
        ",
    )
    .expect("compiles");
    interp.heap_mut_for_test().gc_stress(Some(1));
    assert_eq!(expect_throw(&mut interp).as_smi(), Some(19_900));
}

#[test]
fn alloc_inside_call_keeps_arguments_rooted() {
    // Allocation-heavy callees must observe intact caller windows even while
    // the collector runs between instructions.
    let mut interp = Interp::from_source(
        "
        let junk = {};
        function noise() { junk.a = 1; junk.b = 2; junk.c = junk.a; return 0; }
        function id(x) { noise(); return x; }
        let out = id(41);
        throw out + 1;
        ",
    )
    .expect("compiles");
    interp.heap_mut_for_test().gc_stress(Some(1));
    assert_eq!(expect_throw(&mut interp).as_smi(), Some(42));
}
/// Regression for file-based `try { throw 99; } catch (e) { caught = e; }`.
///
/// The handler delivers the exception into register `stack_depth` and then
/// copies it into the catch binding. The interpreter must keep the full
/// register window (`max_regs`) alive across the unwind so handler
/// temporaries beyond `stack_depth` remain addressable.
#[test]
fn catch_binding_delivers_correct_value() {
    let mut interp =
        Interp::from_source("let caught=0; try { throw 99; } catch(e){caught=e;} throw caught;")
            .expect("catch program should compile");
    assert_eq!(expect_throw(&mut interp).as_smi(), Some(99));
}

/// Catch inside a function, ensuring the delivery register and the binding
/// work through the call frame's window.
#[test]
fn catch_binding_inside_function() {
    let mut interp = Interp::from_source(
        "
        function f() {
            let caught = 0;
            try { throw 42; } catch (e) { caught = e; }
            return caught;
        }
        throw f();
        ",
    )
    .expect("catch in function should compile");
    assert_eq!(expect_throw(&mut interp).as_smi(), Some(42));
}

// ---------------------------------------------------------------------------
// `in` operator
// ---------------------------------------------------------------------------

#[test]
fn in_operator_own_property() {
    let mut interp =
        Interp::from_source("let o = {a: 1, b: 2}; throw ('a' in o);").expect("compiles");
    let v = expect_throw(&mut interp);
    assert_eq!(v.as_bool(), Some(true));
}

#[test]
fn in_operator_missing_property() {
    let mut interp = Interp::from_source("let o = {a: 1}; throw ('z' in o);").expect("compiles");
    let v = expect_throw(&mut interp);
    assert_eq!(v.as_bool(), Some(false));
}

#[test]
fn in_operator_non_object_rhs_throws() {
    let mut interp = Interp::from_source("throw ('a' in 123);").expect("compiles");
    let thrown = expect_throw(&mut interp);
    let msg = interp.to_display_string(thrown);
    assert!(msg.contains("TypeError"), "expected TypeError, got {msg}");
}

#[test]
fn in_operator_heap_prototype_chain() {
    // Parent has "inheritedKey", child prototypes parent: `inheritedKey in child` true.
    let mut interp = program_of(empty_fn(
        2,
        vec![Instr::new(Opcode::Return, 0, 0, 0)],
        ConstantPool::new(),
    ));
    let parent = {
        let heap = interp.heap_mut_for_test();
        let p = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(v12_heap::JsValue::object(p));
        p
    };
    let child = {
        let heap = interp.heap_mut_for_test();
        let c = heap.alloc(v12_heap::JsObject {
            prototype: Some(parent),
            ..Default::default()
        });
        heap.add_root(v12_heap::JsValue::object(c));
        c
    };
    let (_key, key_v, shape) = {
        let heap = interp.heap_mut_for_test();
        let s = heap.intern_string(v12_heap::V12Str::latin1(b"inheritedKey".to_vec()));
        heap.add_root(v12_heap::JsValue::string(s));
        let key = v12_heap::PropKey::from_string(s);
        let key_v = v12_heap::JsValue::string(s);
        let shape = heap.add_property(heap.root_shape(), key, v12_heap::Attrs::DEFAULT);
        heap.add_shape_root(shape);
        (key, key_v, shape)
    };
    interp.bind_shape_for_test(parent, shape);
    interp
        .heap_mut_for_test()
        .get_mut(parent)
        .properties
        .push(v12_heap::JsValue::from_i32_smi(1).unwrap());
    let child_v = v12_heap::JsValue::object(child);
    assert!(interp.op_in_for_test(key_v, child_v).unwrap());
    // Missing key is false.
    let missing_v = {
        let heap = interp.heap_mut_for_test();
        let s = heap.intern_string(v12_heap::V12Str::latin1(b"missing".to_vec()));
        v12_heap::JsValue::string(s)
    };
    assert!(!interp.op_in_for_test(missing_v, child_v).unwrap());
    // Own property on child also true.
    {
        let heap = interp.heap_mut_for_test();
        let s = heap.intern_string(v12_heap::V12Str::latin1(b"own".to_vec()));
        heap.add_root(v12_heap::JsValue::string(s));
        let k = v12_heap::PropKey::from_string(s);
        let child_shape = heap.add_property(heap.root_shape(), k, v12_heap::Attrs::DEFAULT);
        heap.add_shape_root(child_shape);
        let s_handle = s;
        let _k = k;
        interp.bind_shape_for_test(child, child_shape);
        interp
            .heap_mut_for_test()
            .get_mut(child)
            .properties
            .push(v12_heap::JsValue::from_i32_smi(2).unwrap());
        let own_v = v12_heap::JsValue::string(s_handle);
        assert!(interp.op_in_for_test(own_v, child_v).unwrap());
    }
}

// ---------------------------------------------------------------------------
// `instanceof` operator
// ---------------------------------------------------------------------------

#[test]
fn instanceof_plain_objects_false() {
    // obj not linked to Ctor.prototype => false.
    let mut interp = Interp::from_source(
        "
        let Ctor = function () {};
        Ctor.prototype = {kind: 'proto'};
        let obj = {a: 1};
        throw (obj instanceof Ctor);
        ",
    )
    .expect("compiles");
    let v = expect_throw(&mut interp);
    assert_eq!(v.as_bool(), Some(false));
}

#[test]
fn instanceof_prototype_chain_via_heap() {
    // Instance's prototype chain contains Ctor.prototype => true.
    let mut interp = program_of(empty_fn(
        2,
        vec![Instr::new(Opcode::Return, 0, 0, 0)],
        ConstantPool::new(),
    ));
    let proto = {
        let heap = interp.heap_mut_for_test();
        let p = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(v12_heap::JsValue::object(p));
        p
    };
    let ctor = {
        let heap = interp.heap_mut_for_test();
        let c = heap.alloc(v12_heap::JsObject {
            kind: crate::KIND_FUNCTION,
            ..Default::default()
        });
        heap.add_root(v12_heap::JsValue::object(c));
        c
    };
    let instance = {
        let heap = interp.heap_mut_for_test();
        let i = heap.alloc(v12_heap::JsObject {
            prototype: Some(proto),
            ..Default::default()
        });
        heap.add_root(v12_heap::JsValue::object(i));
        i
    };
    // Ctor.prototype = proto
    let shape = {
        let heap = interp.heap_mut_for_test();
        let s = heap.intern_string(v12_heap::V12Str::latin1(b"prototype".to_vec()));
        heap.add_root(v12_heap::JsValue::string(s));
        let key = v12_heap::PropKey::from_string(s);
        let shape = heap.add_property(heap.root_shape(), key, v12_heap::Attrs::DEFAULT);
        heap.add_shape_root(shape);
        shape
    };
    interp.bind_shape_for_test(ctor, shape);
    interp
        .heap_mut_for_test()
        .get_mut(ctor)
        .properties
        .push(v12_heap::JsValue::object(proto));
    let lhs_v = v12_heap::JsValue::object(instance);
    let rhs_v = v12_heap::JsValue::object(ctor);
    assert!(interp.op_instanceof_for_test(lhs_v, rhs_v).unwrap());
    // Unrelated object is false.
    let unrelated = {
        let heap = interp.heap_mut_for_test();
        let u = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(v12_heap::JsValue::object(u));
        u
    };
    assert!(
        !interp
            .op_instanceof_for_test(v12_heap::JsValue::object(unrelated), rhs_v)
            .unwrap()
    );
    // Primitive lhs is false, not throw.
    assert!(
        !interp
            .op_instanceof_for_test(v12_heap::JsValue::from_i32_smi(5).unwrap(), rhs_v)
            .unwrap()
    );
    // Chain of two hops: instance -> middle -> proto
    let middle = {
        let heap = interp.heap_mut_for_test();
        let m = heap.alloc(v12_heap::JsObject {
            prototype: Some(proto),
            ..Default::default()
        });
        heap.add_root(v12_heap::JsValue::object(m));
        m
    };
    let instance2 = {
        let heap = interp.heap_mut_for_test();
        let i2 = heap.alloc(v12_heap::JsObject {
            prototype: Some(middle),
            ..Default::default()
        });
        heap.add_root(v12_heap::JsValue::object(i2));
        i2
    };
    assert!(
        interp
            .op_instanceof_for_test(v12_heap::JsValue::object(instance2), rhs_v)
            .unwrap()
    );
}

#[test]
fn instanceof_non_object_lhs_false() {
    let mut interp = Interp::from_source(
        "
        let Ctor = function () {};
        Ctor.prototype = {};
        throw (123 instanceof Ctor);
        ",
    )
    .expect("compiles");
    let v = expect_throw(&mut interp);
    assert_eq!(v.as_bool(), Some(false));
}

#[test]
fn instanceof_non_callable_rhs_throws() {
    let mut interp = Interp::from_source("let o = {}; throw (o instanceof {});").expect("compiles");
    let thrown = expect_throw(&mut interp);
    let msg = interp.to_display_string(thrown);
    assert!(msg.contains("TypeError"), "expected TypeError, got {msg}");

    let mut interp2 =
        Interp::from_source("let o = {}; throw (o instanceof 123);").expect("compiles");
    let thrown2 = expect_throw(&mut interp2);
    let msg2 = interp2.to_display_string(thrown2);
    assert!(msg2.contains("TypeError"), "expected TypeError, got {msg2}");
}

#[test]
fn accessor_getter_returns_numeric_string() {
    // Bucket 10: accessor with numeric getter string "42" should return 42
    let mut interp = Interp::from_source("let x = 1;").expect("compiles");
    let obj = {
        let heap = interp.heap_mut_for_test();
        let o = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(v12_heap::JsValue::object(o));
        o
    };
    let (key, key_v, getter) = {
        let heap = interp.heap_mut_for_test();
        let s = heap.intern_string(v12_heap::V12Str::latin1(b"val".to_vec()));
        heap.add_root(v12_heap::JsValue::string(s));
        let key = v12_heap::PropKey::from_string(s);
        let key_v = v12_heap::JsValue::string(s);
        let g = heap.intern_string(v12_heap::V12Str::latin1(b"42".to_vec()));
        heap.add_root(v12_heap::JsValue::string(g));
        (key, key_v, g)
    };
    let shape = {
        let heap = interp.heap_mut_for_test();
        let s = heap.define_accessor(
            heap.root_shape(),
            key,
            Some(getter),
            None,
            v12_heap::Attrs::DEFAULT,
        );
        heap.add_shape_root(s);
        s
    };
    interp.bind_shape_for_test(obj, shape);
    // Need to push a dummy property for the accessor slot (hole)
    interp
        .heap_mut_for_test()
        .get_mut(obj)
        .properties
        .push(v12_heap::JsValue::hole());
    let obj_v = v12_heap::JsValue::object(obj);
    let got = interp.get_property_for_test(obj_v, key_v).expect("get");
    assert_eq!(got.as_smi(), Some(42));
}

#[test]
fn accessor_setter_is_noop_without_data_slot() {
    // Bucket 10: setting an accessor with a setter should not create a data slot
    let mut interp = Interp::from_source("let x = 1;").expect("compiles");
    let obj = {
        let heap = interp.heap_mut_for_test();
        let o = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(v12_heap::JsValue::object(o));
        o
    };
    let (key, key_v, getter, setter) = {
        let heap = interp.heap_mut_for_test();
        let s = heap.intern_string(v12_heap::V12Str::latin1(b"prop".to_vec()));
        heap.add_root(v12_heap::JsValue::string(s));
        let key = v12_heap::PropKey::from_string(s);
        let key_v = v12_heap::JsValue::string(s);
        let g = heap.intern_string(v12_heap::V12Str::latin1(b"10".to_vec()));
        let st = heap.intern_string(v12_heap::V12Str::latin1(b"setter_body".to_vec()));
        heap.add_root(v12_heap::JsValue::string(g));
        heap.add_root(v12_heap::JsValue::string(st));
        (key, key_v, g, st)
    };
    let shape = {
        let heap = interp.heap_mut_for_test();
        let s = heap.define_accessor(
            heap.root_shape(),
            key,
            Some(getter),
            Some(setter),
            v12_heap::Attrs::DEFAULT,
        );
        heap.add_shape_root(s);
        s
    };
    interp.bind_shape_for_test(obj, shape);
    interp
        .heap_mut_for_test()
        .get_mut(obj)
        .properties
        .push(v12_heap::JsValue::hole());
    let obj_v = v12_heap::JsValue::object(obj);
    // Set should invoke setter (no-op) and not change getter value
    let new_val = v12_heap::JsValue::from_i32_smi(99).unwrap();
    interp
        .set_property_for_test(obj_v, key_v, new_val)
        .expect("set");
    let got = interp.get_property_for_test(obj_v, key_v).expect("get");
    assert_eq!(got.as_smi(), Some(10));
}

#[test]
fn global_var_alias_for_captured_var() {
    // Bucket 8: top-level `var` that is captured should alias the global object
    let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
    let global = heap.alloc(v12_heap::JsObject::default());
    heap.add_root(v12_heap::JsValue::object(global));
    // Simulate global with 10 intrinsic slots already
    heap.get_mut(global)
        .properties
        .resize(10, v12_heap::JsValue::undefined());
    heap.add_root(v12_heap::JsValue::object(global));
    let src = "var x = 123; function f(){ return x; } throw f();";
    let (program, strings) = v12_bccompiler::compile_source_with_strings(src).expect("compile");
    let mut interp2 =
        Interp::new_with_heap(heap, Some(global), program.functions, program.main, strings);
    let thrown = expect_throw(&mut interp2);
    assert_eq!(thrown.as_smi(), Some(123));
    // Also verify global's property at offset holds the var value
    let heap = interp2.heap();
    let val = heap.get(global).properties[10];
    assert_eq!(val.as_smi(), Some(123));
}

#[test]
fn arguments_exotic_mapped_access_via_elements() {
    // Bucket 8: arguments object with mapped indices stores elements
    let mut interp = Interp::from_source("let x = 1;").expect("compiles");
    let args_obj = {
        let heap = interp.heap_mut_for_test();
        let mapped: Box<[Option<u32>]> = vec![Some(0), None].into_boxed_slice();
        let o = heap.alloc(v12_heap::JsObject {
            kind: v12_heap::KIND_ARGUMENTS,
            elements: vec![
                v12_heap::JsValue::from_i32_smi(7).unwrap(),
                v12_heap::JsValue::from_i32_smi(8).unwrap(),
            ],
            arguments_mapped: Some(mapped),
            ..Default::default()
        });
        heap.add_root(v12_heap::JsValue::object(o));
        o
    };
    let args_v = v12_heap::JsValue::object(args_obj);
    // Indexed access should return elements
    let key0 = v12_heap::JsValue::from_i32_smi(0).unwrap();
    let got0 = interp.get_property_for_test(args_v, key0).expect("get");
    assert_eq!(got0.as_smi(), Some(7));
    // Setting index 0 should update element
    let new_val = v12_heap::JsValue::from_i32_smi(99).unwrap();
    interp
        .set_property_for_test(args_v, key0, new_val)
        .expect("set");
    let got1 = interp
        .get_property_for_test(args_v, key0)
        .expect("get after set");
    assert_eq!(got1.as_smi(), Some(99));
    // Verify mapped flag is still present
    assert!(interp.heap().get(args_obj).arguments_mapped.is_some());
}

// ---------------------------------------------------------------------------
// `null` literal (Const::Null)
// ---------------------------------------------------------------------------

#[test]
fn null_literal_evaluates_to_js_null() {
    // `null` must be a distinct value from `undefined`.
    let mut interp = Interp::from_source("throw null;").expect("compiles");
    let v = expect_throw(&mut interp);
    assert!(
        v.is_null(),
        "expected JsValue::null(), got bits {:#x}",
        v.bits()
    );
    assert!(!v.is_undefined());
}

#[test]
fn typeof_null_is_object() {
    // ECMA-262: `typeof null` is `"object"` (legacy).
    let mut interp = Interp::from_source("throw typeof null;").expect("compiles");
    let v = expect_throw(&mut interp);
    assert!(v.is_string(), "typeof null must be a string");
    let text = interp.to_display_string(v);
    assert_eq!(text, "object");
}

#[test]
fn null_via_load_const_wide_evaluates_to_js_null() {
    // Exercise the WideOp::LoadConstW path for Const::Null.
    let mut pool = ConstantPool::new();
    let k_null = pool.insert(Const::Null).expect("fits");
    // Encode a wide load of the null constant into r0 then throw it.
    let mut instrs = WideOp::LoadConstW {
        dst: 0,
        const_id: u32::from(k_null),
    }
    .encode();
    instrs.push(Instr::new(Opcode::Throw, 0, 0, 0));
    let fb = empty_fn(1, instrs, pool);
    let mut interp = program_of(fb);
    let v = expect_throw(&mut interp);
    assert!(v.is_null(), "wide null must still be JsValue::null()");
}

#[test]
fn null_identity_and_strict_equality() {
    // `null === null` is true, `null == undefined` is true (loose), strict false.
    let mut interp = Interp::from_source("throw (null === null);").expect("compiles");
    assert_eq!(expect_throw(&mut interp).as_bool(), Some(true));
    let mut interp2 = Interp::from_source("throw (null == undefined);").expect("compiles");
    assert_eq!(expect_throw(&mut interp2).as_bool(), Some(true));
    let mut interp3 = Interp::from_source("throw (null === undefined);").expect("compiles");
    assert_eq!(expect_throw(&mut interp3).as_bool(), Some(false));
}
