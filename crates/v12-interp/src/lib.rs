#![forbid(unsafe_code)]

//! Tier-0 interpreter: a single iterative dispatch loop executing
//! [`v12_bccompiler::Program`] bytecode over one contiguous value stack.
//!
//! # Frame model
//!
//! All JavaScript activations share one geometrically grown `Vec<JsValue>`.
//! Each [`Frame`] windows a slice `[base, base + max_regs)` of that vector;
//! registers index from `0` inside the window and initialize to `undefined`,
//! with `r0` holding `this`. Calls never copy arguments: per the compiler's
//! ABI the caller lays out `[callee][this][arg…]` ending exactly at its own
//! window edge, so the callee reads `this`/arguments straight out of the
//! caller's tail. Parameters beyond the supplied argument count read as
//! `undefined`; surplus arguments are ignored.
//!
//! Recursion is bounded by [`MAX_CALL_DEPTH`] to fail fast with a catchable
//! `RangeError` instead of exhausting memory.
//!
//! # Environments and closures
//!
//! An environment is an ordinary heap object whose `properties` vector holds
//! the slots and whose prototype link points at the *enclosing* environment.
//! Because the collector traces prototypes strongly, any environment
//! reachable from a rooted closure keeps its whole chain alive. `Closure`
//! captures the current frame's environment; `NewEnvironment` splices a fresh
//! object in front of it. The static hop count carried in the
//! `NewEnvironment` operand duplicates what the dynamic chain already
//! encodes, so the parent is simply the captured environment.
//!
//! Function objects store their program function index as element slot 0 and
//! their captured environment as the prototype link; [`KIND_FUNCTION`] marks
//! them. Indices at or beyond [`Program::functions`] route to the
//! [`NativeRegistry`] seam instead of bytecode.
//!
//! # Exceptions
//!
//! A thrown value unwinds through the handler tables: the innermost handler
//! covering the current pc wins. Unwinding truncates the frame window to the
//! handler's `stack_depth`, delivers the exception value into register
//! `stack_depth`, and jumps to the handler target. Frames without a matching
//! handler pop; an exception escaping the top-level frame leaves `run` as
//! [`Err(JSException)`](JSException).
//!
//! # Garbage collection
//!
//! Allocation only happens inside `Heap::alloc`, so before every opcode that
//! can allocate, the interpreter republishes the live value stack plus every
//! active environment as GC roots ([`Interp::gc_protect`]). Shapes created by
//! property stores are pinned explicitly ([`Interp::publish_shape`]):
//! objects carry no shape handles, so nothing else anchors divergent
//! transition branches against collection.
//!
//! Object→shape association lives in a side table keyed by the object's
//! validity cell ([`Interp::shape_of`]). Validity cells are assigned lazily,
//! unique to a living object, and reset when a slot is freed — a reused
//! object handle therefore cannot alias a stale entry.

mod feedback;
mod ops;

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use v12_bytecode::{Const, FunctionBytecode, Opcode, WideOp};
use v12_heap::{
    Attrs, Descriptor, GcPolicy, Handle, Heap, JsObject, JsValue, PropKey, ShapeHandle, V12Str,
};

use crate::feedback::{FeedbackVector, MonoIc, TYPE_NAME_COUNT, TYPE_NAMES, TierHooks};

/// Object kind for user functions created by `Closure`.
///
/// Kind values are engine-assigned; these two must stay distinct from the
/// heap's [`v12_heap::KIND_ORDINARY`] and from each other.
pub const KIND_FUNCTION: u8 = 1;

/// Object kind for array literals created by `NewArray`; canonical integer
/// keys on arrays route through the element store instead of named shapes.
pub const KIND_ARRAY: u8 = 2;

/// Maximum simultaneous JavaScript activations.
///
/// Why a limit exists: the dispatch loop is iterative, so recursion costs
/// heap (frames plus register windows), not native stack — an unbounded
/// `function f() { return f(); }` would otherwise OOM the process instead of
/// failing the script. 10 000 frames sits orders of magnitude above any
/// legitimate Tier-1 program while capping worst-case memory at a few
/// megabytes of stack slots; mainstream engines converge on the same order.
const MAX_CALL_DEPTH: usize = 10_000;

/// Initial capacity reserved for the shared value stack and root set, sized
/// to absorb typical bring-up workloads before geometric growth kicks in.
const INITIAL_STACK_CAPACITY: usize = 4 * 1024;

/// A runtime error carrying the ready-to-throw JavaScript value.
///
/// Thrown strings follow the `"TypeError: …"` / `"RangeError: …"` spelling
/// convention so embedders can classify them textually until error-object
/// kinds exist.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct JSException(pub JsValue);

impl std::fmt::Debug for JSException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Bits, not text: rendering requires the heap, which `Debug` cannot
        // take. `Interp::to_display_string` is the human-readable path.
        f.debug_tuple("JSException").field(&self.0.bits()).finish()
    }
}

impl From<JsValue> for JSException {
    fn from(v: JsValue) -> Self {
        JSException(v)
    }
}

/// Seam for host-provided native functions.
///
/// A call whose function index lies beyond [`Program::functions`] denotes a
/// native: the interpreter hands the receiver, arguments, and heap to the
/// registry and takes back the result or the value to throw. The default
/// registry is empty — every native index throws `TypeError` — so programs
/// compiled without built-ins behave identically whether or not a registry
/// is wired in.
pub trait NativeRegistry {
    /// Executes native function `index`. `args` excludes the receiver.
    fn call_native(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        index: u32,
    ) -> Result<JsValue, JsValue>;
}

/// The default [`NativeRegistry`]: no natives exist.
#[derive(Default)]
pub struct EmptyNativeRegistry;

impl NativeRegistry for EmptyNativeRegistry {
    fn call_native(
        &mut self,
        heap: &mut Heap,
        _this: JsValue,
        _args: &[JsValue],
        index: u32,
    ) -> Result<JsValue, JsValue> {
        Err(JsValue::string(intern_text(
            heap,
            &format!("TypeError: native function #{index} is not registered"),
        )))
    }
}

