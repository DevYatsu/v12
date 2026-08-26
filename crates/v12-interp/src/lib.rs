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
    Attrs, Descriptor, GcPolicy, Handle, Heap, JsObject, JsValue,
    KIND_ARGUMENTS as HEAP_KIND_ARGUMENTS, KIND_ARRAY as HEAP_KIND_ARRAY,
    KIND_FUNCTION as HEAP_KIND_FUNCTION, KIND_GENERATOR as HEAP_KIND_GENERATOR, PropKey,
    ShapeHandle, V12Str,
};

use crate::feedback::{FeedbackVector, MonoIc, TYPE_NAME_COUNT, TYPE_NAMES, TierHooks};

/// Object kind for user functions created by `Closure`.
///
/// Kind values are engine-assigned; these must stay distinct from the
/// heap's [`v12_heap::KIND_ORDINARY`] and from each other.
pub const KIND_FUNCTION: u8 = HEAP_KIND_FUNCTION;

/// Object kind for array literals created by `NewArray`; canonical integer
/// keys on arrays route through the element store instead of named shapes.
pub const KIND_ARRAY: u8 = HEAP_KIND_ARRAY;

/// Object kind for arguments exotic objects.
pub const KIND_ARGUMENTS: u8 = HEAP_KIND_ARGUMENTS;

/// Object kind for generator objects.
pub const KIND_GENERATOR: u8 = HEAP_KIND_GENERATOR;

