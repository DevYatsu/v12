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