/// Interns `text` as a canonical heap string (deduplicated by content, so
/// equal texts share one handle — property-key identity relies on this).
pub(crate) fn intern_text(heap: &mut Heap, text: &str) -> Handle<V12Str> {
    let s = if text.is_ascii() {
        V12Str::latin1(text.as_bytes().to_vec())
    } else {
        V12Str::utf16(text.encode_utf16().collect())
    };
    heap.intern_string(s)
}

/// Outcome of preparing a call.
enum CallOutcome {
    /// A bytecode frame was pushed; dispatch continues into it.
    Pushed,
    /// The call completed inline (native path); the value is the result.
    Value(JsValue),
}

/// One JavaScript activation: a function body, its register window on the
/// shared stack, its pc, and the head of its environment chain.
struct Frame {
    fn_idx: u32,
    /// Absolute bytecode pc of the next instruction. On entry to a handler
    /// this is reset to the handler target; a completing call advances the
    /// caller's pc past its `Call` header.
    pc: usize,
    base: usize,
    max_regs: u16,
    /// Innermost environment object (`None` until `NewEnvironment` runs).
    /// Kept here rather than derived so closure capture has one source.
    env: Option<Handle<JsObject>>,
}

/// The Tier-0 interpreter over one compiled program's bytecode.
///
/// Deliberately decoupled from the compiler crate: callers pass the
/// function table, the entry index, and the string table that
/// `Const::Str32` ids resolve through (as produced by
/// `v12_bccompiler::compile_source_with_strings`).
pub struct Interp {
    functions: std::sync::Arc<[FunctionBytecode]>,
    main: u32,
    /// Compiler string table: `Const::Str32` ids resolve through this.
    strings: std::sync::Arc<[String]>,
    heap: Heap,

    /// Interned heap string per `Str32` constant id, filled lazily.
    const_strings: std::collections::HashMap<u32, Handle<V12Str>>,
    /// Interned `typeof` names, lazily filled in [`TYPE_NAMES`] order.
    typeof_names: [Option<Handle<V12Str>>; TYPE_NAME_COUNT],
    /// Cached property key for the array `length` property.
    length_key: Option<PropKey>,
    /// Cached `root --length--> child` shape shared by every array.
    length_shape: Option<ShapeHandle>,
    /// Shape indexes already pinned via `add_shape_root` (pinning is
    /// idempotent-averse: repeated pins would grow the root vector forever).
    pinned_shapes: HashSet<u32>,
    /// Object shape association keyed by validity-cell id; see crate docs.
    shape_of_cell: std::collections::HashMap<u32, ShapeHandle>,

    /// The one contiguous value stack; frames window slices of it.
    stack: Vec<JsValue>,
    frames: Vec<Frame>,

    natives: Box<dyn NativeRegistry>,
    hooks: Box<dyn TierHooks>,
    /// Per-function execution feedback, allocated on first observation.
    feedback: std::collections::HashMap<u32, FeedbackVector>,
    /// Functions that crossed the tier-up threshold since the last drain.
    tier_up_pending: Vec<u32>,
}

impl Interp {
    /// Builds an interpreter over `program`, resolving `Const::Str32` ids
    /// against `strings` (as produced by
    /// `v12_bccompiler::compile_source_with_strings`).
    /// Builds an interpreter over a compiled program, resolving
    /// `Const::Str32` ids against `strings` (as produced by
    /// `v12_bccompiler::compile_source_with_strings`).
    /// Builds an interpreter over a compiled program, resolving
    /// `Const::Str32` ids against `strings` (as produced by
    /// `v12_bccompiler::compile_source_with_strings`).
    pub fn new(functions: Vec<FunctionBytecode>, main: u32, strings: Vec<String>) -> Self {
        let mut heap = Heap::new(GcPolicy::default());
        heap.roots_mut().0.reserve(INITIAL_STACK_CAPACITY);
        Self {
            functions: std::sync::Arc::from(functions.into_boxed_slice()),
            main,
            strings: std::sync::Arc::from(strings.into_boxed_slice()),
            heap,
            const_strings: std::collections::HashMap::new(),
            typeof_names: [const { None }; TYPE_NAME_COUNT],
            length_key: None,
            length_shape: None,
            pinned_shapes: HashSet::new(),
            shape_of_cell: std::collections::HashMap::new(),
            stack: Vec::with_capacity(INITIAL_STACK_CAPACITY),
            frames: Vec::new(),
            natives: Box::new(EmptyNativeRegistry),
            hooks: Box::new(()),
            feedback: std::collections::HashMap::new(),
            tier_up_pending: Vec::new(),
        }
    }

    /// Convenience constructor: compiles `source` and resolves its string
    /// table in one step.
    pub fn from_source(source: &str) -> Result<Self, v12_bccompiler::CompileError> {
        let (program, strings) = v12_bccompiler::compile_source_with_strings(source)?;
        Ok(Self::new(program.functions, program.main, strings))
    }

    /// Installs a native-function seam, replacing any previous registry.
    pub fn set_natives(&mut self, natives: Box<dyn NativeRegistry>) {
        self.natives = natives;
    }

    /// Installs tier-transition hooks invoked between frame completions.
    pub fn set_hooks(&mut self, hooks: Box<dyn TierHooks>) {
        self.hooks = hooks;
    }

    /// Read-only view of the underlying heap.
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    #[cfg(test)]
    pub(crate) fn heap_mut_for_test(&mut self) -> &mut Heap {
        &mut self.heap
    }

    #[cfg(test)]
    pub fn functions_mut_for_test(&mut self) -> &mut [FunctionBytecode] {
        std::sync::Arc::make_mut(&mut self.functions)
    }

    /// Runs the top-level script to completion.
    ///
    /// `Ok(())` on normal completion; [`Err(JSException)`] when a thrown
    /// value escaped every handler.
    pub fn run(&mut self) -> Result<(), JSException> {
        let main_regs =
            self.functions[usize::try_from(self.main).expect("function index fits usize")].max_regs;
        debug_assert!(self.frames.is_empty(), "run() is not reentrant");
        self.stack.clear();
        self.stack
            .resize(usize::from(main_regs), JsValue::undefined());
        self.frames.push(Frame {
            fn_idx: self.main,
            pc: 0,
            base: 0,
            max_regs: main_regs,
            env: None,
        });
        self.note_entry(self.main);
        self.execute()
    }