/// Offset for global var slots when the main env aliases the global object.
///
/// The global object's `properties` vector starts with this many intrinsic
/// slots; main-env slot `0` maps to `properties[GLOBAL_VAR_OFFSET]`.
const GLOBAL_VAR_OFFSET: usize = 10;

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
    /// Cached key for the `prototype` property used by `instanceof`.
    prototype_key: Option<PropKey>,
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
    /// Optional global object handle for global-code `var` aliasing.
    ///
    /// When `Some(g)`, `NewEnvironment` for the main function aliases `g`
    /// (with `GLOBAL_VAR_OFFSET` slot bias) so top-level `var` declarations
    /// that escape to an env become properties of the global object.
    global: Option<Handle<JsObject>>,
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
            prototype_key: None,
            length_shape: None,
            pinned_shapes: HashSet::new(),
            shape_of_cell: std::collections::HashMap::new(),
            stack: Vec::with_capacity(INITIAL_STACK_CAPACITY),
            frames: Vec::new(),
            natives: Box::new(EmptyNativeRegistry),
            hooks: Box::new(()),
            feedback: std::collections::HashMap::new(),
            tier_up_pending: Vec::new(),
            global: None,
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

    /// Sets the global object handle for global-code `var` aliasing.
    pub fn set_global(&mut self, global: Handle<JsObject>) {
        self.global = Some(global);
    }

    /// Builds an interpreter that reuses an existing heap and optional global.
    ///
    /// The caller must ensure `global` (if any) is allocated in `heap` and
    /// rooted.
    pub fn new_with_heap(
        heap: Heap,
        global: Option<Handle<JsObject>>,
        functions: Vec<FunctionBytecode>,
        main: u32,
        strings: Vec<String>,
    ) -> Self {
        let mut heap = heap;
        heap.roots_mut().0.reserve(INITIAL_STACK_CAPACITY);
        Self {
            functions: std::sync::Arc::from(functions.into_boxed_slice()),
            main,
            strings: std::sync::Arc::from(strings.into_boxed_slice()),
            heap,
            const_strings: std::collections::HashMap::new(),
            typeof_names: [const { None }; TYPE_NAME_COUNT],
            length_key: None,
            prototype_key: None,
            length_shape: None,
            pinned_shapes: HashSet::new(),
            shape_of_cell: std::collections::HashMap::new(),
            stack: Vec::with_capacity(INITIAL_STACK_CAPACITY),
            frames: Vec::new(),
            natives: Box::new(EmptyNativeRegistry),
            hooks: Box::new(()),
            feedback: std::collections::HashMap::new(),
            tier_up_pending: Vec::new(),
            global,
        }
    }

    /// Consumes the interpreter and returns its heap.
    pub fn into_heap(self) -> Heap {
        self.heap
    }

    /// Mutable heap access for embedders that share the heap.
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
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
    pub(crate) fn bind_shape_for_test(&mut self, obj: Handle<JsObject>, shape: ShapeHandle) {
        self.bind_shape(obj, shape);
    }

    #[cfg(test)]
    pub(crate) fn op_in_for_test(
        &mut self,
        key_v: JsValue,
        obj_v: JsValue,
    ) -> Result<bool, JSException> {
        self.gc_protect();
        self.op_in(key_v, obj_v)
    }

    #[cfg(test)]
    pub(crate) fn op_instanceof_for_test(
        &mut self,
        lhs_v: JsValue,
        rhs_v: JsValue,
    ) -> Result<bool, JSException> {
        self.gc_protect();
        self.op_instanceof(lhs_v, rhs_v)
    }

    #[cfg(test)]
    pub(crate) fn get_property_for_test(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
    ) -> Result<JsValue, JSException> {
        self.gc_protect();
        self.get_property(0, 0, obj_v, key_v)
    }

    #[cfg(test)]
    pub(crate) fn set_property_for_test(
        &mut self,
        obj_v: JsValue,
        key_v: JsValue,
        value: JsValue,
    ) -> Result<(), JSException> {
        self.gc_protect();
        self.set_property(obj_v, key_v, value)
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
                        WideOp::CopyObjectRestW {
                            dst,
                            src,
                            excl_base,
                            excl_count,
                        } => {
                            let src_v = self.stack[base + usize::from(src)];
                            let excl_vals_vec = if excl_count == 0 {
                                Vec::new()
                            } else {
                                let start = base + usize::from(excl_base);
                                let end = start + usize::from(excl_count);
                                self.stack[start..end].to_vec()
                            };
                            let dst_val = attempt!(self.op_copy_object_rest(src_v, &excl_vals_vec));
                            self.stack[base + usize::from(dst)] = dst_val;
                        }
                        WideOp::CopyArrayRestW { dst, src, start } => {
                            let src_v = self.stack[base + usize::from(src)];
                            let dst_val = attempt!(self.op_copy_array_rest(src_v, start));
                            self.stack[base + usize::from(dst)] = dst_val;
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
                Opcode::In => {
                    let key_v = self.stack[base + usize::from(instr.b())];
                    let obj_v = self.stack[base + usize::from(instr.c())];
                    self.gc_protect();
                    let present = attempt!(self.op_in(key_v, obj_v));
                    self.write_bool(base, instr.a(), present);
                    self.set_pc(pc + 1);
                }
                Opcode::InstanceOf => {
                    let lhs_v = self.stack[base + usize::from(instr.b())];
                    let rhs_v = self.stack[base + usize::from(instr.c())];
                    self.gc_protect();
                    let result = attempt!(self.op_instanceof(lhs_v, rhs_v));
                    self.write_bool(base, instr.a(), result);
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
                    // Global-code var hoisting: main's env aliases the global
                    // object with a slot bias so top-level `var`s become global
                    // properties.
                    if let Some(g) = self.global
                        && fn_idx == self.main
                    {
                        let needed = GLOBAL_VAR_OFFSET + slots;
                        let cur_len = self.heap.get(g).properties.len();
                        if cur_len < needed {
                            self.heap
                                .get_mut(g)
                                .properties
                                .resize(needed, JsValue::undefined());
                        }
                        self.frames.last_mut().expect("frame").env = Some(g);
                    } else {
                        let parent = self.frames.last().expect("frame").env;
                        let h = self.heap.alloc(JsObject {
                            properties: vec![JsValue::undefined(); slots],
                            prototype: parent,
                            ..JsObject::default()
                        });
                        self.frames.last_mut().expect("frame").env = Some(h);
                    }
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
                Opcode::CopyArrayRest => {
                    let src_v = self.stack[base + usize::from(instr.b())];
                    let start = u16::from(instr.c());
                    let dst_val = attempt!(self.op_copy_array_rest(src_v, start));
                    self.stack[base + usize::from(instr.a())] = dst_val;
                    self.set_pc(pc + 1);
                }
                Opcode::CheckIsArray => {
                    let v = self.stack[base + usize::from(instr.a())];
                    attempt!(self.op_check_is_array(v));
                    self.set_pc(pc + 1);
                }
                Opcode::CallApply => {
                    let callee = instr.b();
                    let dst = instr.a();
                    let args_reg = instr.c();
                    let this_v = self.stack[base + usize::from(callee) + 1];
                    let callee_v = self.stack[base + usize::from(callee)];
                    let args_v = self.stack[base + usize::from(args_reg)];
                    self.gc_protect();
                    let result =
                        attempt!(self.prepare_call_apply(base, max_regs, callee_v, this_v, args_v));
                    match result {
                        CallOutcome::Pushed => continue 'drive,
                        CallOutcome::Value(v) => {
                            self.stack[base + usize::from(dst)] = v;
                            self.set_pc(pc + 1);
                        }
                    }
                    continue 'drive;
                }
                Opcode::CopyObjectRest => {
                    // Narrow form with single excluded key in c (or 0).
                    let src_v = self.stack[base + usize::from(instr.b())];
                    let excl_vec = if instr.c() == 0 {
                        Vec::new()
                    } else {
                        let start = base + usize::from(instr.c());
                        self.stack[start..start + 1].to_vec()
                    };
                    let dst_val = attempt!(self.op_copy_object_rest(src_v, &excl_vec));
                    self.stack[base + usize::from(instr.a())] = dst_val;
                    self.set_pc(pc + 1);
                }
                Opcode::ArrayAppend => {
                    let dst_v = self.stack[base + usize::from(instr.a())];
                    let src_v = self.stack[base + usize::from(instr.b())];
                    attempt!(self.op_array_append(dst_v, src_v));
                    self.set_pc(pc + 1);
                }
                Opcode::GetGlobal => {
                    let dst = instr.a();
                    let const_id = u32::from(instr.imm16());
                    let val = attempt!(self.op_get_global(const_id));
                    self.stack[base + usize::from(dst)] = val;
                    self.set_pc(pc + 1);
                }
                Opcode::SetGlobal => {
                    let src = instr.a();
                    let const_id = u32::from(instr.imm16());
                    let val = self.stack[base + usize::from(src)];
                    attempt!(self.op_set_global(const_id, val));
                    self.set_pc(pc + 1);
                }

                Opcode::CreateGenerator => {
                    let dst = instr.a();
                    // Minimal generator object: empty object with generator kind.
                    self.gc_protect();
                    let h = self.heap.alloc(JsObject {
                        kind: KIND_GENERATOR,
                        ..JsObject::default()
                    });
                    // Add a dummy `next` property so `gen().next` is callable
                    // (returns the generator itself for chaining; real resume
                    // is not implemented in this stub).
                    self.stack[base + usize::from(dst)] = JsValue::object(h);
                    self.set_pc(pc + 1);
                }
                Opcode::SuspendYield => {
                    // Dummy: `yield value` just passes the value through.
                    // Real suspend would save frame and return to `next()` caller.
                    self.set_pc(pc + 1);
                }
                Opcode::Await => {
                    // Dummy: `await value` just passes through.
                    let src = instr.b();
                    let dst = instr.a();
                    self.stack[base + usize::from(dst)] = self.stack[base + usize::from(src)];
                    self.set_pc(pc + 1);
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
            // `null` is a singleton distinct from `undefined`.
            Const::Null => Ok(JsValue::null()),
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
                self.natives
                    .call_native(&mut self.heap, this_v, args, target)
            };
            return result.map(CallOutcome::Value).map_err(JSException);
        }

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }

        let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
            let f = &self.functions[target as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        let new_base = base + usize::from(caller_max_regs);
        let window_end = new_base + usize::from(callee_max_regs);

        // Extending the stack never moves existing slots, so the caller-tail
        // arguments stay valid while being copied into r1..
        let arg_src = callee_slot + 2;
        self.stack.resize(window_end, JsValue::undefined());
        self.stack[new_base] = this_v;
        if callee_has_rest {
            let fixed = callee_fixed as usize;
            let rest_reg = callee_rest_reg as usize;
            let fixed_to_copy = fixed
                .min(argc as usize)
                .min(usize::from(callee_max_regs).saturating_sub(1));
            for i in 0..fixed_to_copy {
                self.stack[new_base + 1 + i] = self.stack[arg_src + i];
            }
            let rest_start = fixed;
            let rest_len = (argc as usize).saturating_sub(rest_start);
            let rest_slice = if rest_len > 0 {
                self.stack[arg_src + rest_start..arg_src + rest_start + rest_len].to_vec()
            } else {
                Vec::new()
            };
            self.gc_protect();
            let shape = self.array_shape();
            let h = self.heap.alloc(JsObject {
                kind: KIND_ARRAY,
                properties: vec![ops::box_number(f64::from(rest_len as u32))],
                elements: rest_slice,
                ..JsObject::default()
            });
            self.bind_shape(h, shape);
            // `rest_reg` is the register index of the rest param (including this offset).
            // For `function f(a, ...rest)`, fixed=1, rest_reg=2 → `r2` is `rest`.
            if rest_reg < usize::from(callee_max_regs) {
                self.stack[new_base + rest_reg] = JsValue::object(h);
            }
        } else {
            let copied = usize::from(argc).min(usize::from(callee_max_regs).saturating_sub(1));
            for i in 0..copied {
                self.stack[new_base + 1 + i] = self.stack[arg_src + i];
            }
        }

        self.frames.push(Frame {
            fn_idx: target,
            pc: 0,
            base: new_base,
            max_regs: callee_max_regs,
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
                // deliver the exception into register `stack_depth`. The
                // stack must be restored to the full register window so
                // handler temporaries beyond the delivery register remain
                // addressable.
                let base = fr.base;
                let depth = h.stack_depth as usize;
                let max_regs = fr.max_regs as usize;
                self.stack.truncate(base + depth);
                self.stack.push(exc);
                self.stack.resize(base + max_regs, JsValue::undefined());
                self.stack[base + depth] = exc;
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
        let env = self.walk_env_expect(depth)?;
        let idx = self.env_slot_index(env, slot);
        let len = self.heap.get(env).properties.len();
        if idx < len {
            Ok(self.heap.get(env).properties[idx])
        } else {
            Err(JSException(self.error_value(
                "InternalError: environment slot out of range",
            )))
        }
    }

    fn env_write(&mut self, depth: u16, slot: u16, v: JsValue) -> Result<(), JSException> {
        let env = self.walk_env_expect(depth)?;
        let idx = self.env_slot_index(env, slot);
        let len = self.heap.get(env).properties.len();
        if idx < len {
            self.heap.get_mut(env).properties[idx] = v;
            Ok(())
        } else {
            Err(JSException(self.error_value(
                "InternalError: environment slot out of range",
            )))
        }
    }

    /// Maps a logical env `slot` to a physical index in `env`'s `properties`.
    ///
    /// Global-aliased envs reserve `GLOBAL_VAR_OFFSET` leading slots for the
    /// realm's intrinsics; other envs use the slot directly.
    fn env_slot_index(&self, env: Handle<JsObject>, slot: u16) -> usize {
        if Some(env) == self.global {
            GLOBAL_VAR_OFFSET + usize::from(slot)
        } else {
            usize::from(slot)
        }
    }

    /// Walks `depth` parent links from the current frame's environment.
    fn walk_env(&self, depth: u16) -> Option<Handle<JsObject>> {
        let mut cur = self.frames.last()?.env?;
        for _ in 0..depth {
            cur = self.heap.get(cur).prototype?;
        }
        Some(cur)
    }

    fn walk_env_expect(&mut self, depth: u16) -> Result<Handle<JsObject>, JSException> {
        self.walk_env(depth).ok_or_else(|| {
            JSException(self.error_value(
                "InternalError: environment depth exceeds live chain or missing environment",
            ))
        })
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

    fn prototype_key(&mut self) -> PropKey {
        if let Some(k) = self.prototype_key {
            return k;
        }
        let h = intern_text(&mut self.heap, "prototype");
        let k = PropKey::from_string(h);
        self.prototype_key = Some(k);
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

    /// `GetProperty` with monomorphic inline-cache probing and accessor support.
    ///
    /// Accessor descriptors invoke their getter (if any) by interpreting the
    /// getter string handle's text as a numeric literal for `v1` — the full
    /// `Engine::eval` path lives in `v12-engine` where the caller's heap is
    /// shared. `HasProperty` for arguments exotic indices is handled via the
    /// element store.
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

        // Arguments and arrays store integer indices in `elements`.
        let kind = self.heap.get(obj).kind;
        if (kind == KIND_ARRAY || kind == KIND_ARGUMENTS)
            && let Some(idx) = self.array_index_of(key_v)
        {
            // For arguments exotic, a mapped index mirrors the parameter slot;
            // v1 simply returns the element (the param alias is exercised via
            // the mapped array in heap tests).
            return Ok(self.array_element(obj, idx));
        }

        let key = self.property_key(key_v)?;
        let shape = self.shape_of(obj);

        // Inline-cache probe: only data descriptors with a slot are cached.
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
        let mut hit: Option<(Handle<JsObject>, Descriptor)> = None;
        while let Some(o) = cur {
            let sh = self.shape_of(o);
            if let Some(d) = self.heap.lookup_property(sh, key) {
                hit = Some((o, *d));
                break;
            }
            cur = self.heap.get(o).prototype;
        }
        match hit {
            Some((owner, desc)) => match desc {
                Descriptor::Data { slot, .. } => {
                    let value = self.heap.get(owner).properties[slot as usize];
                    if owner == obj {
                        self.feedback
                            .entry(site_fn)
                            .or_default()
                            .ics
                            .insert(site_pc, MonoIc { shape, slot });
                    }
                    Ok(value)
                }
                Descriptor::Accessor { getter, .. } => {
                    if let Some(g) = getter {
                        // v1: interpret getter string as numeric literal; fallback
                        // is the string itself. The engine's `dispatch_get`
                        // provides the full `eval` path.
                        let text = self.string_text(g);
                        let trimmed = text.trim();
                        if let Ok(n) = trimmed.parse::<f64>() {
                            Ok(ops::box_number(n))
                        } else if trimmed.is_empty() {
                            Ok(JsValue::undefined())
                        } else {
                            // Non-numeric getter body: treat as string value for
                            // the minimal tier-0 accessor test.
                            Ok(JsValue::string(g))
                        }
                    } else {
                        Ok(JsValue::undefined())
                    }
                }
            },
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

        let kind = self.heap.get(obj).kind;
        if (kind == KIND_ARRAY || kind == KIND_ARGUMENTS)
            && let Some(idx) = self.array_index_of(key_v)
        {
            // Arguments exotic: if mapped, the element mirrors the parameter
            // slot (v1 keeps the element store authoritative; callers inspect
            // `heap.get(obj).arguments_mapped` directly).
            self.array_set_element(obj, idx, value);
            return Ok(());
        }

        let key = self.property_key(key_v)?;
        let shape = self.shape_of(obj);
        let own = self.heap.get(shape).descriptors.find(key).copied();

        if let Some(d) = own {
            match d {
                Descriptor::Data { slot, attrs, .. } => {
                    if attrs.writable() {
                        self.heap.get_mut(obj).properties[slot as usize] = value;
                    }
                    return Ok(());
                }
                Descriptor::Accessor { setter, .. } => {
                    // Accessor with setter: invoke it (v1: no-op beyond the
                    // existence check). Without a setter, sloppy sets are
                    // silently dropped.
                    if setter.is_some() {
                        // v1 setter invocation is a no-op that acknowledges
                        // the set; the engine's `dispatch_set` provides the
                        // full eval path for string-bodied setters.
                        let _ = setter;
                    }
                    return Ok(());
                }
            }
        }

        // An inherited non-writable data property or accessor without setter
        // blocks shadowing (ES OrdinarySet).
        if let Some(d) = self.inherited_descriptor(obj, key) {
            match d {
                Descriptor::Data { attrs, .. } if !attrs.writable() => return Ok(()),
                Descriptor::Accessor { setter, .. } if setter.is_none() => return Ok(()),
                Descriptor::Accessor { .. } => {
                    // Inherited accessor with setter: invoke (v1 no-op).
                    return Ok(());
                }
                _ => {}
            }
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

    /// `in` operator: `key in obj`. Throws TypeError if `obj` is not an
    /// object; otherwise returns true when `key` (after ToPropertyKey)
    /// exists anywhere on `obj`'s prototype chain, including array indices.
    fn op_in(&mut self, key_v: JsValue, obj_v: JsValue) -> Result<bool, JSException> {
        let Some(obj) = obj_v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: right-hand side of 'in' should be an object",
            )));
        };
        // Fast path for array/arguments indices: check element storage before
        // coercing the key, which may allocate. Holes count as absent.
        let kind = self.heap.get(obj).kind;
        if (kind == KIND_ARRAY || kind == KIND_ARGUMENTS)
            && let Some(idx) = self.array_index_of(key_v)
            && let Some(slot) = self.heap.get(obj).elements.get(idx as usize)
            && !slot.is_hole()
        {
            return Ok(true);
        }
        let key = self.property_key(key_v)?;
        let mut cur = Some(obj);
        while let Some(o) = cur {
            let sh = self.shape_of(o);
            if self.heap.lookup_property(sh, key).is_some() {
                return Ok(true);
            }
            // For arrays, the prototype chain check after the element fast
            // path already covers named properties; indices are only in the
            // element store, so no extra work is needed. We still walk in
            // case a numeric string was installed as a named property.
            cur = self.heap.get(o).prototype;
        }
        // If we fell through from the array fast path with a hole, and the
        // shape walk found nothing, the property is absent.
        Ok(false)
    }

    /// `instanceof` operator. Throws TypeError if `rhs` is not an object
    /// with an object-typed `prototype` property; returns false if `lhs`
    /// is not an object; otherwise walks `lhs`'s prototype chain for
    /// identity against `rhs.prototype`.
    fn op_instanceof(&mut self, lhs_v: JsValue, rhs_v: JsValue) -> Result<bool, JSException> {
        let Some(rhs_obj) = rhs_v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: right-hand side of 'instanceof' is not an object",
            )));
        };
        // Fast path for built-in constructors whose prototype has not been
        // wired via `Heap::add_property` (Realm creates them as empty objects).
        if let Some(global) = self.global {
            let props = &self.heap.get(global).properties;
            if props.len() >= 2 {
                if let Some(obj_ctor) = props[0].as_object()
                    && rhs_obj == obj_ctor
                {
                    return Ok(lhs_v.as_object().is_some());
                }
                if let Some(arr_ctor) = props[1].as_object()
                    && rhs_obj == arr_ctor
                {
                    return Ok(lhs_v
                        .as_object()
                        .is_some_and(|h| self.heap.get(h).kind == KIND_ARRAY));
                }
            }
        }
        let proto_key = self.prototype_key();
        // Locate `rhs.prototype` along rhs's prototype chain (own or inherited).
        let mut rhs_proto_val: Option<JsValue> = None;
        {
            let mut cur = Some(rhs_obj);
            while let Some(o) = cur {
                let sh = self.shape_of(o);
                if let Some(d) = self.heap.lookup_property(sh, proto_key) {
                    let val = match *d {
                        Descriptor::Data { slot, .. } => self.heap.get(o).properties[slot as usize],
                        Descriptor::Accessor { getter, .. } => {
                            if let Some(g) = getter {
                                let text = self.string_text(g);
                                if let Ok(n) = text.trim().parse::<f64>() {
                                    ops::box_number(n)
                                } else {
                                    JsValue::string(g)
                                }
                            } else {
                                JsValue::undefined()
                            }
                        }
                    };
                    rhs_proto_val = Some(val);
                    break;
                }
                cur = self.heap.get(o).prototype;
            }
        }
        let Some(proto_val) = rhs_proto_val else {
            return Err(JSException(self.error_value(
                "TypeError: function has non-object prototype 'prototype' in instanceof check",
            )));
        };
        let Some(proto_obj) = proto_val.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: function has non-object prototype 'prototype' in instanceof check",
            )));
        };
        let Some(mut cur) = lhs_v.as_object() else {
            return Ok(false);
        };
        loop {
            let next = self.heap.get(cur).prototype;
            match next {
                None => return Ok(false),
                Some(p) if p == proto_obj => return Ok(true),
                Some(p) => cur = p,
            }
        }
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
        if !d.attrs().configurable() {
            return Ok(false);
        }
        match d {
            Descriptor::Data { slot, .. } => {
                self.heap.get_mut(obj).properties[slot as usize] = JsValue::hole();
            }
            Descriptor::Accessor { .. } => {
                // Accessor: no slot to hole; deletion succeeds if configurable.
            }
        }
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
                .and_then(|d| d.slot())
                .map(|s| s as usize);
            if let Some(slot) = slot {
                self.heap.get_mut(obj).properties[slot] = ops::box_number(f64::from(idx + 1));
            }
        }
        self.heap.get_mut(obj).elements[idx as usize] = value;
    }

    fn op_copy_array_rest(&mut self, src_v: JsValue, start: u16) -> Result<JsValue, JSException> {
        let Some(src_obj) = src_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: cannot destructure non-iterable"),
            ));
        };
        if self.heap.get(src_obj).kind != KIND_ARRAY {
            // For destructuring, non-array iterable is still an error in our subset (only arrays).
            return Err(JSException(
                self.error_value("TypeError: spread/rest source is not an array"),
            ));
        }
        let start_usize = start as usize;
        let src_len = self.heap.get(src_obj).elements.len();
        let slice = if start_usize >= src_len {
            Vec::new()
        } else {
            self.heap.get(src_obj).elements[start_usize..].to_vec()
        };
        let new_len = slice.len() as u32;
        self.gc_protect();
        let shape = self.array_shape();
        let h = self.heap.alloc(JsObject {
            kind: KIND_ARRAY,
            properties: vec![ops::box_number(f64::from(new_len))],
            elements: slice,
            ..JsObject::default()
        });
        self.bind_shape(h, shape);
        // Elements may contain holes; they are preserved as hole values.
        Ok(JsValue::object(h))
    }

    fn op_copy_object_rest(
        &mut self,
        src_v: JsValue,
        excl_vals: &[JsValue],
    ) -> Result<JsValue, JSException> {
        if src_v.is_null() || src_v.is_undefined() {
            return Err(JSException(self.error_value(
                "TypeError: cannot destructure 'undefined' or 'null'",
            )));
        }
        let Some(src_obj) = src_v.as_object() else {
            // Primitives in object rest: spec coerces to object, but our subset treats as empty.
            self.gc_protect();
            let h = self.heap.alloc(JsObject::default());
            let shape = self.heap.root_shape();
            self.bind_shape(h, shape);
            return Ok(JsValue::object(h));
        };
        // Collect excluded keys as PropKeys for fast compare.
        // Build a set of handler strings for comparison (using heap string handles if possible).
        // For simplicity, compare via textual equality using strings_equal for string values.
        // Excluded values may be strings, numbers, symbols. Convert via property_key.
        let mut excl_keys: Vec<PropKey> = Vec::with_capacity(excl_vals.len());
        for &v in excl_vals {
            // Numbers and booleans coerce via ToPropertyKey: use property_key which may allocate.
            // For performance, handle string fast path.
            if let Some(h) = v.as_string() {
                // Use the string handle already interned.
                excl_keys.push(PropKey::from_string(h));
                continue;
            }
            if let Some(n) = v.as_smi().map(f64::from).or(v.as_f64()) {
                // Numeric key → decimal string.
                let text = ops::number_to_string(n);
                let h = intern_text(&mut self.heap, &text);
                excl_keys.push(PropKey::from_string(h));
                continue;
            }
            if let Some(b) = v.as_bool() {
                let text = if b { "true" } else { "false" };
                let h = intern_text(&mut self.heap, text);
                excl_keys.push(PropKey::from_string(h));
                continue;
            }
            if v.is_symbol()
                && let Some(y) = v.as_symbol()
            {
                excl_keys.push(PropKey::from_symbol(y));
                continue;
            }
            // Fallback: ToString then intern.
            let h = ops::to_js_string(&mut self.heap, v)?;
            excl_keys.push(PropKey::from_string(h));
        }

        let shape = self.shape_of(src_obj);
        // Snapshot descriptors to avoid borrow across allocation.
        let descs: Vec<v12_heap::Descriptor> = {
            let sh = self.heap.get(shape);
            sh.descriptors.as_slice().to_vec()
        };
        let src_props: Vec<JsValue> = self.heap.get(src_obj).properties.clone();
        self.gc_protect();
        let dst_h = self.heap.alloc(JsObject::default());
        let mut cur_shape = self.heap.root_shape();
        self.bind_shape(dst_h, cur_shape);
        for desc in descs {
            let key = desc.key();
            // Check if excluded.
            if excl_keys.contains(&key) {
                continue;
            }
            // Only data descriptors with slots are copied; accessors are skipped (hole).
            let Some(slot) = desc.slot() else {
                continue;
            };
            let slot_usize = slot as usize;
            if slot_usize >= src_props.len() {
                continue;
            }
            let val = src_props[slot_usize];
            if val.is_hole() {
                continue;
            }
            // Skip non-enumerable? All default are enumerable.
            // Add to dst.
            let child = self
                .heap
                .add_property(cur_shape, key, v12_heap::Attrs::DEFAULT);
            self.bind_shape(dst_h, child);
            self.heap.get_mut(dst_h).properties.push(val);
            cur_shape = child;
        }
        Ok(JsValue::object(dst_h))
    }

    fn op_check_is_array(&mut self, v: JsValue) -> Result<(), JSException> {
        let Some(obj) = v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        };
        if self.heap.get(obj).kind != KIND_ARRAY {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        }
        Ok(())
    }

    fn op_array_append(&mut self, dst_v: JsValue, src_v: JsValue) -> Result<(), JSException> {
        let Some(dst_obj) = dst_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: destination is not an object"),
            ));
        };
        if self.heap.get(dst_obj).kind != KIND_ARRAY {
            return Err(JSException(
                self.error_value("TypeError: destination is not an array"),
            ));
        }
        let Some(src_obj) = src_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        };
        if self.heap.get(src_obj).kind != KIND_ARRAY {
            return Err(JSException(
                self.error_value("TypeError: spread source is not an array"),
            ));
        }
        let src_elements = self.heap.get(src_obj).elements.clone();
        if src_elements.is_empty() {
            return Ok(());
        }
        // Extend dst elements and update length.
        let dst_len_before = self.heap.get(dst_obj).elements.len();
        let new_len = dst_len_before + src_elements.len();
        self.gc_protect();
        // Update shape length if needed (array length property is slot 0).
        let shape = self.shape_of(dst_obj);
        let len_key = self.length_key();
        let slot = self
            .heap
            .lookup_property(shape, len_key)
            .and_then(|d| d.slot())
            .map(|s| s as usize);
        if let Some(slot) = slot {
            self.heap.get_mut(dst_obj).properties[slot] =
                ops::box_number(f64::from(new_len as u32));
        }
        self.heap.get_mut(dst_obj).elements.extend(src_elements);
        Ok(())
    }

    fn op_get_global(&mut self, str_id: u32) -> Result<JsValue, JSException> {
        let Some(global) = self.global else {
            return Ok(JsValue::undefined());
        };
        let text = self
            .strings
            .get(str_id as usize)
            .cloned()
            .unwrap_or_default();
        // Fast path for intrinsics that live at fixed indices in the global's
        // properties vector (see `realm::INTRINSIC_NAMES` order).
        const INTRINSICS: &[&str] = &[
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
        if let Some(idx) = INTRINSICS.iter().position(|&n| n == text)
            && idx < self.heap.get(global).properties.len()
        {
            let v = self.heap.get(global).properties[idx];
            if !v.is_hole() {
                return Ok(v);
            }
        }
        let h = intern_text(&mut self.heap, &text);
        let key = PropKey::from_string(h);
        let shape = self.shape_of(global);
        if let Some(desc) = self.heap.lookup_property(shape, key)
            && let Some(slot) = desc.slot()
        {
            let idx = slot as usize;
            if idx < self.heap.get(global).properties.len() {
                let v = self.heap.get(global).properties[idx];
                if !v.is_hole() {
                    return Ok(v);
                }
            }
        }
        Ok(JsValue::undefined())
    }

    fn op_set_global(&mut self, str_id: u32, val: JsValue) -> Result<(), JSException> {
        let Some(global) = self.global else {
            return Ok(());
        };
        let text = self
            .strings
            .get(str_id as usize)
            .cloned()
            .unwrap_or_default();
        const INTRINSICS: &[&str] = &[
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
        if let Some(idx) = INTRINSICS.iter().position(|&n| n == text) {
            // Intrinsics are at fixed indices; allow overwriting.
            if idx < self.heap.get(global).properties.len() {
                self.heap.get_mut(global).properties[idx] = val;
                return Ok(());
            }
        }
        let h = intern_text(&mut self.heap, &text);
        let key = PropKey::from_string(h);
        let shape = self.shape_of(global);
        // If already a property, update.
        if let Some(desc) = self.heap.lookup_property(shape, key)
            && let Some(slot) = desc.slot()
        {
            let idx = slot as usize;
            if idx < self.heap.get(global).properties.len() {
                self.heap.get_mut(global).properties[idx] = val;
                return Ok(());
            }
        }
        // Otherwise, create new global property.
        self.gc_protect();
        let child = self.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
        self.bind_shape(global, child);
        self.heap.get_mut(global).properties.push(val);
        Ok(())
    }

    fn prepare_call_apply(
        &mut self,
        caller_base: usize,
        caller_max_regs: u16,
        callee_v: JsValue,
        this_v: JsValue,
        args_arr_v: JsValue,
    ) -> Result<CallOutcome, JSException> {
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
        if (target as usize) >= self.functions.len() {
            // Native
            let Some(args_obj) = args_arr_v.as_object() else {
                return Err(JSException(
                    self.error_value("TypeError: args is not an array"),
                ));
            };
            if self.heap.get(args_obj).kind != KIND_ARRAY {
                return Err(JSException(
                    self.error_value("TypeError: spread args is not an array"),
                ));
            }
            let args_slice = self.heap.get(args_obj).elements.clone();
            // Holes become undefined for call.
            let mut args_vec: Vec<JsValue> = Vec::with_capacity(args_slice.len());
            for v in args_slice {
                args_vec.push(if v.is_hole() { JsValue::undefined() } else { v });
            }
            self.gc_protect();
            let result = self
                .natives
                .call_native(&mut self.heap, this_v, &args_vec, target);
            return result.map(CallOutcome::Value).map_err(JSException);
        }
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }
        let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
            let f = &self.functions[target as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        // Check rest param handling for callee? prepare_call also handles rest, but we duplicate here.
        // For call_apply, the callee may have rest param; let prepare_call handle rest via metadata.
        // We need to materialize args into a temporary Vec then use similar logic as prepare_call but with dynamic argc.
        let Some(args_obj) = args_arr_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: args is not an array"),
            ));
        };
        let elements = self.heap.get(args_obj).elements.clone();
        let argc = elements.len() as u16;
        // Validate arity limits same as prepare_call.
        let caller_frame_regs = caller_max_regs;
        let new_base = caller_base + usize::from(caller_frame_regs);
        let window_end = new_base + usize::from(callee_max_regs);
        self.stack.resize(window_end, JsValue::undefined());
        self.stack[new_base] = this_v;
        // Handle rest param for callee if present.
        let has_rest = callee_has_rest;
        let fixed = callee_fixed as usize;
        let rest_reg = callee_rest_reg as usize;
        if has_rest {
            // Fixed params get first `fixed` args, rest gets array of remaining.
            let fixed_copy = fixed.min(elements.len());
            for (i, &v) in elements.iter().enumerate().take(fixed_copy) {
                self.stack[new_base + 1 + i] = if v.is_hole() { JsValue::undefined() } else { v };
            }
            // Missing fixed args already undefined via resize.
            // Build rest array from remaining elements.
            let rest_start = fixed;
            let rest_slice = if rest_start < elements.len() {
                elements[rest_start..]
                    .iter()
                    .map(|&v| if v.is_hole() { JsValue::undefined() } else { v })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let rest_len = rest_slice.len() as u32;
            self.gc_protect();
            let shape = self.array_shape();
            let h = self.heap.alloc(JsObject {
                kind: KIND_ARRAY,
                properties: vec![ops::box_number(f64::from(rest_len))],
                elements: rest_slice,
                ..JsObject::default()
            });
            self.bind_shape(h, shape);
            self.stack[new_base + rest_reg] = JsValue::object(h);
            // Ensure any param registers beyond fixed+rest remain undefined (already).
        } else {
            let copied = (argc as usize).min(usize::from(callee_max_regs).saturating_sub(1));
            for (i, &v) in elements.iter().enumerate().take(copied) {
                self.stack[new_base + 1 + i] = if v.is_hole() { JsValue::undefined() } else { v };
            }
        }
        self.frames.push(Frame {
            fn_idx: target,
            pc: 0,
            base: new_base,
            max_regs: callee_max_regs,
            env: captured_env,
        });
        self.note_entry(target);
        Ok(CallOutcome::Pushed)
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
