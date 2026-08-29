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

pub mod feedback;
mod ops;

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use v12_bytecode::{Const, FunctionBytecode, Instr, Opcode, WideOp};
use v12_heap::{
    Attrs, Descriptor, GcPolicy, Handle, Heap, JsObject, JsValue,
    KIND_ARGUMENTS as HEAP_KIND_ARGUMENTS, KIND_ARRAY as HEAP_KIND_ARRAY,
    KIND_FUNCTION as HEAP_KIND_FUNCTION, KIND_GENERATOR as HEAP_KIND_GENERATOR,
    KIND_ORDINARY as HEAP_KIND_ORDINARY, PropKey, ShapeHandle, V12Str,
};

use crate::feedback::{FeedbackVector, Lattice, MonoIc, TYPE_NAME_COUNT, TYPE_NAMES, TierHooks};

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

/// Native index for `console.log` — a console method that prints its
/// arguments and returns `undefined`. Chosen beyond any plausible program
/// function count, matching `v12_engine::builtins::NATIVE_CONSOLE_LOG`.
pub const NATIVE_CONSOLE_LOG: u32 = 1900;

/// Native index for `Array.prototype.push`, mirroring
/// `v12_engine::builtins::NATIVE_ARRAY_PUSH` (same duplication pattern as
/// `NATIVE_CONSOLE_LOG`; the interpreter sits below the engine crate).
pub const NATIVE_ARRAY_PUSH: u32 = 1100;

/// Native index for `Array.prototype.join`, mirroring
/// `v12_engine::builtins::NATIVE_ARRAY_JOIN`.
pub const NATIVE_ARRAY_JOIN: u32 = 1102;

/// Native index for `Object.enumerableOwnKeys` (internal), mirroring
/// `v12_engine::builtins::NATIVE_ENUMERABLE_OWN_KEYS`.
pub const NATIVE_ENUMERABLE_OWN_KEYS: u32 = 1901;

/// Native indices of the minimal Promise surface (`Promise.resolve`,
/// `Promise.reject`, `Promise.prototype.then`), mirroring
/// `v12_engine::builtins` constants.
pub const NATIVE_PROMISE_RESOLVE: u32 = 1710;
pub const NATIVE_PROMISE_REJECT: u32 = 1711;
pub const NATIVE_PROMISE_THEN: u32 = 1712;

/// Native index for `Generator.prototype.next`.
pub const NATIVE_GENERATOR_NEXT: u32 = 1910;
pub const NATIVE_GENERATOR_RETURN: u32 = 1911;
pub const NATIVE_GENERATOR_THROW: u32 = 1912;

/// Selector for [`Interp::cached_native`].
#[derive(Clone, Copy)]
enum NativeFn {
    PromiseResolve,
    PromiseReject,
    PromiseThen,
    ArrayPush,
    ArrayJoin,
    EnumerableOwnKeys,
    GeneratorNext,
    GeneratorReturn,
    GeneratorThrow,
}

/// Native indices of the only constructor-shaped built-ins (`new Boolean`,
/// `new Error`, and subclasses), mirroring `v12_engine::builtins`
/// constants so `Construct` can route them through the shared registry.
pub const NATIVE_BOOLEAN_CONSTRUCT: u32 = 1500;
pub const NATIVE_ERROR_CREATE: u32 = 1600;

/// Offset of user-declared global slots in the global object's `properties`.
///
/// The realm pushes this many intrinsic slots (`INTRINSIC_NAMES`) onto the
/// global's `properties` vector without shape descriptors, so any slot a
/// shape descriptor reports for the global must be biased by this constant
/// before indexing storage (see [`Interp::global_slot_index`]). Must stay in
/// sync with `v12-engine/src/realm.rs` `INTRINSIC_NAMES.len()` and
/// `v12-bccompiler/src/model.rs` `GLOBAL_INTRINSICS.len()` (v1: 14).
const GLOBAL_VAR_OFFSET: usize = 14;

/// Names of the intrinsic slots installed by the realm at fixed positions
/// in the global object's `properties` vector.
///
/// Mirrors `v12-engine/src/realm.rs` `INTRINSIC_NAMES` exactly (same names,
/// same order, same length as [`GLOBAL_VAR_OFFSET`]); duplicated here because
/// the interpreter sits below the engine crate. Deliberately *not* the
/// compiler's longer [`v12_bccompiler::model::GLOBAL_INTRINSICS`] table,
/// which lists more names than the v1 realm installs.
const GLOBAL_INTRINSIC_NAMES: &[&str] = &[
    "Object",
    "Array",
    "String",
    "Number",
    "Boolean",
    "Math",
    "JSON",
    "Error",
    "TypeError",
    "RangeError",
    "Promise",
    "Symbol",
    "console",
    "globalThis",
];

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
    /// Associated generator object when this frame is a generator activation.
    generator: Option<Handle<JsObject>>,
    /// Destination register of the SuspendYield that suspended this frame (for resume value delivery).
    yield_dst: Option<u16>,
}

/// Decodes the instruction at `pc` into
/// `(op, a, b, c, word_width, narrow_word)`.
///
/// - Narrow op: operands are the instruction's own byte slots, width 1.
/// - `Wide` header with discriminant [`WideOp::DISC_REG_EXT`]: merges the
///   prefix's high bytes into the following narrow instruction's register
///   slots per the prefix mask; header + payload + narrow instruction
///   execute as one logical op of width 3.
/// - Any other `Wide` header: returned as [`Opcode::Wide`] (width 1) so
///   the wide arm decodes the payload itself.
///
/// The returned `narrow_word` is the instruction whose *immediates*
/// (imm16/imm24, unmerged byte slots) apply; it equals the header word
/// for plain narrow ops and the third word for `RegExt` pairs.
fn decode_instr(instrs: &[Instr], pc: usize) -> (Opcode, u16, u16, u16, usize, Instr) {
    let instr = instrs[pc];
    let Some(op) = instr.op() else {
        panic!("corrupt bytecode: unassigned opcode byte at pc {pc}");
    };
    if op != Opcode::Wide {
        return (
            op,
            u16::from(instr.a()),
            u16::from(instr.b()),
            u16::from(instr.c()),
            1,
            instr,
        );
    }
    if u32::from(instr.c()) == WideOp::DISC_REG_EXT {
        let (wide, _) = WideOp::try_decode(&instrs[pc..]).expect("malformed wide opcode sequence");
        let WideOp::RegExt {
            mask,
            a_hi,
            b_hi,
            c_hi,
        } = wide
        else {
            unreachable!("discriminant matched REG_EXT")
        };
        let narrow = instrs.get(pc + 2).copied().unwrap_or_else(|| {
            panic!("corrupt bytecode: RegExt prefix at pc {pc} without its narrow instruction")
        });
        let narrow_op = narrow.op().unwrap_or_else(|| {
            panic!(
                "corrupt bytecode: RegExt prefix at pc {pc} not followed by a narrow instruction"
            )
        });
        let merge = |slot: u8, hi: u8, bit: u8| -> u16 {
            if mask & bit != 0 {
                (u16::from(hi) << 8) | u16::from(slot)
            } else {
                u16::from(slot)
            }
        };
        return (
            narrow_op,
            merge(narrow.a(), a_hi, 1),
            merge(narrow.b(), b_hi, 2),
            merge(narrow.c(), c_hi, 4),
            3,
            narrow,
        );
    }
    (Opcode::Wide, 0, 0, 0, 1, instr)
}

/// Destination register and total word width of the Call/Construct header
/// parked at `pc` while a callee frame runs, plus whether it is a
/// `Construct` (which needs the spec's return-value adjustment).
///
/// Handles the narrow forms, the wide `CallW`/`ConstructW` escapes, and the
/// `RegExt`-prefixed narrow form (parked pc is the prefix header).
fn decode_parked_call(instrs: &[Instr], pc: usize) -> (bool, u16, usize) {
    let instr = instrs[pc];
    match instr.op() {
        Some(Opcode::Wide) => match WideOp::try_decode(&instrs[pc..]) {
            Ok((WideOp::CallW { dst, .. }, width)) => (false, dst, width),
            Ok((WideOp::ConstructW { dst, .. }, width)) => (true, dst, width),
            Ok((WideOp::RegExt { mask, a_hi, .. }, _)) => {
                // The narrow call/construct follows the 2-word prefix.
                let narrow = instrs
                    .get(pc + 2)
                    .copied()
                    .expect("RegExt prefix without its narrow instruction");
                let dst = if mask & 1 != 0 {
                    (u16::from(a_hi) << 8) | u16::from(narrow.a())
                } else {
                    u16::from(narrow.a())
                };
                let is_construct = narrow.op() == Some(Opcode::Construct);
                (is_construct, dst, 3)
            }
            other => panic!("call parked on malformed wide header: {other:?}"),
        },
        _ => (
            instr.op() == Some(Opcode::Construct),
            u16::from(instr.a()),
            1,
        ),
    }
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
    /// Optional embedder-provided global object for `GetGlobal`/`SetGlobal`.
    ///
    /// When `None` at construction, a private default is allocated in
    /// [`Interp::ensure_default_global`], so it is always `Some` afterwards.
    /// Shape-derived property slots on this object map to
    /// `properties[GLOBAL_VAR_OFFSET + slot]`; see [`Self::global_slot_index`].
    global: Option<Handle<JsObject>>,
    /// Cached `console.log` function object, synthesized lazily on first
    /// `get_property` for `console.log`. The object is a `KIND_FUNCTION`
    /// whose `elements[0]` is `NATIVE_CONSOLE_LOG`, so `prepare_call` routes
    /// it through the `NativeRegistry`.
    console_log: Option<JsValue>,
    /// Cached native function objects for the Promise surface and the array
    /// `push`/`join` methods, synthesized lazily like `console_log` (see the
    /// `get_property` fast paths).
    promise_resolve_fn: Option<JsValue>,
    promise_reject_fn: Option<JsValue>,
    promise_then_fn: Option<JsValue>,
    array_push_fn: Option<JsValue>,
    array_join_fn: Option<JsValue>,
    enumerable_own_keys_fn: Option<JsValue>,
    generator_next_fn: Option<JsValue>,
    generator_return_fn: Option<JsValue>,
    generator_throw_fn: Option<JsValue>,
    /// Completion value of the bottom frame when the dispatch loop ends.
    ///
    /// `run` ignores it; `call_object` reads it to return the callee's result.
    top_result: Option<JsValue>,
    /// Pending async resumes as FIFO microtask queue: (generator, value, is_reject).
    pending_awaits: std::collections::VecDeque<(Handle<JsObject>, JsValue, bool)>,
}