    /// Applies ES `ToString` from outside the machine — diagnostics and test
    /// harnesses, not executable semantics.
    pub fn to_display_string(&mut self, v: JsValue) -> String {
        match ops::to_js_string(&mut self.heap, v) {
            Ok(h) => {
                let units = ops::string_units(&mut self.heap, h);
                String::from_utf16_lossy(&units)
            }
            Err(_) => "<unprintable>".into(),
        }
    }

    // ------------------------------------------------------------------
    // Dispatch loop
    // ------------------------------------------------------------------

    /// The single iterative dispatch loop. Each arm either advances the
    /// current frame's pc, redirects it, pushes/pops frames, or raises a
    /// pending exception for delivery at the top of the next iteration.
    fn execute(&mut self) -> Result<(), JSException> {
        // Thrown value awaiting delivery to a handler (or escape).
        let mut pending: Option<JsValue> = None;

        'drive: loop {
            if let Some(exc) = pending.take() {
                // Either lands in a handler (pc rewritten, value delivered)
                // or pops frames; escaping the bottom frame ends the run.
                self.unwind(exc)?;
            }

            // Snapshot hot frame state: arms call back into `self` and must
            // not hold borrows across those calls.
            let (fn_idx, pc, base, max_regs) = {
                let f = self.frames.last().expect("execute() requires a frame");
                (f.fn_idx, f.pc, f.base, f.max_regs)
            };

            // Falling off the instruction stream is implicit `return
            // undefined` (the documented completion ABI).
            let Some(&instr) = self.functions[fn_idx as usize].instrs.get(pc) else {
                if self.complete_frame(JsValue::undefined())? {
                    return Ok(());
                }
                continue 'drive;
            };
            let Some(op) = instr.op() else {
                panic!("corrupt bytecode: unassigned opcode byte at {fn_idx}:{pc}");
            };

            macro_rules! throw_js {
                ($v:expr) => {{
                    pending = Some($v);
                    continue 'drive;
                }};
            }
            macro_rules! attempt {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(JSException(v)) => throw_js!(v),
                    }
                };
            }

            match op {
                // ------------------------------------------------------
                // Data movement and constants
                // ------------------------------------------------------
                Opcode::Move => {
                    self.stack[base + usize::from(instr.a())] =
                        self.stack[base + usize::from(instr.b())];
                    self.set_pc(pc + 1);
                }
                Opcode::LoadInt => {
                    let v = i8::from_be_bytes([instr.c()]);
                    self.stack[base + usize::from(instr.a())] = ops::box_number(f64::from(v));
                    self.set_pc(pc + 1);
                }
                Opcode::LoadConst => {
                    let value = attempt!(self.const_value(fn_idx, u32::from(instr.imm16())));
                    self.stack[base + usize::from(instr.a())] = value;
                    self.set_pc(pc + 1);
                }
                Opcode::Wide => {
                    let words = &self.functions[fn_idx as usize].instrs[pc..];
                    let (wide, width) =
                        WideOp::try_decode(words).expect("malformed wide opcode sequence");
                    match wide {
                        WideOp::LoadIntW { dst, value } => {
                            // i64 → f64 is lossy past 2⁵³; identical to the
                            // reference behavior for the constants emitted.
                            self.stack[base + usize::from(dst)] = ops::box_number(value as f64);
                        }
                        WideOp::LoadConstW { dst, const_id } => {
                            let value = attempt!(self.const_value(fn_idx, const_id));
                            self.stack[base + usize::from(dst)] = value;
                        }
                        WideOp::GetEnvSlotW { dst, depth, slot } => {
                            let v = attempt!(self.env_read(depth, slot));
                            self.stack[base + usize::from(dst)] = v;
                        }
                        WideOp::SetEnvSlotW { src, depth, slot } => {
                            let v = self.stack[base + usize::from(src)];
                            attempt!(self.env_write(depth, slot, v));
                        }
                        WideOp::CallW { dst, func, argc } => {
                            match attempt!(self.prepare_call(base, max_regs, func, argc)) {
                                CallOutcome::Pushed => continue 'drive,
                                CallOutcome::Value(v) => {
                                    let caller_base = base;
                                    self.stack[caller_base + usize::from(dst)] = v;
                                    self.set_pc(pc + 2);
                                }
                            }
                            continue 'drive;
                        }
                    }
                    self.set_pc(pc + width);
                }

                // ------------------------------------------------------
                // Arithmetic
                // ------------------------------------------------------
                Opcode::Add => {
                    let l = self.stack[base + usize::from(instr.b())];
                    let r = self.stack[base + usize::from(instr.c())];
                    self.gc_protect();
                    let v = attempt!(ops::add(&mut self.heap, l, r));
                    self.stack[base + usize::from(instr.a())] = v;
                    self.set_pc(pc + 1);
                }
                Opcode::Sub => {
                    let l = self.stack[base + usize::from(instr.b())];
                    let r = self.stack[base + usize::from(instr.c())];
                    self.stack[base + usize::from(instr.a())] = ops::sub(&mut self.heap, l, r);
                    self.set_pc(pc + 1);
                }
                Opcode::Mul => {
                    let l = self.stack[base + usize::from(instr.b())];
                    let r = self.stack[base + usize::from(instr.c())];
                    self.stack[base + usize::from(instr.a())] = ops::mul(&mut self.heap, l, r);
                    self.set_pc(pc + 1);
                }
                Opcode::Div | Opcode::Mod | Opcode::Pow => {
                    let l = self.stack[base + usize::from(instr.b())];
                    let r = self.stack[base + usize::from(instr.c())];
                    let n = match op {
                        Opcode::Div => ops::div(&mut self.heap, l, r),
                        Opcode::Mod => ops::modulo(&mut self.heap, l, r),
                        _ => ops::js_pow(&mut self.heap, l, r),
                    };
                    self.stack[base + usize::from(instr.a())] = n;
                    self.set_pc(pc + 1);
                }

                // ------------------------------------------------------
                // Bitwise operations and shifts (ES ToInt32/ToUint32)
                // ------------------------------------------------------
                Opcode::BitAnd | Opcode::BitOr | Opcode::BitXor => {
                    let ln =
                        ops::to_number(&mut self.heap, self.stack[base + usize::from(instr.b())]);
                    let rn =
                        ops::to_number(&mut self.heap, self.stack[base + usize::from(instr.c())]);
                    let (a, b) = (ops::to_int32(ln), ops::to_int32(rn));
                    let n = match op {
                        Opcode::BitAnd => a & b,
                        Opcode::BitOr => a | b,
                        _ => a ^ b,
                    };
                    self.stack[base + usize::from(instr.a())] = ops::box_number(f64::from(n));
                    self.set_pc(pc + 1);
                }
                Opcode::Shl | Opcode::Shr | Opcode::UShr => {
                    let ln =
                        ops::to_number(&mut self.heap, self.stack[base + usize::from(instr.b())]);
                    let rn =
                        ops::to_number(&mut self.heap, self.stack[base + usize::from(instr.c())]);
                    let shift = ops::to_uint32(rn) & 31;
                    let n = match op {
                        Opcode::Shl => ops::to_int32(ln) << shift,
                        Opcode::Shr => ops::to_int32(ln) >> shift,
                        // Unsigned shift reinterprets the int32 bits as u32.
                        _ => (ops::to_int32(ln) as u32 >> shift) as i32,
                    };
                    self.stack[base + usize::from(instr.a())] = ops::box_number(f64::from(n));
                    self.set_pc(pc + 1);
                }

                // ------------------------------------------------------
                // Equality, comparison, unary operators
                // ------------------------------------------------------
                Opcode::Eq | Opcode::Ne => {
                    let l = self.stack[base + usize::from(instr.b())];
                    let r = self.stack[base + usize::from(instr.c())];
                    let eq = ops::loose_equals(&mut self.heap, l, r);
                    self.write_bool(base, instr.a(), eq ^ (op == Opcode::Ne));
                    self.set_pc(pc + 1);
                }
                Opcode::StrictEq | Opcode::StrictNe => {
                    let l = self.stack[base + usize::from(instr.b())];
                    let r = self.stack[base + usize::from(instr.c())];
                    let eq = ops::strict_equals(&self.heap, l, r);
                    self.write_bool(base, instr.a(), eq ^ (op == Opcode::StrictNe));
                    self.set_pc(pc + 1);
                }
                Opcode::Lt | Opcode::Le | Opcode::Gt | Opcode::Ge => {
                    let l = self.stack[base + usize::from(instr.b())];
                    let r = self.stack[base + usize::from(instr.c())];
                    let ord = ops::compare(op, &mut self.heap, l, r);
                    self.write_bool(base, instr.a(), ord);
                    self.set_pc(pc + 1);
                }
                Opcode::Neg => {
                    let n =
                        -ops::to_number(&mut self.heap, self.stack[base + usize::from(instr.b())]);
                    self.stack[base + usize::from(instr.a())] = ops::box_number(n);
                    self.set_pc(pc + 1);
                }
                Opcode::BitNot => {
                    let n =
                        ops::to_number(&mut self.heap, self.stack[base + usize::from(instr.b())]);
                    self.stack[base + usize::from(instr.a())] =
                        ops::box_number(f64::from(!ops::to_int32(n)));
                    self.set_pc(pc + 1);
                }
                Opcode::Not => {
                    let truthy =
                        ops::to_boolean(&self.heap, self.stack[base + usize::from(instr.b())]);
                    self.write_bool(base, instr.a(), !truthy);
                    self.set_pc(pc + 1);
                }
                Opcode::TypeOf => {
                    let v = self.stack[base + usize::from(instr.b())];
                    self.gc_protect();
                    let tag = self.type_tag(v);
                    let name = attempt!(self.typeof_name(tag));
                    self.stack[base + usize::from(instr.a())] = JsValue::string(name);
                    self.set_pc(pc + 1);
                }

                // ------------------------------------------------------
                // Control flow
                // ------------------------------------------------------
                Opcode::Jump => {
                    self.set_pc(instr.imm24() as usize);
                }
                Opcode::JumpIfFalse | Opcode::JumpIfTrue => {
                    let truthy =
                        ops::to_boolean(&self.heap, self.stack[base + usize::from(instr.a())]);
                    let taken = truthy ^ (op == Opcode::JumpIfFalse);
                    self.set_pc(if taken {
                        usize::from(instr.imm16())
                    } else {
                        pc + 1
                    });
                }
                Opcode::LoopHeader => {
                    self.note_loop(fn_idx);
                    self.set_pc(pc + 1);
                }

                // ------------------------------------------------------
                // Calls, returns, throws
                // ------------------------------------------------------
                Opcode::Call => {
                    let argc = u16::from(instr.c());
                    match attempt!(self.prepare_call(base, max_regs, instr.b(), argc)) {
                        CallOutcome::Pushed => continue 'drive,
                        CallOutcome::Value(v) => {
                            self.stack[base + usize::from(instr.a())] = v;
                            self.set_pc(pc + 1);
                        }
                    }
                    continue 'drive;
                }
                Opcode::Return => {
                    let v = self.stack[base + usize::from(instr.a())];
                    if self.complete_frame(v)? {
                        return Ok(());
                    }
                    continue 'drive;
                }
                Opcode::Throw => {
                    throw_js!(self.stack[base + usize::from(instr.a())]);
                }

                // ------------------------------------------------------
                // Property access
                // ------------------------------------------------------
                Opcode::GetProperty => {
                    let obj_v = self.stack[base + usize::from(instr.b())];
                    let key_v = self.stack[base + usize::from(instr.c())];
                    self.gc_protect();
                    let v = attempt!(self.get_property(fn_idx, pc as u32, obj_v, key_v));
                    self.stack[base + usize::from(instr.a())] = v;
                    self.set_pc(pc + 1);
                }
                Opcode::SetProperty => {
                    let obj_v = self.stack[base + usize::from(instr.a())];
                    let key_v = self.stack[base + usize::from(instr.b())];
                    let value = self.stack[base + usize::from(instr.c())];
                    self.gc_protect();
                    attempt!(self.set_property(obj_v, key_v, value));
                    self.set_pc(pc + 1);
                }
                Opcode::DeleteProperty => {
                    let obj_v = self.stack[base + usize::from(instr.b())];
                    let key_v = self.stack[base + usize::from(instr.c())];
                    let deleted = attempt!(self.delete_property(obj_v, key_v));
                    self.write_bool(base, instr.a(), deleted);
                    self.set_pc(pc + 1);
                }

                // ------------------------------------------------------
                // Allocation and environments
                // ------------------------------------------------------
                Opcode::NewObject => {
                    self.gc_protect();
                    let h = self.heap.alloc(JsObject::default());
                    self.stack[base + usize::from(instr.a())] = JsValue::object(h);
                    self.set_pc(pc + 1);
                }
                Opcode::NewArray => {
                    let first = base + usize::from(instr.b());
                    let len = usize::from(instr.c());
                    self.gc_protect();
                    let elements = self.stack[first..first + len].to_vec();
                    let shape = self.array_shape();
                    let h = self.heap.alloc(JsObject {
                        kind: KIND_ARRAY,
                        properties: vec![ops::box_number(f64::from(len as u32))],
                        elements,
                        ..JsObject::default()
                    });
                    // Publish the shape onto the fresh object before anything
                    // else can allocate (the shape is pinned in
                    // `array_shape`, satisfying the allocation contract).
                    self.bind_shape(h, shape);
                    self.stack[base + usize::from(instr.a())] = JsValue::object(h);
                    self.set_pc(pc + 1);
                }
                Opcode::Closure => {
                    self.gc_protect();
                    let env = self.frames.last().expect("frame").env;
                    let h = self.heap.alloc(JsObject {
                        kind: KIND_FUNCTION,
                        // Element slot 0 carries the program function index;
                        // the prototype link doubles as the captured
                        // environment so the collector traces the chain.
                        elements: vec![ops::box_number(f64::from(instr.b()))],
                        prototype: env,
                        ..JsObject::default()
                    });
                    self.stack[base + usize::from(instr.a())] = JsValue::object(h);
                    self.set_pc(pc + 1);
                }
                Opcode::NewEnvironment => {
                    let slots = usize::from(instr.b());
                    self.gc_protect();
                    let parent = self.frames.last().expect("frame").env;
                    let h = self.heap.alloc(JsObject {
                        properties: vec![JsValue::undefined(); slots],
                        prototype: parent,
                        ..JsObject::default()
                    });
                    self.frames.last_mut().expect("frame").env = Some(h);
                    self.set_pc(pc + 1);
                }
                Opcode::GetEnvSlot => {
                    let v = attempt!(self.env_read(u16::from(instr.b()), u16::from(instr.c())));
                    self.stack[base + usize::from(instr.a())] = v;
                    self.set_pc(pc + 1);
                }
                Opcode::SetEnvSlot => {
                    let v = self.stack[base + usize::from(instr.c())];
                    attempt!(self.env_write(u16::from(instr.a()), u16::from(instr.b()), v));
                    self.set_pc(pc + 1);
                }

                // Generators and async remain unimplemented end to end: the
                // compiler rejects those constructs, so reaching one of these
                // bytes is a contract breach, not a runtime condition.
                Opcode::CreateGenerator | Opcode::SuspendYield | Opcode::Await => {
                    panic!("generator/async opcodes are unreachable in compiled programs")
                }
            }
        }
    }

    fn set_pc(&mut self, pc: usize) {
        self.frames
            .last_mut()
            .expect("dispatch requires a frame")
            .pc = pc;
    }

    fn write_bool(&mut self, base: usize, reg: u8, b: bool) {
        self.stack[base + usize::from(reg)] = if b {
            JsValue::true_()
        } else {
            JsValue::false_()
        };
    }

    // ------------------------------------------------------------------
    // Constants
    // ------------------------------------------------------------------

    fn const_value(&mut self, fn_idx: u32, id: u32) -> Result<JsValue, JSException> {
        let konst = self.functions[fn_idx as usize]
            .consts
            .get(id as u16)
            .unwrap_or_else(|| panic!("constant k{id} out of range in fn {fn_idx}"));
        match konst {
            Const::F64(v) => Ok(ops::box_number(v)),
            Const::Str32(str_id) => {
                if let Some(&h) = self.const_strings.get(&str_id) {
                    return Ok(JsValue::string(h));
                }
                self.gc_protect();
                let text = self
                    .strings
                    .get(str_id as usize)
                    .unwrap_or_else(|| panic!("Str32({str_id}) missing from the string table"));
                let h = intern_text(&mut self.heap, text);
                self.const_strings.insert(str_id, h);
                Ok(JsValue::string(h))
            }
            // BigInt literals are rejected at compile time today; reaching
            // these variants implies hand-built bytecode.
            Const::BigIntId(_) | Const::BigU64(_) => {
                Err(JSException(JsValue::string(intern_text(
                    &mut self.heap,
                    "InternalError: BigInt constants are not supported yet",
                ))))
            }
        }
    }

    fn typeof_name(&mut self, tag: usize) -> Result<Handle<V12Str>, JSException> {
        if let Some(h) = self.typeof_names[tag] {
            return Ok(h);
        }
        let h = intern_text(&mut self.heap, TYPE_NAMES[tag]);
        self.typeof_names[tag] = Some(h);
        Ok(h)
    }

    /// `typeof` classification, indexing [`TYPE_NAMES`].
    fn type_tag(&self, v: JsValue) -> usize {
        if v.is_undefined() {
            0
        } else if v.is_boolean() {
            1
        } else if v.is_smi() || v.is_f64() {
            2
        } else if v.is_string() {
            3
        } else if v.is_bigint() {
            4
        } else if v.is_symbol() {
            5
        } else if v.is_null() || v.is_object() {
            // null historically types as "object"; functions split out here.
            let function = v
                .as_object()
                .is_some_and(|h| self.heap.get(h).kind == KIND_FUNCTION);
            if function { 7 } else { 6 }
        } else {
            panic!("non-canonical value cannot be typed: {:#x}", v.bits())
        }
    }

    // ------------------------------------------------------------------
    // Calls
    // ------------------------------------------------------------------

    /// Resolves `[callee][this][args…]` at `callee_reg` in the current frame
    /// and either pushes a bytecode frame or completes a native inline.
    fn prepare_call(
        &mut self,
        base: usize,
        caller_max_regs: u16,
        callee_reg: u8,
        argc: u16,
    ) -> Result<CallOutcome, JSException> {
        let callee_slot = base + usize::from(callee_reg);
        let callee_v = self.stack[callee_slot];
        let this_v = self.stack[callee_slot + 1];

        let Some(callee_obj) = callee_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: callee is not a function"),
            ));
        };
        if self.heap.get(callee_obj).kind != KIND_FUNCTION {
            return Err(JSException(
                self.error_value("TypeError: callee is not a function"),
            ));
        }
        let (target, captured_env) = {
            let c = self.heap.get(callee_obj);
            let idx = c.elements.first().and_then(|v| v.as_smi()).unwrap_or(-1);
            (idx, c.prototype)
        };
        if target < 0 {
            return Err(JSException(
                self.error_value("InternalError: malformed function object"),
            ));
        }
        let target = u32::try_from(target).expect("checked non-negative");

        // Indices beyond the compiled program route to the native seam.
        if (target as usize) >= self.functions.len() {
            let args_start = callee_slot + 2;
            let args_end = args_start + usize::from(argc);
            self.gc_protect();
            let result = {
                let args = &self.stack[args_start..args_end];
                // Disjoint field borrows: heap + natives mut, stack immut.
                // Borrow checker allows distinct fields in 2024 edition.
                self.natives.call_native(&mut self.heap, this_v, args, target)
            };
            return result.map(CallOutcome::Value).map_err(JSException);
        }

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }

        let callee_fn = &self.functions[target as usize];
        let new_base = base + usize::from(caller_max_regs);
        let window_end = new_base + usize::from(callee_fn.max_regs);

        // Extending the stack never moves existing slots, so the caller-tail
        // arguments stay valid while being copied into r1..
        let arg_src = callee_slot + 2;
        self.stack.resize(window_end, JsValue::undefined());
        self.stack[new_base] = this_v;
        let copied = usize::from(argc).min(usize::from(callee_fn.max_regs).saturating_sub(1));
        for i in 0..copied {
            self.stack[new_base + 1 + i] = self.stack[arg_src + i];
        }

        self.frames.push(Frame {
            fn_idx: target,
            pc: 0,
            base: new_base,
            max_regs: callee_fn.max_regs,
            env: captured_env,
        });
        self.note_entry(target);
        Ok(CallOutcome::Pushed)
    }

    /// Completes the top frame with `result`: deposits it into the caller's
    /// destination register and resumes there. Returns `true` when the
    /// completed frame was the top-level script — the run is done.
    fn complete_frame(&mut self, result: JsValue) -> Result<bool, JSException> {
        let finished = self.frames.pop().expect("complete_frame requires a frame");
        self.notify_tier_ups();
        let Some(caller) = self.frames.last_mut() else {
            self.stack.truncate(finished.base);
            return Ok(true);
        };
        // The caller is parked on its Call/Wide header; step past it and
        // deposit the result into the destination register recorded there.
        let call_instr = self.functions[caller.fn_idx as usize].instrs[caller.pc];
        let width = usize::from(call_instr.op() == Some(Opcode::Wide));
        let dst = usize::from(call_instr.a());
        let caller_base = caller.base;
        self.stack.truncate(finished.base);
        self.stack[caller_base + dst] = result;
        caller.pc += 1 + width;
        Ok(false)
    }

    /// Delivers `exc` to the innermost applicable handler, popping frames
    /// until one accepts. Escaping the bottom frame returns `Err` to `run`.
    fn unwind(&mut self, exc: JsValue) -> Result<(), JSException> {
        loop {
            let covering = self.frames.last().and_then(|frame| {
                self.functions[frame.fn_idx as usize]
                    .handlers
                    .iter()
                    .filter(|h| {
                        usize::try_from(h.start).expect("handler pc fits usize") <= frame.pc
                            && frame.pc < usize::try_from(h.end).expect("handler pc fits usize")
                    })
                    .max_by_key(|h| h.start)
            });
            if let Some(h) = covering {
                let fr = self.frames.last_mut().expect("a frame was just inspected");
                // Truncate the register window to the handler depth, then
                // deliver the exception into register `stack_depth`.
                self.stack.truncate(fr.base + h.stack_depth as usize);
                self.stack.push(exc);
                fr.pc = h.target as usize;
                return Ok(());
            }
            let Some(popped) = self.frames.pop() else {
                return Err(JSException(exc));
            };
            self.stack.truncate(popped.base);
            self.notify_tier_ups();
            if self.frames.is_empty() {
                return Err(JSException(exc));
            }
        }
    }

    /// Materializes an interned error string as a throwable value.
    fn error_value(&mut self, text: &str) -> JsValue {
        JsValue::string(intern_text(&mut self.heap, text))
    }

    // ------------------------------------------------------------------
    // Environments
    // ------------------------------------------------------------------

    fn env_read(&mut self, depth: u16, slot: u16) -> Result<JsValue, JSException> {
        let env = self.walk_env(depth);
        let len = self.heap.get(env).properties.len();
        if usize::from(slot) < len {
            Ok(self.heap.get(env).properties[slot as usize])
        } else {
            Err(JSException(self.error_value(
                "InternalError: environment slot out of range",
            )))
        }
    }

    fn env_write(&mut self, depth: u16, slot: u16, v: JsValue) -> Result<(), JSException> {
        let env = self.walk_env(depth);
        let len = self.heap.get(env).properties.len();
        if usize::from(slot) < len {
            self.heap.get_mut(env).properties[slot as usize] = v;
            Ok(())
        } else {
            Err(JSException(self.error_value(
                "InternalError: environment slot out of range",
            )))
        }
    }

    /// Walks `depth` parent links from the current frame's environment.
    /// Out-of-chain depths indicate broken bytecode, hence the panics.
    fn walk_env(&self, depth: u16) -> Handle<JsObject> {
        let mut cur = self
            .frames
            .last()
            .expect("environment opcode outside any frame")
            .env
            .expect("environment opcode executed without an environment");
        for _ in 0..depth {
            cur = self
                .heap
                .get(cur)
                .prototype
                .expect("environment depth exceeds the live chain");
        }
        cur
    }

    // ------------------------------------------------------------------
    // Property access
    // ------------------------------------------------------------------

    /// The shape describing `obj`: looked up by the object's validity cell,
    /// defaulting to the pinned empty-object root.
    fn shape_of(&mut self, obj: Handle<JsObject>) -> ShapeHandle {
        let cell = self.heap.validity_cell_of(obj);
        self.shape_of_cell
            .get(&cell.0)
            .copied()
            .unwrap_or_else(|| self.heap.root_shape())
    }

    /// Records `obj`'s shape. Pinning happened (or happens) via
    /// [`Self::pin_shape`]; this only binds the association.
    fn bind_shape(&mut self, obj: Handle<JsObject>, shape: ShapeHandle) {
        self.pin_shape(shape);
        let cell = self.heap.validity_cell_of(obj);
        self.shape_of_cell.insert(cell.0, shape);
    }

    /// Anchors `shape` against collection exactly once. Transition edges are
    /// untraced, so an unpinned shape dies even while objects descended from
    /// it live; pinning trades that hazard for bounded metadata growth.
    fn pin_shape(&mut self, shape: ShapeHandle) {
        if self.pinned_shapes.insert(shape.index()) {
            self.heap.add_shape_root(shape);
        }
    }

    /// `root --key--> child` transition, pinned immediately.
    fn named_child_shape(&mut self, key: PropKey) -> ShapeHandle {
        let child = self
            .heap
            .add_property(self.heap.root_shape(), key, Attrs::DEFAULT);
        self.pin_shape(child);
        child
    }

    fn length_key(&mut self) -> PropKey {
        if let Some(k) = self.length_key {
            return k;
        }
        let h = intern_text(&mut self.heap, "length");
        let k = PropKey::from_string(h);
        self.length_key = Some(k);
        k
    }

    fn array_shape(&mut self) -> ShapeHandle {
        if let Some(s) = self.length_shape {
            return s;
        }
        let k = self.length_key();
        let s = self.named_child_shape(k);
        self.length_shape = Some(s);
        s
    }

    /// Canonical array index for a key value: unsigned small integers (Smi or
    /// integral double) or their decimal-string spellings.
    fn array_index_of(&mut self, key_v: JsValue) -> Option<u32> {
        if let Some(n) = key_v.as_smi() {
            return u32::try_from(n).ok();
        }
        if let Some(n) = integral_index(key_v) {
            return Some(n);
        }
        let h = key_v.as_string()?;
        let digits = self.string_text(h);
        if digits.is_empty() || digits.len() > 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse::<u32>().ok()
    }

    /// Decodes a heap string to Rust text (key handling and diagnostics).
    fn string_text(&mut self, h: Handle<V12Str>) -> String {
        let units = ops::string_units(&mut self.heap, h);
        String::from_utf16_lossy(&units)
    }

    /// Resolves a key value to a named-property key. Numbers coerce through
    /// their canonical decimal spelling; everything else goes through
    /// ES `ToString`.
    fn property_key(&mut self, key_v: JsValue) -> Result<PropKey, JSException> {
        if let Some(h) = key_v.as_string() {
            return Ok(PropKey::from_string(h));
        }
        if let Some(y) = key_v.as_symbol() {
            return Ok(PropKey::from_symbol(y));
        }
        let h = ops::to_js_string(&mut self.heap, key_v)?;
        Ok(PropKey::from_string(h))
    }

    /// `GetProperty` with monomorphic inline-cache probing.
    ///
    /// Accessors: descriptors carry attribute bits only — accessor pairs have
    /// no storage in the current heap revision, so none can exist on any
    /// object this interpreter builds and lookups always yield plain slot
    /// values. When accessor support lands, `get_property` and
    /// `set_property` must both learn to detect and invoke them.
    fn get_property(
        &mut self,
        site_fn: u32,
        site_pc: u32,
        obj_v: JsValue,
        key_v: JsValue,
    ) -> Result<JsValue, JSException> {
        // Primitives have no wrappers yet: reads yield undefined, matching
        // real JS minus the built-ins that would populate the wrappers.
        let Some(obj) = obj_v.as_object() else {
            return Ok(JsValue::undefined());
        };

        if self.heap.get(obj).kind == KIND_ARRAY
            && let Some(idx) = self.array_index_of(key_v)
        {
            return Ok(self.array_element(obj, idx));
        }

        let key = self.property_key(key_v)?;
        let shape = self.shape_of(obj);

        // Inline-cache probe: trust the cached (shape, slot) pair only after
        // the shape still matches the receiver's current one.
        let cached = self
            .feedback
            .get(&site_fn)
            .and_then(|fv| fv.ics.get(&site_pc))
            .copied();
        if let Some(ic) = cached
            && ic.shape == shape
            && let Some(v) = self.heap.get(obj).properties.get(ic.slot as usize)
        {
            return Ok(*v);
        }

        // Slow path: own shape first, then the prototype chain.
        let mut cur = Some(obj);
        let mut hit = None;
        while let Some(o) = cur {
            let sh = self.shape_of(o);
            if let Some(d) = self.heap.lookup_property(sh, key) {
                hit = Some((o, d.slot));
                break;
            }
            cur = self.heap.get(o).prototype;
        }
        match hit {
            Some((owner, slot)) => {
                let value = self.heap.get(owner).properties[slot as usize];
                // Record receiver-own hits only: inherited loads gain nothing
                // from a cache keyed to the receiver's shape.
                if owner == obj {
                    self.feedback
                        .entry(site_fn)
                        .or_default()
                        .ics
                        .insert(site_pc, MonoIc { shape, slot });
                }
                Ok(value)
            }
            None => Ok(JsValue::undefined()),
        }
    }

    /// `SetProperty`: overwrite own writable slots, create new own properties
    /// through shape transitions, shadow writable inherited ones, and route
    /// canonical indices on arrays through the element store. Blocked writes
    /// are silently dropped, matching sloppy-mode JS; strict-mode throwing
    /// awaits error-object plumbing.
    fn set_property(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        value: JsValue,
    ) -> Result<(), JSException> {
        let Some(obj) = obj_v.as_object() else {
            if obj_v.is_null() || obj_v.is_undefined() {
                return Err(JSException(self.error_value(
                    "TypeError: cannot set properties of null or undefined",
                )));
            }
            // Primitive targets accept and drop writes (no wrapper objects).
            return Ok(());
        };

        if self.heap.get(obj).kind == KIND_ARRAY
            && let Some(idx) = self.array_index_of(key_v)
        {
            self.array_set_element(obj, idx, value);
            return Ok(());
        }

        let key = self.property_key(key_v)?;
        let shape = self.shape_of(obj);
        let own = self.heap.get(shape).descriptors.find(key).copied();

        if let Some(d) = own {
            if d.attrs.writable() {
                self.heap.get_mut(obj).properties[d.slot as usize] = value;
            }
            return Ok(());
        }

        // An inherited non-writable property blocks shadowing (ES OrdinarySet).
        if let Some(d) = self.inherited_descriptor(obj, key)
            && !d.attrs.writable()
        {
            return Ok(());
        }

        if self.heap.get(obj).flags & JsObject::FLAG_NOT_EXTENSIBLE != 0 {
            return Ok(());
        }

        // Extend the layout: the transition may allocate, so protect roots
        // first and publish the new shape before touching storage again.
        self.gc_protect();
        let child = self.heap.add_property(shape, key, Attrs::DEFAULT);
        self.bind_shape(obj, child);
        self.heap.get_mut(obj).properties.push(value);
        Ok(())
    }

    /// First descriptor naming `key` along `obj`'s prototype chain.
    fn inherited_descriptor(&mut self, obj: Handle<JsObject>, key: PropKey) -> Option<Descriptor> {
        let mut cur = self.heap.get(obj).prototype;
        while let Some(o) = cur {
            let sh = self.shape_of(o);
            if let Some(d) = self.heap.lookup_property(sh, key) {
                return Some(*d);
            }
            cur = self.heap.get(o).prototype;
        }
        None
    }

    /// `DeleteProperty`: configurable own properties become holes (slot
    /// numbering survives for siblings), absent ones report success, locked
    /// ones report failure. Element deletes hole out the slot.
    fn delete_property(&mut self, obj_v: JsValue, key_v: JsValue) -> Result<bool, JSException> {
        let Some(obj) = obj_v.as_object() else {
            if obj_v.is_null() || obj_v.is_undefined() {
                return Err(JSException(self.error_value(
                    "TypeError: cannot delete properties of null or undefined",
                )));
            }
            // Primitives have no own properties: nothing to remove.
            return Ok(true);
        };

        if self.heap.get(obj).kind == KIND_ARRAY
            && let Some(idx) = self.array_index_of(key_v)
        {
            let els = &mut self.heap.get_mut(obj).elements;
            if usize::try_from(idx).is_ok_and(|i| i < els.len()) {
                els[idx as usize] = JsValue::hole();
            }
            return Ok(true);
        }

        let key = self.property_key(key_v)?;
        let shape = self.shape_of(obj);
        let Some(d) = self.heap.get(shape).descriptors.find(key).copied() else {
            return Ok(true); // not an own property: ES says success
        };
        if !d.attrs.configurable() {
            return Ok(false);
        }
        self.heap.get_mut(obj).properties[d.slot as usize] = JsValue::hole();
        Ok(true)
    }

    fn array_element(&self, obj: Handle<JsObject>, idx: u32) -> JsValue {
        self.heap
            .get(obj)
            .elements
            .get(idx as usize)
            .filter(|v| !v.is_hole())
            .copied()
            .unwrap_or(JsValue::undefined())
    }

    /// Stores an element, hole-filling gaps and keeping `length` current.
    fn array_set_element(&mut self, obj: Handle<JsObject>, idx: u32, value: JsValue) {
        let grows = usize::try_from(idx).is_ok_and(|i| i >= self.heap.get(obj).elements.len());
        if grows {
            {
                let els = &mut self.heap.get_mut(obj).elements;
                els.resize(idx as usize + 1, JsValue::hole());
            }
            let len_key = self.length_key();
            let shape = self.shape_of(obj);
            // Copy the slot out: the descriptor borrows the heap immutably,
            // which would conflict with the store below.
            let slot = self
                .heap
                .lookup_property(shape, len_key)
                .map(|d| d.slot as usize);
            if let Some(slot) = slot {
                self.heap.get_mut(obj).properties[slot] = ops::box_number(f64::from(idx + 1));
            }
        }
        self.heap.get_mut(obj).elements[idx as usize] = value;
    }

    // ------------------------------------------------------------------
    // Feedback
    // ------------------------------------------------------------------

    /// Counts one loop-header crossing for `fn_idx`.
    fn note_loop(&mut self, fn_idx: u32) {
        if self.feedback.entry(fn_idx).or_default().crossing_loop() {
            self.tier_up_pending.push(fn_idx);
        }
    }

    /// Counts one activation of `fn_idx`.
    fn note_entry(&mut self, fn_idx: u32) {
        if self.feedback.entry(fn_idx).or_default().activated() {
            self.tier_up_pending.push(fn_idx);
        }
    }

    /// Fires tier-up hooks for everything observed since the last drain.
    /// Invoked between frame completions so the driver sees stable frames.
    fn notify_tier_ups(&mut self) {
        if self.tier_up_pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.tier_up_pending);
        for fn_idx in pending {
            self.hooks.on_tier_up(fn_idx);
        }
    }

    // ------------------------------------------------------------------
    // GC coordination
    // ------------------------------------------------------------------

    /// Republishes every live reference as a GC root — the whole value stack
    /// plus each active frame's environment — immediately before opcodes
    /// that can reach `Heap::alloc`.
    fn gc_protect(&mut self) {
        let roots = &mut self.heap.roots_mut().0;
        roots.clear();
        roots.extend_from_slice(&self.stack);
        for frame in &self.frames {
            if let Some(env) = frame.env {
                roots.push(JsValue::object(env));
            }
        }
    }
}

/// Unsigned-array-index view of a numeric value (integral doubles only;
/// Smis were handled by the caller).
fn integral_index(v: JsValue) -> Option<u32> {
    let n = v.as_f64()?;
    if n.fract() != 0.0 || !(0.0..4_294_967_296.0).contains(&n) {
        return None;
    }
    // Guarded above: integral and below 2³² casts exactly.
    Some(n as u32)
}