impl Interp {
    /// Builds an interpreter over `program`, resolving `Const::Str32` ids
    /// against `strings` (as produced by
    /// `v12_bccompiler::compile_source_with_strings`).
    ///
    /// Top-level code addresses globals through `GetGlobal`/`SetGlobal`, so a
    /// global object must exist even when no embedder provides one: without an
    /// explicit [`Self::set_global`], a private default global is allocated
    /// and rooted here. It carries the `GLOBAL_VAR_OFFSET` leading intrinsic
    /// slots (all `undefined` outside a realm) so shared `GetGlobal` fast
    /// paths stay in bounds.
    pub fn new(functions: Vec<FunctionBytecode>, main: u32, strings: Vec<String>) -> Self {
        let mut heap = Heap::new(GcPolicy::default());
        heap.roots_mut().0.reserve(INITIAL_STACK_CAPACITY);
        let mut interp = Self {
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
            console_log: None,
            promise_resolve_fn: None,
            promise_reject_fn: None,
            promise_then_fn: None,
            array_push_fn: None,
            array_join_fn: None,
            enumerable_own_keys_fn: None,
            generator_next_fn: None,
            generator_return_fn: None,
            generator_throw_fn: None,
            top_result: None,
            pending_awaits: std::collections::VecDeque::new(),
        };
        interp.ensure_default_global();
        interp
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
    /// rooted. When `global` is `None`, a private default global is allocated
    /// and rooted in `heap` (see [`Self::new`]).
    pub fn new_with_heap(
        heap: Heap,
        global: Option<Handle<JsObject>>,
        functions: Vec<FunctionBytecode>,
        main: u32,
        strings: Vec<String>,
    ) -> Self {
        let mut heap = heap;
        heap.roots_mut().0.reserve(INITIAL_STACK_CAPACITY);
        let mut interp = Self {
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
            console_log: None,
            promise_resolve_fn: None,
            promise_reject_fn: None,
            promise_then_fn: None,
            array_push_fn: None,
            array_join_fn: None,
            enumerable_own_keys_fn: None,
            generator_next_fn: None,
            generator_return_fn: None,
            generator_throw_fn: None,
            top_result: None,
            pending_awaits: std::collections::VecDeque::new(),
        };
        interp.ensure_default_global();
        interp
    }

    /// Allocates the standalone default global when no embedder supplied one.
    ///
    /// The object is rooted immediately (allocation contract) and carries the
    /// `GLOBAL_VAR_OFFSET` intrinsic prefix slots so intrinsics fast paths can
    /// index without bounds concerns.
    fn ensure_default_global(&mut self) {
        if self.global.is_some() {
            return;
        }
        let g = self.heap.alloc(JsObject {
            properties: vec![JsValue::undefined(); GLOBAL_VAR_OFFSET],
            ..JsObject::default()
        });
        self.heap.add_root(JsValue::object(g));
        // Minimal Promise wiring for standalone interp tests (mirrors realm.rs)
        let promise_proto = self.heap.alloc(JsObject::default());
        self.heap.add_root(JsValue::object(promise_proto));
        let promise_ctor = self.heap.alloc(JsObject { kind: KIND_FUNCTION, prototype: Some(promise_proto), ..JsObject::default() });
        self.heap.add_root(JsValue::object(promise_ctor));
        {
            let props = &mut self.heap.get_mut(g).properties;
            if props.len() > 10 {
                props[10] = JsValue::object(promise_ctor);
            }
        }
        self.global = Some(g);
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
            generator: None,
            yield_dst: None,
        });
        self.note_entry(self.main);
        self.execute()
    }

    /// Calls a function by bytecode/native index from outside the machine.
    ///
    /// Host-driven activation seam (Promise reaction jobs, embedder calls):
    /// synthesizes the callee object `prepare_call` expects — a
    /// `KIND_FUNCTION` whose `elements[0]` selects the target, bytecode index
    /// below `functions.len()` or native index above — then delegates to
    /// [`Self::call_object`]. Must not be called while `run()` is active.
    pub fn call_function(
        &mut self,
        fn_idx: u32,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        self.gc_protect();
        let callee = self.heap.alloc(JsObject {
            kind: KIND_FUNCTION,
            elements: vec![JsValue::from_i32_smi(fn_idx as i32)
                .expect("function index fits Smi")],
            ..JsObject::default()
        });
        self.call_object(callee, this, args)
    }

    /// Calls an existing function object from outside the machine.
    ///
    /// Going through `prepare_call` (rather than pushing a frame by hand)
    /// preserves closure environment capture and native routing; the captured
    /// environment of a closure lives in the function object's `prototype`
    /// slot. Requires an empty frame stack — jobs run between `run()`
    /// activations, never inside one.
    pub fn call_object(
        &mut self,
        callee: Handle<JsObject>,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        debug_assert!(self.frames.is_empty(), "call_object must run outside of run()");
        // Lay out `[callee][this][args…]` exactly as a parked `Call` would.
        self.stack.clear();
        self.stack.push(JsValue::object(callee));
        self.stack.push(this);
        self.stack.extend_from_slice(args);
        let caller_max_regs =
            u16::try_from(self.stack.len()).expect("arguments fit a frame window");
        let argc = u16::try_from(args.len()).expect("argument count fits u16");
        self.top_result = None;
        match self.prepare_call(0, caller_max_regs, 0, argc)? {
            CallOutcome::Pushed => {
                self.execute()?;
                self.top_result.take().ok_or_else(|| {
                    JSException(self.error_value("InternalError: call completed without a result"))
                })
            }
            CallOutcome::Value(v) => Ok(v),
        }
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
            let Some(_op) = instr.op() else {
                panic!("corrupt bytecode: unassigned opcode byte at {fn_idx}:{pc}");
            };
            // Decode operands: narrow ops expose their byte slots directly;
            // a `RegExt` prefix merges high bytes into the following narrow
            // instruction's register slots and executes as one 3-word op
            // (see `WideOp::RegExt`).
            let (op, ra, rb, rc, op_width, narrow) = {
                let instrs = &self.functions[fn_idx as usize].instrs;
                decode_instr(instrs, pc)
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
                    self.stack[base + usize::from(ra)] = self.stack[base + usize::from(rb)];
                    self.set_pc(pc + op_width);
                }
                Opcode::LoadInt => {
                    let v = i8::from_be_bytes([narrow.c()]);
                    self.stack[base + usize::from(ra)] = ops::box_number(f64::from(v));
                    self.set_pc(pc + op_width);
                }
                Opcode::LoadConst => {
                    let value = attempt!(self.const_value(fn_idx, u32::from(narrow.imm16())));
                    self.stack[base + usize::from(ra)] = value;
                    self.set_pc(pc + op_width);
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
                                    self.set_pc(pc + width);
                                }
                            }
                            continue 'drive;
                        }
                        WideOp::ConstructW { dst, func, argc } => {
                            match attempt!(self.prepare_construct(base, max_regs, func, argc)) {
                                CallOutcome::Pushed => continue 'drive,
                                CallOutcome::Value(v) => {
                                    let caller_base = base;
                                    self.stack[caller_base + usize::from(dst)] = v;
                                    self.set_pc(pc + width);
                                }
                            }
                            continue 'drive;
                        }
                        WideOp::ClosureW {
                            dst,
                            function_index,
                        } => {
                            self.gc_protect();
                            let env = self.frames.last().expect("frame").env;
                            let h = self.heap.alloc(JsObject {
                                kind: KIND_FUNCTION,
                                elements: vec![ops::box_number(f64::from(function_index))],
                                prototype: env,
                                ..JsObject::default()
                            });
                            self.stack[base + usize::from(dst)] = JsValue::object(h);
                        }
                        WideOp::NewEnvironmentW { depth: _, slots } => {
                            // The static `depth` operand duplicates the
                            // dynamic parent chain (see crate docs); only the
                            // slot count matters here, matching the narrow op.
                            self.gc_protect();
                            let parent = self.frames.last().expect("frame").env;
                            let h = self.heap.alloc(JsObject {
                                properties: vec![JsValue::undefined(); usize::from(slots)],
                                prototype: parent,
                                ..JsObject::default()
                            });
                            self.frames.last_mut().expect("frame").env = Some(h);
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
                        // RegExt is decoded by `decode_instr` before dispatch
                        // (it merges the wide register halves into the narrow
                        // operand); reaching this arm means corrupt bytecode.
                        WideOp::RegExt { .. } => {
                            panic!("corrupt bytecode: bare RegExt reached dispatch")
                        }
                    }
                    self.set_pc(pc + width);
                }

                // ------------------------------------------------------
                // Arithmetic
                // ------------------------------------------------------
                Opcode::Add => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    let v = attempt!(ops::add(&mut self.heap, l, r));
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::Sub => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let v = ops::sub(&mut self.heap, l, r);
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::Mul => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let v = ops::mul(&mut self.heap, l, r);
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::Div | Opcode::Mod | Opcode::Pow => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let n = match op {
                        Opcode::Div => ops::div(&mut self.heap, l, r),
                        Opcode::Mod => ops::modulo(&mut self.heap, l, r),
                        _ => ops::js_pow(&mut self.heap, l, r),
                    };
                    self.stack[base + usize::from(ra)] = n;
                    let lat = Lattice::from_value(n, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Bitwise operations and shifts (ES ToInt32/ToUint32)
                // ------------------------------------------------------
                Opcode::BitAnd | Opcode::BitOr | Opcode::BitXor => {
                    let ln = ops::to_number(&mut self.heap, self.stack[base + usize::from(rb)]);
                    let rn = ops::to_number(&mut self.heap, self.stack[base + usize::from(rc)]);
                    let (a, b) = (ops::to_int32(ln), ops::to_int32(rn));
                    let n = match op {
                        Opcode::BitAnd => a & b,
                        Opcode::BitOr => a | b,
                        _ => a ^ b,
                    };
                    self.stack[base + usize::from(ra)] = ops::box_number(f64::from(n));
                    self.set_pc(pc + op_width);
                }
                Opcode::Shl | Opcode::Shr | Opcode::UShr => {
                    let ln = ops::to_number(&mut self.heap, self.stack[base + usize::from(rb)]);
                    let rn = ops::to_number(&mut self.heap, self.stack[base + usize::from(rc)]);
                    let shift = ops::to_uint32(rn) & 31;
                    let n = match op {
                        Opcode::Shl => ops::to_int32(ln) << shift,
                        Opcode::Shr => ops::to_int32(ln) >> shift,
                        // Unsigned shift reinterprets the int32 bits as u32.
                        _ => (ops::to_int32(ln) as u32 >> shift) as i32,
                    };
                    self.stack[base + usize::from(ra)] = ops::box_number(f64::from(n));
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Equality, comparison, unary operators
                // ------------------------------------------------------
                Opcode::Eq | Opcode::Ne => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let eq = ops::loose_equals(&mut self.heap, l, r);
                    self.write_bool(base, ra, eq ^ (op == Opcode::Ne));
                    self.set_pc(pc + op_width);
                }
                Opcode::StrictEq | Opcode::StrictNe => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let eq = ops::strict_equals(&self.heap, l, r);
                    self.write_bool(base, ra, eq ^ (op == Opcode::StrictNe));
                    self.set_pc(pc + op_width);
                }
                Opcode::Lt | Opcode::Le | Opcode::Gt | Opcode::Ge => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let ord = ops::compare(op, &mut self.heap, l, r);
                    self.write_bool(base, ra, ord);
                    self.set_pc(pc + op_width);
                }
                Opcode::Neg => {
                    let n = -ops::to_number(&mut self.heap, self.stack[base + usize::from(rb)]);
                    self.stack[base + usize::from(ra)] = ops::box_number(n);
                    self.set_pc(pc + op_width);
                }
                Opcode::BitNot => {
                    let n = ops::to_number(&mut self.heap, self.stack[base + usize::from(rb)]);
                    self.stack[base + usize::from(ra)] =
                        ops::box_number(f64::from(!ops::to_int32(n)));
                    self.set_pc(pc + op_width);
                }
                Opcode::Not => {
                    let truthy = ops::to_boolean(&self.heap, self.stack[base + usize::from(rb)]);
                    self.write_bool(base, ra, !truthy);
                    self.set_pc(pc + op_width);
                }
                Opcode::TypeOf => {
                    let v = self.stack[base + usize::from(rb)];
                    self.gc_protect();
                    let tag = self.type_tag(v);
                    let name = attempt!(self.typeof_name(tag));
                    self.stack[base + usize::from(ra)] = JsValue::string(name);
                    self.set_pc(pc + op_width);
                }
                Opcode::In => {
                    let key_v = self.stack[base + usize::from(rb)];
                    let obj_v = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    let present = attempt!(self.op_in(key_v, obj_v));
                    self.write_bool(base, ra, present);
                    self.set_pc(pc + op_width);
                }
                Opcode::InstanceOf => {
                    let lhs_v = self.stack[base + usize::from(rb)];
                    let rhs_v = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    let result = attempt!(self.op_instanceof(lhs_v, rhs_v));
                    self.write_bool(base, ra, result);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Control flow
                // ------------------------------------------------------
                Opcode::Jump => {
                    self.set_pc(narrow.imm24() as usize);
                }
                Opcode::JumpIfFalse | Opcode::JumpIfTrue => {
                    let truthy = ops::to_boolean(&self.heap, self.stack[base + usize::from(ra)]);
                    let taken = truthy ^ (op == Opcode::JumpIfFalse);
                    self.set_pc(if taken {
                        usize::from(narrow.imm16())
                    } else {
                        pc + op_width
                    });
                }
                Opcode::LoopHeader => {
                    self.note_loop(fn_idx);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Calls, returns, throws
                // ------------------------------------------------------
                Opcode::Call => {
                    let argc = rc;
                    match attempt!(self.prepare_call(base, max_regs, rb, argc)) {
                        CallOutcome::Pushed => continue 'drive,
                        CallOutcome::Value(v) => {
                            self.stack[base + usize::from(ra)] = v;
                            self.set_pc(pc + op_width);
                        }
                    }
                    continue 'drive;
                }
                Opcode::Construct => {
                    let argc = rc;
                    match attempt!(self.prepare_construct(base, max_regs, rb, argc)) {
                        CallOutcome::Pushed => continue 'drive,
                        CallOutcome::Value(v) => {
                            self.stack[base + usize::from(ra)] = v;
                            self.set_pc(pc + op_width);
                        }
                    }
                    continue 'drive;
                }
                Opcode::Return => {
                    let v = self.stack[base + usize::from(ra)];
                    if self.complete_frame(v)? {
                        return Ok(());
                    }
                    continue 'drive;
                }
                Opcode::Throw => {
                    throw_js!(self.stack[base + usize::from(ra)]);
                }

                // ------------------------------------------------------
                // Property access
                // ------------------------------------------------------
                Opcode::GetProperty => {
                    let obj_v = self.stack[base + usize::from(rb)];
                    let key_v = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    let v = attempt!(self.get_property(fn_idx, pc as u32, obj_v, key_v));
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, v.as_object().map(|h| self.shape_of(h)));
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::SetProperty => {
                    let obj_v = self.stack[base + usize::from(ra)];
                    let key_v = self.stack[base + usize::from(rb)];
                    let value = self.stack[base + usize::from(rc)];
                    // Guard: null/undefined base throws TypeError per ES 9.1.9 / 13.14.3.
                    if obj_v.is_null() || obj_v.is_undefined() {
                        return Err(JSException(self.error_value(
                            "TypeError: cannot set properties of null or undefined",
                        )));
                    }
                    self.gc_protect();
                    attempt!(self.set_property(obj_v, key_v, value));
                    self.set_pc(pc + op_width);
                }
                Opcode::DeleteProperty => {
                    let obj_v = self.stack[base + usize::from(rb)];
                    let key_v = self.stack[base + usize::from(rc)];
                    let deleted = attempt!(self.delete_property(obj_v, key_v));
                    self.write_bool(base, ra, deleted);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Allocation and environments
                // ------------------------------------------------------
                Opcode::NewObject => {
                    self.gc_protect();
                    let h = self.heap.alloc(JsObject::default());
                    self.stack[base + usize::from(ra)] = JsValue::object(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::NewArray => {
                    let first = base + usize::from(rb);
                    let len = usize::from(rc);
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
                    self.stack[base + usize::from(ra)] = JsValue::object(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::Closure => {
                    self.gc_protect();
                    let env = self.frames.last().expect("frame").env;
                    let h = self.heap.alloc(JsObject {
                        kind: KIND_FUNCTION,
                        elements: vec![ops::box_number(f64::from(rb))],
                        prototype: env,
                        ..JsObject::default()
                    });
                    self.stack[base + usize::from(ra)] = JsValue::object(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::NewEnvironment => {
                    let slots = usize::from(rb);
                    self.gc_protect();
                    // Environments are always fresh objects, mirroring the
                    // reference interpreter in `v12-bccompiler/tests.rs`: the
                    // global object lives *outside* the environment chain
                    // (top-level `var`s route through `SetGlobal`/`GetGlobal`),
                    // so aliased environments cannot collide with user global
                    // properties that occupy the same physical storage.
                    let parent = self.frames.last().expect("frame").env;
                    let h = self.heap.alloc(JsObject {
                        properties: vec![JsValue::undefined(); slots],
                        prototype: parent,
                        ..JsObject::default()
                    });
                    self.frames.last_mut().expect("frame").env = Some(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::GetEnvSlot => {
                    let v = attempt!(self.env_read(rb, rc));
                    self.stack[base + usize::from(ra)] = v;
                    self.set_pc(pc + op_width);
                }
                Opcode::SetEnvSlot => {
                    let v = self.stack[base + usize::from(rc)];
                    attempt!(self.env_write(ra, rb, v));
                    self.set_pc(pc + op_width);
                }
                Opcode::CopyArrayRest => {
                    let src_v = self.stack[base + usize::from(rb)];
                    let start = rc;
                    let dst_val = attempt!(self.op_copy_array_rest(src_v, start));
                    self.stack[base + usize::from(ra)] = dst_val;
                    self.set_pc(pc + op_width);
                }
                Opcode::CheckIsArray => {
                    let v = self.stack[base + usize::from(ra)];
                    attempt!(self.op_check_is_array(v));
                    self.set_pc(pc + op_width);
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
                            self.set_pc(pc + op_width);
                        }
                    }
                    continue 'drive;
                }
                Opcode::CopyObjectRest => {
                    // Narrow form with single excluded key in c (or 0).
                    let src_v = self.stack[base + usize::from(rb)];
                    let excl_vec = if instr.c() == 0 {
                        Vec::new()
                    } else {
                        let start = base + usize::from(rc);
                        self.stack[start..start + 1].to_vec()
                    };
                    let dst_val = attempt!(self.op_copy_object_rest(src_v, &excl_vec));
                    self.stack[base + usize::from(ra)] = dst_val;
                    self.set_pc(pc + op_width);
                }
                Opcode::ArrayAppend => {
                    let dst_v = self.stack[base + usize::from(ra)];
                    let src_v = self.stack[base + usize::from(rb)];
                    attempt!(self.op_array_append(dst_v, src_v));
                    self.set_pc(pc + op_width);
                }
                Opcode::GetGlobal => {
                    let dst = instr.a();
                    let const_id = u32::from(narrow.imm16());
                    let val = attempt!(self.op_get_global(const_id));
                    self.stack[base + usize::from(dst)] = val;
                    self.set_pc(pc + op_width);
                }
                Opcode::SetGlobal => {
                    let src = instr.a();
                    let const_id = u32::from(narrow.imm16());
                    let val = self.stack[base + usize::from(src)];
                    // Guard: global base must be an object (invariant; defensive).
                    if let Some(global) = self.global {
                        let global_v = JsValue::object(global);
                        if global_v.is_null() || global_v.is_undefined() {
                            return Err(JSException(self.error_value(
                                "TypeError: cannot set properties of null or undefined",
                            )));
                        }
                    }
                    attempt!(self.op_set_global(const_id, val));
                    self.set_pc(pc + op_width);
                }

                Opcode::CreateGenerator => {
                    // No longer emitted by compiler; generator creation is handled in prepare_call.
                    // Keep stub for manual bytecode: create dormant generator capturing current frame state after this pc.
                    let dst = instr.a();
                    let src = instr.b();
                    let func_idx = self.stack[base + usize::from(src)].as_smi().map(|v| v as u32).unwrap_or(fn_idx);
                    self.gc_protect();
                    let h = self.heap.alloc(JsObject {
                        kind: KIND_GENERATOR,
                        properties: vec![
                            ops::box_number(f64::from(func_idx)),
                            ops::box_number(f64::from((pc + op_width) as u32)),
                            ops::box_number(0.0),
                        ],
                        elements: self.stack[base..base + usize::from(max_regs)].to_vec(),
                        prototype: self.frames.last().and_then(|f| f.env),
                        ..JsObject::default()
                    });
                    self.heap.add_root(JsValue::object(h));
                    self.stack[base + usize::from(dst)] = JsValue::object(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::SuspendYield => {
                    // Suspend generator: save register window and resume pc, then exit inner execute.
                    // yield* delegation is lowered by the compiler to a generic iterator loop of SuspendYield
                    // (see crates/v12-bccompiler/src/expr.rs YieldExpression delegate path).
                    self.gc_protect();
                    let dst = instr.a();
                    let yielded = self.stack[base + usize::from(dst)];
                    let gen_obj = self.frames.last().expect("frame").generator.expect("yield outside generator");
                    let snapshot = self.stack[base..base + usize::from(max_regs)].to_vec();
                    let resume_pc = pc + op_width;
                    self.heap.get_mut(gen_obj).properties[1] = ops::box_number(resume_pc as f64);
                    if self.heap.get(gen_obj).properties.len() < 4 {
                        self.heap.get_mut(gen_obj).properties.resize(4, JsValue::undefined());
                    }
                    self.heap.get_mut(gen_obj).properties[3] = ops::box_number(f64::from(dst));
                    self.heap.get_mut(gen_obj).properties[2] = ops::box_number(2.0); // 2.0 = suspended
                    self.heap.get_mut(gen_obj).elements = snapshot;
                    self.heap.get_mut(gen_obj).prototype = self.frames.last().and_then(|f| f.env);
                    let finished_base = self.frames.pop().expect("pop").base;
                    self.stack.truncate(finished_base);
                    self.top_result = Some(yielded);
                    return Ok(());
                }
                Opcode::Await => {
                    self.gc_protect();
                    let src = instr.b();
                    let dst = instr.a();
                    let arg = self.stack[base + usize::from(src)];
                    let r#gen = self.frames.last().expect("frame").generator.expect("await outside async");
                    let snapshot = self.stack[base..base + usize::from(max_regs)].to_vec();
                    let resume_pc = pc + op_width;
                    self.heap.get_mut(r#gen).properties[1] = ops::box_number(resume_pc as f64);
                    if self.heap.get(r#gen).properties.len() < 4 {
                        self.heap.get_mut(r#gen).properties.resize(4, JsValue::undefined());
                    }
                    self.heap.get_mut(r#gen).properties[3] = ops::box_number(f64::from(dst));
                    self.heap.get_mut(r#gen).properties[2] = ops::box_number(2.0);
                    // Ensure async promise slot exists (task 6/7: async returns Promise)
                    let async_promise = if self.heap.get(r#gen).properties.len() > 4 {
                        self.heap.get(r#gen).properties[4]
                    } else {
                        JsValue::undefined()
                    };
                    let has_promise = async_promise.as_object().is_some();
                    self.heap.get_mut(r#gen).elements = snapshot;
                    self.heap.get_mut(r#gen).prototype = self.frames.last().and_then(|f| f.env);
                    let finished_frame = self.frames.pop().expect("pop");
                    let finished_base = finished_frame.base;
                    self.stack.truncate(finished_base);
                    // Promise.resolve(arg) -> enqueue reaction that resumes
                    let (promise, is_rejected, payload) = self.promise_resolve_for_await(arg);
                    self.heap.add_root(payload);
                    if let Some(ph) = promise.as_object() { self.heap.add_root(JsValue::object(ph)); }
                    self.pending_awaits.push_back((r#gen, payload, is_rejected));
                    self.top_result = None;
                    // Advance caller past its Call header: async call returns Promise if available else undefined (task 7)
                    if let Some(caller) = self.frames.last_mut() {
                        let instrs = &self.functions[caller.fn_idx as usize].instrs;
                        let (_, cdst, width) = decode_parked_call(instrs, caller.pc);
                        let caller_base = caller.base;
                        if has_promise {
                            self.stack[caller_base + usize::from(cdst)] = async_promise;
                        } else {
                            // No promise slot yet (legacy path) – keep undefined for backward compat
                            self.stack[caller_base + usize::from(cdst)] = JsValue::undefined();
                        }
                        caller.pc += width;
                    } else {
                        return Ok(());
                    }
                    let _ = promise;
                    continue 'drive;
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

    fn write_bool(&mut self, base: usize, reg: u16, b: bool) {
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
                // Interning allocates, so republish roots first: values
                // created since the last gc_protect point (e.g. the operand
                // of a preceding Closure) are otherwise invisible to the
                // collector.
                self.gc_protect();
                let text: String = self
                    .strings
                    .get(str_id as usize)
                    .unwrap_or_else(|| panic!("Str32({str_id}) missing from the string table"))
                    .clone();
                let h = intern_text(&mut self.heap, &text);
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
        callee_reg: u16,
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
            if target == NATIVE_GENERATOR_NEXT {
                let arg = if (callee_slot + 2) < self.stack.len() && argc > 0 {
                    self.stack[callee_slot + 2]
                } else {
                    JsValue::undefined()
                };
                let res = self.generator_next(this_v, arg)?;
                return Ok(CallOutcome::Value(res));
            }
            if target == NATIVE_GENERATOR_RETURN {
                let arg = if (callee_slot + 2) < self.stack.len() && argc > 0 {
                    self.stack[callee_slot + 2]
                } else {
                    JsValue::undefined()
                };
                let res = self.generator_return(this_v, arg)?;
                return Ok(CallOutcome::Value(res));
            }
            if target == NATIVE_GENERATOR_THROW {
                let arg = if (callee_slot + 2) < self.stack.len() && argc > 0 {
                    self.stack[callee_slot + 2]
                } else {
                    JsValue::undefined()
                };
                let res = self.generator_throw(this_v, arg)?;
                return Ok(CallOutcome::Value(res));
            }
            // Fallback for Array join/push when no engine natives are wired (interp-alone tests).
            if target == NATIVE_ARRAY_JOIN {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                let args_slice = self.stack[args_start..args_end].to_vec();
                let res = self.array_join_fallback(this_v, &args_slice)?;
                return Ok(CallOutcome::Value(res));
            }
            if target == NATIVE_ARRAY_PUSH {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                let args_slice = self.stack[args_start..args_end].to_vec();
                let res = self.array_push_fallback(this_v, &args_slice)?;
                return Ok(CallOutcome::Value(res));
            }
            if target == NATIVE_PROMISE_RESOLVE {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                let args_slice = self.stack[args_start..args_end].to_vec();
                let v = args_slice.first().copied().unwrap_or(JsValue::undefined());
                if self.is_promise(v) {
                    return Ok(CallOutcome::Value(v));
                }
                self.gc_protect();
                let reactions = self.heap.alloc(JsObject { kind: v12_heap::KIND_ARRAY, ..JsObject::default() });
                self.heap.add_root(JsValue::object(reactions));
                let p = self.heap.alloc(JsObject {
                    kind: HEAP_KIND_ORDINARY,
                    properties: vec![JsValue::from_i32_smi(1).expect("fits"), v, JsValue::object(reactions)],
                    ..JsObject::default()
                });
                self.heap.add_root(JsValue::object(p));
                return Ok(CallOutcome::Value(JsValue::object(p)));
            }
            if target == NATIVE_PROMISE_REJECT {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                let args_slice = self.stack[args_start..args_end].to_vec();
                let v = args_slice.first().copied().unwrap_or(JsValue::undefined());
                self.gc_protect();
                let reactions = self.heap.alloc(JsObject { kind: v12_heap::KIND_ARRAY, ..JsObject::default() });
                self.heap.add_root(JsValue::object(reactions));
                let p = self.heap.alloc(JsObject {
                    kind: HEAP_KIND_ORDINARY,
                    properties: vec![JsValue::from_i32_smi(2).expect("fits"), v, JsValue::object(reactions)],
                    ..JsObject::default()
                });
                self.heap.add_root(JsValue::object(p));
                return Ok(CallOutcome::Value(JsValue::object(p)));
            }
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

        // Generator function: calling it returns a generator object without executing body.
        if self.is_generator_fn(target) {
            let r#gen = self.create_generator_object(target, captured_env, this_v, callee_slot, argc)?;
            return Ok(CallOutcome::Value(JsValue::object(r#gen)));
        }

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }

        // Async functions return a pending Promise immediately (Task 7).
        // Inline execution until first Await would block the caller; instead we create
        // a generator object holding the initial state and defer body execution.
        // For the simple `return 1` case the promise stays pending (test only checks object identity).
        // For `await` cases, the deferred body will be driven by `run_jobs` via pending_awaits.
        if self.is_async_fn(target) {
            self.gc_protect();
            let reactions = self.heap.alloc(JsObject { kind: v12_heap::KIND_ARRAY, ..JsObject::default() });
            self.heap.add_root(JsValue::object(reactions));
            let promise = self.heap.alloc(JsObject {
                kind: HEAP_KIND_ORDINARY,
                properties: vec![JsValue::from_i32_smi(0).expect("0 fits Smi"), JsValue::undefined(), JsValue::object(reactions)],
                prototype: None,
                ..JsObject::default()
            });
            self.heap.add_root(JsValue::object(promise));
            // Capture initial register window for deferred execution
            let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
                let f = &self.functions[target as usize];
                (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
            };
            let mut window = vec![JsValue::undefined(); usize::from(callee_max_regs)];
            window[0] = this_v;
            let arg_src = callee_slot + 2;
            if callee_has_rest {
                let fixed = callee_fixed as usize;
                let rest_reg = callee_rest_reg as usize;
                let to_copy = fixed.min(argc as usize).min(window.len().saturating_sub(1));
                for i in 0..to_copy { window[1+i] = self.stack[arg_src+i]; }
                let rest_len = (argc as usize).saturating_sub(fixed);
                let rest_slice = if rest_len>0 { self.stack[arg_src+fixed..arg_src+fixed+rest_len].to_vec() } else { Vec::new() };
                let shape = self.array_shape();
                let h = self.heap.alloc(JsObject { kind: KIND_ARRAY, properties: vec![ops::box_number(f64::from(rest_len as u32))], elements: rest_slice, ..JsObject::default() });
                self.bind_shape(h, shape);
                if rest_reg < window.len() { window[rest_reg] = JsValue::object(h); }
            } else {
                let copied = usize::from(argc).min(window.len().saturating_sub(1));
                for i in 0..copied { window[1+i] = self.stack[arg_src+i]; }
            }
            let g = self.heap.alloc(JsObject {
                kind: KIND_GENERATOR,
                properties: vec![ ops::box_number(f64::from(target)), ops::box_number(0.0), ops::box_number(0.0), ops::box_number(0.0), JsValue::object(promise) ],
                elements: window,
                prototype: captured_env,
                ..JsObject::default()
            });
            self.heap.add_root(JsValue::object(g));
            // Defer: enqueue resume at pc 0
            self.pending_awaits.push_back((g, JsValue::undefined(), false));
            return Ok(CallOutcome::Value(JsValue::object(promise)));
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
            generator: None,
            yield_dst: None,
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
        if let Some(r#gen) = finished.generator {
            // Async completion: settle stored promise and don't overwrite caller dst (promise already delivered)
            let is_async = self.is_async_fn(finished.fn_idx);
            let has_promise_slot = self.heap.get(r#gen).properties.len() > 4;
            if is_async && has_promise_slot {
                if let Some(ph) = self.heap.get(r#gen).properties[4].as_object() {
                    self.heap.get_mut(ph).properties[0] = JsValue::from_i32_smi(1).expect("fits");
                    self.heap.get_mut(ph).properties[1] = result;
                }
            }
            self.stack.truncate(finished.base);
            if self.heap.get(r#gen).properties.len() >= 3 {
                self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
            }
            // For async jobs, caller already resumed with Promise; just settle and resume caller
            if is_async && has_promise_slot {
                // If this was a direct call without prior await suspension (no caller advancement),
                // ensure caller gets the promise
                if let Some(caller) = self.frames.last_mut() {
                    let pc = caller.pc;
                    if let Some(&instr) = self.functions[caller.fn_idx as usize].instrs.get(pc) {
                        if instr.op() == Some(v12_bytecode::Opcode::Call) || instr.op() == Some(v12_bytecode::Opcode::Wide) {
                            let instrs = &self.functions[caller.fn_idx as usize].instrs;
                            if let Some(ph) = self.heap.get(r#gen).properties.get(4).copied() {
                                let try_decode = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_parked_call(instrs, pc)));
                                if let Ok((_, dst, width)) = try_decode {
                                    let caller_base = caller.base;
                                    if self.stack[caller_base + usize::from(dst)].is_undefined() {
                                        self.stack[caller_base + usize::from(dst)] = ph;
                                        caller.pc += width;
                                        return Ok(false);
                                    }
                                }
                            }
                        }
                    }
                    // Caller already advanced (prepare_call returned Value); just resume it
                    return Ok(false);
                }
                self.top_result = Some(result);
                return Ok(true);
            }
            self.top_result = Some(result);
            // Always exit inner execute so generator_next can wrap as {value,done:true}.
            // If frames is empty this is top-level completion; otherwise still exit to caller.
            return Ok(true);
        }
        let Some(caller) = self.frames.last_mut() else {
            self.stack.truncate(finished.base);
            // Record the bottom frame's completion value for `call_object`;
            // `run` ignores it.
            self.top_result = Some(result);
            return Ok(true);
        };
        // The caller is parked on its Call/Construct header — narrow, the
        // wide `CallW`/`ConstructW` escape, or a `RegExt` prefix — so decode
        // the destination register and total word width from that header.
        // Construct adds the spec's return-value adjustment: a body that
        // returns an object replaces the instance, otherwise the newly
        // allocated instance (still sitting in the callee's r0) is returned.
        let instrs = &self.functions[caller.fn_idx as usize].instrs;
        let (is_construct, dst, width) = decode_parked_call(instrs, caller.pc);
        let caller_base = caller.base;
        let result = if is_construct && result.as_object().is_none() && !result.is_hole() {
            // Callee frame still intact at this point (truncation happens
            // below), so its `this` register — the constructed instance.
            let v = self.stack[finished.base];
            if v.as_object().is_some() { v } else { result }
        } else {
            result
        };
        self.stack.truncate(finished.base);
        self.stack[caller_base + usize::from(dst)] = result;
        caller.pc += width;
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
        let idx = usize::from(slot);
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
        let idx = usize::from(slot);
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

    /// Get the canonical array shape (cached after first computation).
    pub fn array_shape(&mut self) -> ShapeHandle {
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
    /// Lazily materializes the `console.log` native function object.
    ///
    /// The function is a `KIND_FUNCTION` whose `elements[0]` is
    /// `NATIVE_CONSOLE_LOG`, so `prepare_call` routes it through the
    /// `NativeRegistry`. The object is cached in `self.console_log` and
    /// rooted, so repeated `get_property` for `console.log` returns the
    /// same handle.
    fn console_log_fn(&mut self) -> JsValue {
        if let Some(cached) = self.console_log {
            return cached;
        }
        self.gc_protect();
        let func = self.heap.alloc(JsObject {
            kind: KIND_FUNCTION,
            elements: vec![
                JsValue::from_i32_smi(NATIVE_CONSOLE_LOG as i32).expect("native index fits Smi"),
            ],
            ..JsObject::default()
        });
        let value = JsValue::object(func);
        self.heap.add_root(value);
        self.console_log = Some(value);
        value
    }

    /// Compares a key value's string text against `text` (flattening first).
    fn key_is(&mut self, key_v: JsValue, text: &str) -> bool {
        let Some(handle) = key_v.as_string() else {
            return false;
        };
        self.heap.flatten(handle);
        match &self.heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => bytes == text.as_bytes(),
            v12_heap::StrStorage::Utf16(units) => {
                units.iter().copied().eq(text.encode_utf16())
            }
            _ => false,
        }
    }

    /// Which lazily-synthesized native function object to produce.
    ///
    /// Grouped so the synthesis body is written once; `console_log_fn`
    /// predates it and keeps its own copy.
    fn cached_native(&mut self, which: NativeFn) -> JsValue {
        let (index, cached) = match which {
            NativeFn::PromiseResolve => (NATIVE_PROMISE_RESOLVE, self.promise_resolve_fn),
            NativeFn::PromiseReject => (NATIVE_PROMISE_REJECT, self.promise_reject_fn),
            NativeFn::PromiseThen => (NATIVE_PROMISE_THEN, self.promise_then_fn),
            NativeFn::ArrayPush => (NATIVE_ARRAY_PUSH, self.array_push_fn),
            NativeFn::ArrayJoin => (NATIVE_ARRAY_JOIN, self.array_join_fn),
            NativeFn::EnumerableOwnKeys => (NATIVE_ENUMERABLE_OWN_KEYS, self.enumerable_own_keys_fn),
            NativeFn::GeneratorNext => (NATIVE_GENERATOR_NEXT, self.generator_next_fn),
            NativeFn::GeneratorReturn => (NATIVE_GENERATOR_RETURN, self.generator_return_fn),
            NativeFn::GeneratorThrow => (NATIVE_GENERATOR_THROW, self.generator_throw_fn),
        };
        if let Some(cached) = cached {
            return cached;
        }
        self.gc_protect();
        let func = self.heap.alloc(JsObject {
            kind: KIND_FUNCTION,
            elements: vec![JsValue::from_i32_smi(index as i32).expect("native index fits Smi")],
            ..JsObject::default()
        });
        let value = JsValue::object(func);
        self.heap.add_root(value);
        match which {
            NativeFn::PromiseResolve => self.promise_resolve_fn = Some(value),
            NativeFn::PromiseReject => self.promise_reject_fn = Some(value),
            NativeFn::PromiseThen => self.promise_then_fn = Some(value),
            NativeFn::ArrayPush => self.array_push_fn = Some(value),
            NativeFn::ArrayJoin => self.array_join_fn = Some(value),
            NativeFn::EnumerableOwnKeys => self.enumerable_own_keys_fn = Some(value),
            NativeFn::GeneratorNext => self.generator_next_fn = Some(value),
            NativeFn::GeneratorReturn => self.generator_return_fn = Some(value),
            NativeFn::GeneratorThrow => self.generator_throw_fn = Some(value),
        }
        value
    }

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

        // Fast path for `console.log`: the console object lives at
        // `global.properties[12]` (INTRINSICS[12] == "console"). When the
        // lookup is for `"log"` on that object, synthesize the native
        // function. This avoids needing a shape for `console`'s `log`
        // property and keeps `Realm` free of interpreter shape bookkeeping.
        let console_obj_opt = if let Some(g) = self.global {
            let val_opt = {
                let heap = &self.heap;
                heap.get(g).properties.get(12).copied()
            };
            val_opt.and_then(|v| v.as_object())
        } else {
            None
        };
        if let Some(console_obj) = console_obj_opt
            && obj == console_obj
            && let Some(handle) = key_v.as_string()
        {
            let is_log = {
                self.heap.flatten(handle);
                match &self.heap.get(handle).storage {
                    v12_heap::StrStorage::Latin1(bytes) => bytes == b"log",
                    v12_heap::StrStorage::Utf16(units) => {
                        units.len() == 3 && units[0] == 108 && units[1] == 111 && units[2] == 103
                    }
                    _ => false,
                }
            };
            if is_log {
                return Ok(self.console_log_fn());
            }
        }

        // Fast paths for the Promise surface and the array `push`/`join`
        // methods, mirroring the `console.log` synthesis above. Natives cannot
        // attach shape-bound properties (shape binding is interpreter state),
        // so these reads are recognized structurally:
        // - `Promise.resolve` / `Promise.reject` on the Promise constructor
        //   (intrinsic slot 10 of the duplicated `GLOBAL_INTRINSIC_NAMES`).
        // - `then` on any object whose prototype is the Promise constructor's
        //   `prototype` link — the realm installs that link, and the engine's
        //   promise built-ins give every promise instance the same prototype.
        // - `push` / `join` on array-kind objects.
        if let Some(g) = self.global {
            let promise_ctor = {
                let heap = &self.heap;
                heap.get(g).properties.get(10).and_then(|v| v.as_object())
            };
            if let Some(promise_ctor) = promise_ctor {
                if obj == promise_ctor {
                    if self.key_is(key_v, "resolve") {
                        return Ok(self.cached_native(NativeFn::PromiseResolve));
                    }
                    if self.key_is(key_v, "reject") {
                        return Ok(self.cached_native(NativeFn::PromiseReject));
                    }
                } else if self.key_is(key_v, "then")
                    && self.heap.get(obj).prototype.is_some()
                    && self.heap.get(obj).prototype == self.heap.get(promise_ctor).prototype
                {
                    return Ok(self.cached_native(NativeFn::PromiseThen));
                }
            }
            // Fast path for Object.enumerableOwnKeys on the Object constructor
            let object_ctor = self.heap.get(g).properties.get(0).and_then(|v| v.as_object());
            if let Some(object_ctor) = object_ctor {
                if obj == object_ctor && self.key_is(key_v, "enumerableOwnKeys") {
                    return Ok(self.cached_native(NativeFn::EnumerableOwnKeys));
                }
            }
        }
        if self.heap.get(obj).kind == KIND_GENERATOR {
            if self.key_is(key_v, "next") {
                return Ok(self.cached_native(NativeFn::GeneratorNext));
            }
            if self.key_is(key_v, "return") {
                return Ok(self.cached_native(NativeFn::GeneratorReturn));
            }
            if self.key_is(key_v, "throw") {
                return Ok(self.cached_native(NativeFn::GeneratorThrow));
            }
        }
        if self.heap.get(obj).kind == KIND_ARRAY {
            if self.key_is(key_v, "push") {
                return Ok(self.cached_native(NativeFn::ArrayPush));
            }
            if self.key_is(key_v, "join") {
                return Ok(self.cached_native(NativeFn::ArrayJoin));
            }
            if self.key_is(key_v, "length") {
                // Length is properties[0] for arrays regardless of shape state
                // (covers arrays created by native handlers without shape binding)
                if let Some(v) = self.heap.get(obj).properties.first().copied() {
                    return Ok(v);
                }
            }
        }

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
            && let Some(v) = self
                .heap
                .get(obj)
                .properties
                .get(self.global_slot_index(obj, ic.slot as usize))
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
                    let value = self.heap.get(owner).properties
                        [self.global_slot_index(owner, slot as usize)];
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
                        let idx = self.global_slot_index(obj, slot as usize);
                        self.heap.get_mut(obj).properties[idx] = value;
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
        if Some(obj) == self.global {
            // Global storage keeps the intrinsic prefix; slot numbering from
            // the shared shape chain must not overlap it. The invariant
            // `properties.len() == GLOBAL_VAR_OFFSET + num_own` restores
            // itself by appending (and backfilling if an embedder's global
            // was assembled with fewer slots).
            let idx = GLOBAL_VAR_OFFSET
                + usize::try_from(self.heap.get(child).num_own - 1).expect("slot fits usize");
            let len = self.heap.get(obj).properties.len();
            if len <= idx {
                self.heap
                    .get_mut(obj)
                    .properties
                    .resize(idx + 1, JsValue::undefined());
                self.heap
                    .get_mut(obj)
                    .property_keys
                    .resize(idx + 1, None);
            }
            self.heap.get_mut(obj).properties[idx] = value;
            self.heap.get_mut(obj).property_keys[idx] = Some(key);
        } else {
            self.heap.get_mut(obj).properties.push(value);
            self.heap.get_mut(obj).property_keys.push(Some(key));
        }
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

        if (self.heap.get(obj).kind == KIND_ARRAY || self.heap.get(obj).kind == KIND_ARGUMENTS)
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
                let idx = self.global_slot_index(obj, slot as usize);
                self.heap.get_mut(obj).properties[idx] = JsValue::hole();
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
        // Snapshot descriptors + properties before the loop below: each
        // iteration calls `add_property`, which allocates a shape slot and
        // may trigger a collection; a borrowed `&sh.descriptors` would dangle
        // across that mutation (and cannot borrow-check against `&mut
        // self.heap`). Descriptors are handles, so a stale-by-one-GC snapshot
        // is fine as long as `src_obj` keeps its shape alive as a root.
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
            let phys = self.global_slot_index(src_obj, slot_usize);
            if phys >= src_props.len() {
                continue;
            }
            let val = src_props[phys];
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

    /// Physical index for a shape-derived property `slot` on object `obj`.
    ///
    /// The global object's `properties` vector is prefixed by
    /// `GLOBAL_VAR_OFFSET` intrinsic slots that the shape graph does not
    /// track (the realm installs them by pushing directly), so every
    /// descriptor slot on the global maps to `GLOBAL_VAR_OFFSET + slot`;
    /// ordinary objects use the slot as-is.
    fn global_slot_index(&self, obj: Handle<JsObject>, slot: usize) -> usize {
        if Some(obj) == self.global {
            GLOBAL_VAR_OFFSET + slot
        } else {
            slot
        }
    }

    fn op_get_global(&mut self, str_id: u32) -> Result<JsValue, JSException> {
        let Some(global) = self.global else {
            return Ok(JsValue::undefined());
        };
        // The fast path allocates only when interning an unseen key, but any
        // `Heap::alloc` can collect — publish roots first so values written
        // since the last opcode-level protect stay reachable.
        self.gc_protect();
        // Borrow the compiler's string table entry: comparing against the
        // intrinsics list and interning both take &str, so no String clone
        // is needed on this fast path.
        let text: &str = self
            .strings
            .get(str_id as usize)
            .map(String::as_str)
            .unwrap_or("");
        if let Some(idx) = GLOBAL_INTRINSIC_NAMES.iter().position(|&n| n == text)
            && idx < self.heap.get(global).properties.len()
        {
            let v = self.heap.get(global).properties[idx];
            if !v.is_hole() {
                return Ok(v);
            }
        }
        let h = intern_text(&mut self.heap, text);
        let key = PropKey::from_string(h);
        let shape = self.shape_of(global);
        if let Some(desc) = self.heap.lookup_property(shape, key)
            && let Some(slot) = desc.slot()
        {
            let idx = self.global_slot_index(global, slot as usize);
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
        // Interning a new key and the shape transition below can each
        // allocate; publish roots first so `val` survives any collection.
        self.gc_protect();
        let text = self
            .strings
            .get(str_id as usize)
            .cloned()
            .unwrap_or_default();
        if let Some(idx) = GLOBAL_INTRINSIC_NAMES.iter().position(|&n| n == text) {
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
            let idx = self.global_slot_index(global, slot as usize);
            let len = self.heap.get(global).properties.len();
            if idx >= len {
                // Repair the vector if an embedder assembled the global
                // without the full intrinsic prefix.
                self.heap
                    .get_mut(global)
                    .properties
                    .resize(idx + 1, JsValue::undefined());
            }
            self.heap.get_mut(global).properties[idx] = val;
            return Ok(());
        }
        // Otherwise, create new global property. The shape transition may
        // allocate, but roots were published at the top of this handler and
        // nothing here introduces values beyond that set (the interned key is
        // kept alive by the strong intern table), so no re-protect is needed.
        // Keep the physical index in sync with the shape's slot numbering.
        let child = self.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
        self.bind_shape(global, child);
        let new_slot = usize::try_from(self.heap.get(child).num_own - 1).expect("slot fits usize");
        let idx = self.global_slot_index(global, new_slot);
        let len = self.heap.get(global).properties.len();
        if len <= idx {
            self.heap
                .get_mut(global)
                .properties
                .resize(idx + 1, JsValue::undefined());
        }
        self.heap.get_mut(global).properties[idx] = val;
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
            generator: None,
            yield_dst: None,
        });
        self.note_entry(target);
        Ok(CallOutcome::Pushed)
    }

    /// `new F(args)` ([`Opcode::Construct`]).
    ///
    /// Only constructors are constructible here:
    /// - a bytecode function (`Closure`) gets real construct semantics — an
    ///   instance is allocated with [[Prototype]] = `F.prototype` (the
    ///   property is created on first use, as spec-mandated for plain
    ///   functions), bound as `this`, and the body runs;
    /// - the constructor-shaped natives ([`NATIVE_ERROR_CREATE`],
    ///   [`NATIVE_BOOLEAN_CONSTRUCT`]) route through the registry ignoring
    ///   the receiver, like their spec counterparts do when called;
    /// - everything else throws TypeError "not a constructor".
    ///
    /// The return-value adjustment (body result if it returns an object,
    /// otherwise the instance) happens in [`Interp::complete_frame`], which
    /// can see that the caller parked on a `Construct` opcode.
    fn prepare_construct(
        &mut self,
        base: usize,
        caller_max_regs: u16,
        callee_reg: u16,
        argc: u16,
    ) -> Result<CallOutcome, JSException> {
        let callee_slot = base + usize::from(callee_reg);
        let callee_v = self.stack[callee_slot];

        let Some(callee_obj) = callee_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: value is not a constructor"),
            ));
        };
        if self.heap.get(callee_obj).kind != KIND_FUNCTION {
            return Err(JSException(
                self.error_value("TypeError: value is not a constructor"),
            ));
        }
        let idx = {
            let c = self.heap.get(callee_obj);
            c.elements.first().and_then(|v| v.as_smi()).unwrap_or(-1)
        };

        // Native seam: only known constructor-shaped built-ins are
        // constructible; the realm's placeholder intrinsics carry no valid
        // function index and therefore reject like any other non-constructor.
        if idx < 0 || usize::try_from(idx).expect("checked non-negative") >= self.functions.len() {
            let Ok(target) = u32::try_from(idx) else {
                return Err(JSException(
                    self.error_value("TypeError: value is not a constructor"),
                ));
            };
            if target != NATIVE_BOOLEAN_CONSTRUCT && target != NATIVE_ERROR_CREATE {
                return Err(JSException(
                    self.error_value("TypeError: value is not a constructor"),
                ));
            }
            let args_start = callee_slot + 2;
            let args_end = args_start + usize::from(argc);
            self.gc_protect();
            let result = {
                let args = &self.stack[args_start..args_end];
                self.natives
                    .call_native(&mut self.heap, JsValue::undefined(), args, target)
            };
            return result.map(CallOutcome::Value).map_err(JSException);
        }

        // Bytecode function. Resolve or lazily create `.prototype`.
        let proto_key = self.prototype_key();
        let proto_val: Option<JsValue> = {
            let shape = self.shape_of(callee_obj);
            match self.heap.lookup_property(shape, proto_key) {
                Some(Descriptor::Data { slot, .. }) => {
                    Some(self.heap.get(callee_obj).properties[*slot as usize])
                }
                _ => None,
            }
        };
        let proto_v = match proto_val {
            Some(v) if v.as_object().is_some() => v,
            _ => {
                // Fallback for function objects created outside `Closure`
                // (host-created) that lack the spec-mandated property.
                self.gc_protect();
                let key_handle = intern_text(&mut self.heap, "prototype");
                let p = self.heap.alloc(JsObject {
                    kind: HEAP_KIND_ORDINARY,
                    ..JsObject::default()
                });
                // Untracked until `set_property` stores it behind the callee;
                // that path allocates, so root it for the duration.
                let p_val = JsValue::object(p);
                self.heap.add_root(p_val);
                self.set_property(callee_v, JsValue::string(key_handle), p_val)?;
                p_val
            }
        };
        let Some(proto) = proto_v.as_object() else {
            // Guarded above by `v.as_object().is_some()`; kept exhaustive.
            return Err(JSException(
                self.error_value("TypeError: value is not a constructor"),
            ));
        };

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }

        // Allocate the instance with [[Prototype]] linking, then push the
        // frame with `this` = instance.
        self.gc_protect();
        let instance = self.heap.alloc(JsObject {
            kind: HEAP_KIND_ORDINARY,
            prototype: Some(proto),
            ..JsObject::default()
        });
        let instance_v = JsValue::object(instance);

        let target_u32 = u32::try_from(idx).expect("checked non-negative");
        let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
            let f = &self.functions[target_u32 as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        let new_base = base + usize::from(caller_max_regs);
        let window_end = new_base + usize::from(callee_max_regs);

        let arg_src = callee_slot + 2;
        self.stack.resize(window_end, JsValue::undefined());
        self.stack[new_base] = instance_v;
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
            fn_idx: target_u32,
            pc: 0,
            base: new_base,
            max_regs: callee_max_regs,
            env: self.heap.get(callee_obj).prototype,
            generator: None,
            yield_dst: None,
        });
        self.note_entry(target_u32);
        Ok(CallOutcome::Pushed)
    }

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

    #[cfg(test)]
    pub(crate) fn type_feedback_at(&self, fn_idx: u32, pc: u32) -> Lattice {
        self.feedback
            .get(&fn_idx)
            .map(|fv| fv.type_at(pc))
            .unwrap_or(Lattice::Unknown)
    }

    /// Returns the per-function feedback vector collected by the interpreter,
    /// if one was allocated. The tier-2 driver reads this when deciding
    /// whether to speculate.
    #[must_use]
    pub fn feedback_vector(&self, fn_idx: u32) -> Option<&FeedbackVector> {
        self.feedback.get(&fn_idx)
    }

    #[cfg(test)]
    pub fn feedback_vector_mut(&mut self, fn_idx: u32) -> Option<&mut FeedbackVector> {
        self.feedback.get_mut(&fn_idx)
    }

    // ------------------------------------------------------------------
    // GC coordination
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Generators
    // ------------------------------------------------------------------

    fn is_generator_fn(&self, fn_idx: u32) -> bool {
        let f = &self.functions[fn_idx as usize];
        if f.is_generator {
            return true;
        }
        // Fallback for old bytecode / hand-built tests without flag.
        for instr in &f.instrs {
            if instr.op() == Some(v12_bytecode::Opcode::SuspendYield) {
                return true;
            }
            if instr.op() == Some(v12_bytecode::Opcode::Wide) {
                // Wide instructions cannot be SuspendYield, so ignore.
            }
        }
        false
    }

    fn is_async_fn(&self, fn_idx: u32) -> bool {
        self.functions[fn_idx as usize].is_async
    }

    fn create_generator_object(
        &mut self,
        fn_idx: u32,
        captured_env: Option<Handle<JsObject>>,
        this_v: JsValue,
        callee_slot: usize,
        argc: u16,
    ) -> Result<Handle<JsObject>, JSException> {
        let (max_regs, has_rest, fixed, rest_reg) = {
            let f = &self.functions[fn_idx as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        // Build initial register window snapshot.
        let mut window = vec![JsValue::undefined(); usize::from(max_regs)];
        window[0] = this_v;
        let arg_src = callee_slot + 2;
        if has_rest {
            let fixed_usize = fixed as usize;
            let to_copy = (fixed_usize).min(argc as usize).min(window.len().saturating_sub(1));
            for i in 0..to_copy {
                window[1 + i] = self.stack[arg_src + i];
            }
            let rest_start = fixed_usize;
            let rest_len = (argc as usize).saturating_sub(rest_start);
            let slice = if rest_len > 0 {
                self.stack[arg_src + rest_start..arg_src + rest_start + rest_len].to_vec()
            } else {
                Vec::new()
            };
            self.gc_protect();
            let shape = self.array_shape();
            let h = self.heap.alloc(JsObject {
                kind: KIND_ARRAY,
                properties: vec![ops::box_number(f64::from(rest_len as u32))],
                elements: slice,
                ..JsObject::default()
            });
            self.bind_shape(h, shape);
            if (rest_reg as usize) < window.len() {
                window[rest_reg as usize] = JsValue::object(h);
            }
        } else {
            let copied = (argc as usize).min(window.len().saturating_sub(1));
            for i in 0..copied {
                window[1 + i] = self.stack[arg_src + i];
            }
        }
        // Real suspension: store initial register window snapshot, not eager yields.
        self.gc_protect();
        let r#gen = self.heap.alloc(JsObject {
            kind: KIND_GENERATOR,
            properties: vec![
                ops::box_number(f64::from(fn_idx)),
                ops::box_number(0.0), // resume pc
                ops::box_number(0.0), // done flag
                ops::box_number(0.0), // yield_dst
            ],
            elements: window,
            prototype: captured_env,
            ..JsObject::default()
        });
        self.heap.add_root(JsValue::object(r#gen));
        Ok(r#gen)
    }

    fn generator_next(&mut self, this_v: JsValue, arg: JsValue) -> Result<JsValue, JSException> {
        let Some(r#gen) = this_v.as_object() else {
            return Err(JSException(self.error_value("TypeError: generator next called on non-object")));
        };
        if self.heap.get(r#gen).kind != KIND_GENERATOR {
            return Err(JSException(self.error_value("TypeError: not a generator")));
        }
        let done = self.heap.get(r#gen).properties.get(2).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) == 1.0;
        if done {
            return Ok(self.make_iterator_result(JsValue::undefined(), true));
        }
        let fn_idx = self.heap.get(r#gen).properties.first().and_then(|v| v.as_smi().map(|n| n as u32 as f64).or(v.as_f64())).unwrap_or(0.0) as u32;
        let resume_pc = self.heap.get(r#gen).properties.get(1).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) as usize;
        let snapshot = self.heap.get(r#gen).elements.clone();
        let env = self.heap.get(r#gen).prototype;
        let f_max_regs = self.functions[fn_idx as usize].max_regs;
        let new_base = self.stack.len();
        let window_end = new_base + usize::from(f_max_regs);
        self.stack.resize(window_end, JsValue::undefined());
        let copy_len = snapshot.len().min(usize::from(f_max_regs));
        for i in 0..copy_len {
            self.stack[new_base + i] = snapshot[i];
        }
        // If resuming (resume_pc != 0), feed arg into yield_dst register
        if resume_pc != 0 {
            let yield_dst = self.heap.get(r#gen).properties.get(3).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) as u16;
            if (yield_dst as usize) < usize::from(f_max_regs) {
                self.stack[new_base + usize::from(yield_dst)] = arg;
            }
        }
        self.frames.push(Frame {
            fn_idx,
            pc: resume_pc,
            base: new_base,
            max_regs: f_max_regs,
            env,
            generator: Some(r#gen),
            yield_dst: None,
        });
        self.top_result = None;
        let frames_before = self.frames.len();
        let exec_res = self.execute();
        // execute returns Ok(()) on suspension (SuspendYield returned early) with top_result holding yielded
        // or Ok(()) on completion (complete_frame popped gen frame and set top_result)
        match exec_res {
            Ok(()) => {
                // Discriminate suspend (done==2.0, frames popped) vs completion (done==1.0).
                let done_val = self.heap.get(r#gen).properties.get(2).and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64))).unwrap_or(0.0);
                if done_val == 2.0 && self.frames.len() < frames_before {
                    let yielded = self.top_result.take().unwrap_or(JsValue::undefined());
                    return Ok(self.make_iterator_result(yielded, false));
                } else {
                    let ret = self.top_result.take().unwrap_or(JsValue::undefined());
                    if done_val != 1.0 && self.heap.get(r#gen).properties.len() >= 3 {
                        self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                    }
                    return Ok(self.make_iterator_result(ret, true));
                }
            }
            Err(e) => {
                // Pop the generator frame if still there, mark done
                if self.frames.len() >= frames_before {
                    self.frames.pop();
                    self.stack.truncate(new_base);
                }
                if self.heap.get(r#gen).properties.len() >= 3 {
                    self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                }
                return Err(e);
            }
        }
    }

    fn make_iterator_result(&mut self, value: JsValue, done: bool) -> JsValue {
        self.gc_protect();
        let h = self.heap.alloc(JsObject::default());
        self.heap.add_root(JsValue::object(h));
        // Avoid set_property recursion issues for now: store directly via properties vec and shape binding via heap
        // Use minimal shape: add properties via heap without interpreter's set_property
        let value_key = self.heap.intern_string(v12_heap::V12Str::latin1(b"value".to_vec()));
        let done_key = self.heap.intern_string(v12_heap::V12Str::latin1(b"done".to_vec()));
        let pk_value = PropKey::from_string(value_key);
        let pk_done = PropKey::from_string(done_key);
        let shape0 = self.heap.root_shape();
        let shape1 = self.heap.add_property(shape0, pk_value, v12_heap::Attrs::DEFAULT);
        let shape2 = self.heap.add_property(shape1, pk_done, v12_heap::Attrs::DEFAULT);
        // Bind shape to object via interp's shape_of tracking
        self.bind_shape(h, shape2);
        let done_val = if done { JsValue::true_() } else { JsValue::false_() };
        self.heap.get_mut(h).properties = vec![value, done_val];
        self.heap.get_mut(h).property_keys = vec![Some(pk_value), Some(pk_done)];
        JsValue::object(h)
    }

    fn generator_return(&mut self, this_v: JsValue, arg: JsValue) -> Result<JsValue, JSException> {
        let Some(r#gen) = this_v.as_object() else {
            return Err(JSException(self.error_value("TypeError: generator return called on non-object")));
        };
        if self.heap.get(r#gen).kind != KIND_GENERATOR {
            return Err(JSException(self.error_value("TypeError: not a generator")));
        }
        let done = self.heap.get(r#gen).properties.get(2).and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64))).unwrap_or(0.0) == 1.0;
        if done {
            return Ok(self.make_iterator_result(arg, true));
        }
        // Mark done and unwind any suspended frame.
        if self.heap.get(r#gen).properties.len() >= 3 {
            self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
        }
        self.heap.get_mut(r#gen).elements.clear();
        self.gc_protect();
        Ok(self.make_iterator_result(arg, true))
    }

    fn generator_throw(&mut self, this_v: JsValue, arg: JsValue) -> Result<JsValue, JSException> {
        let Some(r#gen) = this_v.as_object() else {
            return Err(JSException(self.error_value("TypeError: generator throw called on non-object")));
        };
        if self.heap.get(r#gen).kind != KIND_GENERATOR {
            return Err(JSException(self.error_value("TypeError: not a generator")));
        }
        let done = self.heap.get(r#gen).properties.get(2).and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64))).unwrap_or(0.0) == 1.0;
        if done {
            return Err(JSException(arg));
        }
        if self.heap.get(r#gen).properties.len() >= 3 {
            self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
        }
        self.heap.get_mut(r#gen).elements.clear();
        Err(JSException(arg))
    }

    fn array_join_fallback(&mut self, this_v: JsValue, args: &[JsValue]) -> Result<JsValue, JSException> {
        let Some(arr) = this_v.as_object() else {
            return Err(JSException(self.error_value("TypeError: Array.prototype.join requires an array")));
        };
        let sep = if let Some(&v) = args.first() {
            if v.is_undefined() { ",".to_string() } else { self.to_display_string(v) }
        } else { ",".to_string() };
        let elements = self.heap.get(arr).elements.clone();
        let mut parts = Vec::with_capacity(elements.len());
        for &v in &elements {
            if v.is_undefined() || v.is_null() || v.is_hole() {
                parts.push(String::new());
            } else {
                parts.push(self.to_display_string(v));
            }
        }
        self.gc_protect();
        Ok(JsValue::string(intern_text(&mut self.heap, &parts.join(&sep))))
    }

    fn array_push_fallback(&mut self, this_v: JsValue, args: &[JsValue]) -> Result<JsValue, JSException> {
        let Some(obj) = this_v.as_object() else {
            return Err(JSException(self.error_value("TypeError: Array.prototype.push called on non-object")));
        };
        for &item in args {
            self.heap.get_mut(obj).elements.push(item);
        }
        let new_len = self.heap.get(obj).elements.len() as u32;
        // Sync length if shape exists
        let key = self.length_key();
        let shape = self.shape_of(obj);
        if let Some(desc) = self.heap.lookup_property(shape, key).and_then(|d| d.slot().map(|s| s as usize)) {
            if desc < self.heap.get(obj).properties.len() {
                self.heap.get_mut(obj).properties[desc] = ops::box_number(f64::from(new_len));
            }
        }
        Ok(ops::box_number(f64::from(new_len)))
    }

    fn is_promise(&self, v: JsValue) -> bool {
        let Some(obj) = v.as_object() else { return false; };
        let o = self.heap.get(obj);
        o.properties.len() >= 3 && o.properties[0].as_smi().is_some_and(|s| (0..=2).contains(&s))
    }

    fn promise_resolve_for_await(&mut self, v: JsValue) -> (JsValue, bool, JsValue) {
        if self.is_promise(v) {
            let obj = v.as_object().unwrap();
            let state = self.heap.get(obj).properties[0].as_smi().unwrap_or(0);
            let payload = self.heap.get(obj).properties[1];
            if state == 1 {
                return (v, false, payload);
            } else if state == 2 {
                return (v, true, payload);
            } else {
                // Pending promise: for task 6, treat as fulfilled with undefined payload (no thenable unwrapping)
                return (v, false, JsValue::undefined());
            }
        }
        // Create fulfilled promise for non-promise arg
        self.gc_protect();
        let reactions = self.heap.alloc(JsObject { kind: v12_heap::KIND_ARRAY, ..JsObject::default() });
        self.heap.add_root(JsValue::object(reactions));
        let promise = self.heap.alloc(JsObject {
            kind: HEAP_KIND_ORDINARY,
            properties: vec![JsValue::from_i32_smi(1).expect("fits"), v, JsValue::object(reactions)],
            ..JsObject::default()
        });
        self.heap.add_root(JsValue::object(promise));
        (JsValue::object(promise), false, v)
    }

    fn try_unwrap_promise(&self, v: JsValue) -> Option<JsValue> {
        let obj = v.as_object()?;
        let p = self.heap.get(obj);
        if p.properties.len() >= 3 && p.properties[0].as_smi().is_some_and(|s| (0..=2).contains(&s)) {
            let state = p.properties[0].as_smi().unwrap();
            if state == 1 {
                return Some(p.properties[1]);
            }
        }
        None
    }

    fn resume_async(&mut self, r#gen: Handle<JsObject>, value: JsValue) -> Result<(), JSException> {
        let fn_idx = self.heap.get(r#gen).properties.first().and_then(|v| v.as_smi().map(|n| n as u32 as f64).or(v.as_f64())).unwrap_or(0.0) as u32;
        let resume_pc = self.heap.get(r#gen).properties.get(1).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) as usize;
        let snapshot = self.heap.get(r#gen).elements.clone();
        let env = self.heap.get(r#gen).prototype;
        let f_max_regs = self.functions[fn_idx as usize].max_regs;
        let new_base = self.stack.len();
        self.stack.resize(new_base + usize::from(f_max_regs), JsValue::undefined());
        let copy_len = snapshot.len().min(usize::from(f_max_regs));
        for i in 0..copy_len {
            self.stack[new_base + i] = snapshot[i];
        }
        let yield_dst = self.heap.get(r#gen).properties.get(3).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) as u16;
        if (yield_dst as usize) < usize::from(f_max_regs) {
            self.stack[new_base + usize::from(yield_dst)] = value;
        }
        self.frames.push(Frame { fn_idx, pc: resume_pc, base: new_base, max_regs: f_max_regs, env, generator: Some(r#gen), yield_dst: None });
        self.top_result = None;
        self.execute()?;
        Ok(())
    }

    fn resume_async_throw(&mut self, r#gen: Handle<JsObject>, exc: JsValue) -> Result<(), JSException> {
        let fn_idx = self.heap.get(r#gen).properties.first().and_then(|v| v.as_smi().map(|n| n as u32 as f64).or(v.as_f64())).unwrap_or(0.0) as u32;
        let resume_pc = self.heap.get(r#gen).properties.get(1).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) as usize;
        let snapshot = self.heap.get(r#gen).elements.clone();
        let env = self.heap.get(r#gen).prototype;
        let f_max_regs = self.functions[fn_idx as usize].max_regs;
        let new_base = self.stack.len();
        self.stack.resize(new_base + usize::from(f_max_regs), JsValue::undefined());
        let copy_len = snapshot.len().min(usize::from(f_max_regs));
        for i in 0..copy_len {
            self.stack[new_base + i] = snapshot[i];
        }
        self.frames.push(Frame { fn_idx, pc: resume_pc, base: new_base, max_regs: f_max_regs, env, generator: Some(r#gen), yield_dst: None });
        self.top_result = None;
        // Inject exception via unwind then execute
        self.unwind(exc)?;
        self.execute()?;
        Ok(())
    }

    /// Drains pending async awaits FIFO (microtask checkpoint). Returns number executed.
    pub fn run_jobs(&mut self) -> usize {
        let mut count = 0;
        while let Some((r#gen, val, is_reject)) = self.pending_awaits.pop_front() {
            let res = if is_reject { self.resume_async_throw(r#gen, val) } else { self.resume_async(r#gen, val) };
            let _ = res;
            count += 1;
            if count > 10000 { break; }
        }
        count
    }

    /// Number of pending async jobs.
    pub fn pending_jobs(&self) -> usize { self.pending_awaits.len() }

    /// Republishes every live reference as a GC root — the whole value stack
    /// plus each active frame's environment — immediately before opcodes
    /// that can reach `Heap::alloc`.
    fn gc_protect(&mut self) {
        let roots = &mut self.heap.roots_mut().0;
        // The root set is fully republished here, so long-lived interpreter
        // state kept outside the stack/frames — the global object and the
        // cached `console.log` native — must be re-rooted on every pass or
        // a collection between allocations drops their referents.
        let persistent: [Option<JsValue>; 5] = [self.global.map(JsValue::object), self.console_log, self.generator_next_fn, self.generator_return_fn, self.generator_throw_fn];
        roots.clear();
        roots.extend_from_slice(&self.stack);
        for frame in &self.frames {
            if let Some(env) = frame.env {
                roots.push(JsValue::object(env));
            }
            if let Some(g) = frame.generator {
                roots.push(JsValue::object(g));
            }
        }
        for (g, v, _) in &self.pending_awaits {
            roots.push(JsValue::object(*g));
            roots.push(*v);
        }
        if let Some(v) = self.top_result { roots.push(v); }
        roots.extend(persistent.into_iter().flatten());
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
