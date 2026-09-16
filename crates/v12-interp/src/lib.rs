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
//! their captured environment as the prototype link; [`Kind::Function`] marks
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

pub(crate) mod call;
mod call_setup;
mod execute;
pub mod feedback;
mod generator_async;
mod globals;
mod object_ops;
mod ops;
mod property;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use v12_bytecode::{FunctionBytecode, Opcode};
use v12_bytecode::{GLOBAL_INTRINSICS as GLOBAL_INTRINSIC_NAMES, GLOBAL_VAR_OFFSET};
use v12_heap::{Attrs, Handle, Heap, JsObject, JsValue, Kind, PropKey, ShapeHandle, V12Str};

#[cfg(test)]
use crate::feedback::Lattice;
use crate::feedback::{FeedbackVector, TYPE_NAME_COUNT, TierHooks};

// ---------------------------------------------------------------------------
// Native indices — one shared enum.
//
// The interpreter used to duplicate ~30 `NATIVE_*` u32 constants from the
// engine (three fragile index spaces). All of them now live in the single
// `v12_native::NativeId` enum; these re-exports keep existing `NATIVE_*`
// spelling working and are typed as `NativeId`.
// ---------------------------------------------------------------------------

pub use v12_native::NativeId; // the shared enum itself

/// Offset of user-declared global slots in the global object's `properties`.
/// Const-stable string equality. `str == str` goes through `PartialEq`,
/// which is not yet a const trait on stable rustc; byte-wise comparison is
/// (integer compares are const-stable). Used only at compile time.
const fn const_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// First index of `name` in [`GLOBAL_INTRINSIC_NAMES`], computed at compile
/// time. Identical first-match semantics to `iter().position(|&n| n == name)`,
/// so callers pay zero runtime cost for a slot that is fixed at compile time.
const fn intrinsic_idx(name: &'static str) -> Option<usize> {
    let mut i = 0;
    while i < GLOBAL_INTRINSIC_NAMES.len() {
        if const_str_eq(GLOBAL_INTRINSIC_NAMES[i], name) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Fixed property-slot positions (in the global object's `properties` prefix)
/// of the intrinsics the hot lookup paths read by name. Each element index in
/// [`GLOBAL_INTRINSIC_NAMES`] doubles as that slot, so these are constants.
const CONSOLE_IDX: Option<usize> = intrinsic_idx("console");
const SYMBOL_IDX: Option<usize> = intrinsic_idx("Symbol");
const PROMISE_IDX: Option<usize> = intrinsic_idx("Promise");
const ARRAY_IDX: Option<usize> = intrinsic_idx("Array");
const OBJECT_IDX: Option<usize> = intrinsic_idx("Object");
const REGEXP_IDX: Option<usize> = intrinsic_idx("RegExp");

/// Maps a runtime `text` (already borrowed from the string table at the call
/// site) to the fixed property-slot index of the matching global intrinsic,
/// or `None` if `text` is not an intrinsic name. O(1) jump table mirroring
/// [`GLOBAL_INTRINSIC_NAMES`] — keep its arms in the same order as the array.
#[inline]
fn intrinsic_slot(text: &str) -> Option<usize> {
    match text {
        "Object" => Some(OBJECT_IDX.expect("intrinsic 'Object' present")),
        "Array" => Some(ARRAY_IDX.expect("intrinsic 'Array' present")),
        "String" => Some(intrinsic_idx("String").expect("intrinsic 'String' present")),
        "Number" => Some(intrinsic_idx("Number").expect("intrinsic 'Number' present")),
        "Boolean" => Some(intrinsic_idx("Boolean").expect("intrinsic 'Boolean' present")),
        "Math" => Some(intrinsic_idx("Math").expect("intrinsic 'Math' present")),
        "JSON" => Some(intrinsic_idx("JSON").expect("intrinsic 'JSON' present")),
        "Error" => Some(intrinsic_idx("Error").expect("intrinsic 'Error' present")),
        "TypeError" => Some(intrinsic_idx("TypeError").expect("intrinsic 'TypeError' present")),
        "RangeError" => Some(intrinsic_idx("RangeError").expect("intrinsic 'RangeError' present")),
        "ReferenceError" => {
            Some(intrinsic_idx("ReferenceError").expect("intrinsic 'ReferenceError' present"))
        }
        "SyntaxError" => {
            Some(intrinsic_idx("SyntaxError").expect("intrinsic 'SyntaxError' present"))
        }
        "Promise" => Some(PROMISE_IDX.expect("intrinsic 'Promise' present")),
        "Symbol" => Some(SYMBOL_IDX.expect("intrinsic 'Symbol' present")),
        "Map" => Some(intrinsic_idx("Map").expect("intrinsic 'Map' present")),
        "Set" => Some(intrinsic_idx("Set").expect("intrinsic 'Set' present")),
        "RegExp" => Some(REGEXP_IDX.expect("intrinsic 'RegExp' present")),
        "eval" => Some(intrinsic_idx("eval").expect("intrinsic 'eval' present")),
        "console" => Some(CONSOLE_IDX.expect("intrinsic 'console' present")),
        "globalThis" => Some(intrinsic_idx("globalThis").expect("intrinsic 'globalThis' present")),
        "Proxy" => Some(intrinsic_idx("Proxy").expect("intrinsic 'Proxy' present")),
        _ => None,
    }
}

/// The internal-slot property reads on a RegExp object, in the order the
/// realm materializes them in the object's `properties` vector (`source`,
/// `flags`, `lastIndex`). Match on this enum, never on the raw slot numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegExpSlot {
    Source,
    Flags,
    LastIndex,
}

/// Compile-time guard: every name in [`GLOBAL_INTRINSIC_NAMES`] must resolve
/// identically to its array position. `Iterator::all`, `match` on `&str`, and
/// `PartialEq` as a const trait (for `str`/`Option`) are not const-stable in
/// this toolchain, so the runtime `match` table in [`intrinsic_slot`] cannot
/// itself be const-evaluated. Instead a const `while` loop re-derives each
/// name's slot via [`intrinsic_idx`] and asserts it equals the array index,
/// comparing only primitive `usize` (const-safe); a hand-written arm in
/// [`intrinsic_slot`] that resolves a name to the wrong slot then fails to
/// compile, and an unresolvable name hits the `None` sentinel branch below.
const fn intrinsic_slot_guard() {
    let mut i = 0;
    while i < GLOBAL_INTRINSIC_NAMES.len() {
        // A `None` (unresolvable) name is mapped to `!i`, which always differs
        // from `i`, forcing the mismatch branch below.
        let hit = match intrinsic_idx(GLOBAL_INTRINSIC_NAMES[i]) {
            Some(idx) => idx,
            None => !i,
        };
        if hit != i {
            panic!("intrinsic_slot drifted from GLOBAL_INTRINSIC_NAMES");
        }
        i += 1;
    }
}
const _: () = intrinsic_slot_guard();

/// Every well-known property name the `get_property` surface chain probes,
/// in canonical slot order. [`Interp::wk_keys`] caches one interned
/// [`PropKey`] per entry; surface probes integer-compare against those
/// instead of `key_is` flatten+memcmp. Keep the `WK_*` indices in the same
/// order as this array.
const WELL_KNOWN_NAMES: &[&str] = &[
    "length",            // 0
    "prototype",         // 1
    "log",               // 2
    "iterator",          // 3
    "resolve",           // 4
    "reject",            // 5
    "then",              // 6
    "catch",             // 7
    "create",            // 8
    "getPrototypeOf",    // 9
    "defineProperty",    // 10
    "keys",              // 11
    "values",            // 12
    "entries",           // 13
    "enumerableOwnKeys", // 14
    "next",              // 15
    "return",            // 16
    "throw",             // 17
    "source",            // 18
    "flags",             // 19
    "lastIndex",         // 20
    "isArray",           // 21
    "call",              // 22
    "apply",             // 23
    "bind",              // 24
    "toString",          // 25
    "valueOf",           // 26
    "hasOwnProperty",    // 27
    "size",              // 28
    "get",               // 29
    "set",               // 30
    "has",               // 31
    "delete",            // 32
    "clear",             // 33
    "forEach",           // 34
    "add",               // 35
    "constructor",       // 36
];
const WK_COUNT: usize = WELL_KNOWN_NAMES.len();
const WK_LENGTH: usize = 0;
const WK_PROTOTYPE: usize = 1;
const WK_LOG: usize = 2;
const WK_ITERATOR: usize = 3;
const WK_RESOLVE: usize = 4;
const WK_REJECT: usize = 5;
const WK_THEN: usize = 6;
const WK_CATCH: usize = 7;
const WK_CREATE: usize = 8;
const WK_GET_PROTOTYPE_OF: usize = 9;
const WK_DEFINE_PROPERTY: usize = 10;
const WK_KEYS: usize = 11;
const WK_VALUES: usize = 12;
const WK_ENTRIES: usize = 13;
const WK_ENUMERABLE_OWN_KEYS: usize = 14;
const WK_NEXT: usize = 15;
const WK_RETURN: usize = 16;
const WK_THROW: usize = 17;
const WK_SOURCE: usize = 18;
const WK_FLAGS: usize = 19;
const WK_LAST_INDEX: usize = 20;
const WK_IS_ARRAY: usize = 21;
const WK_CALL: usize = 22;
const WK_APPLY: usize = 23;
const WK_BIND: usize = 24;
const WK_TO_STRING: usize = 25;
const WK_VALUE_OF: usize = 26;
const WK_HAS_OWN_PROPERTY: usize = 27;
const WK_SIZE: usize = 28;
const WK_GET: usize = 29;
const WK_SET: usize = 30;
const WK_HAS: usize = 31;
const WK_DELETE: usize = 32;
const WK_CLEAR: usize = 33;
const WK_FOR_EACH: usize = 34;
const WK_ADD: usize = 35;
const WK_CONSTRUCTOR: usize = 36;

/// Maximum simultaneous JavaScript activations.
///
/// Why a limit exists: the dispatch loop is iterative, so recursion costs
/// heap (frames plus register windows), not native stack — an unbounded
/// `function f() { return f(); }` would otherwise OOM the process instead of
/// failing the script. 10 000 frames sits orders of magnitude above any
/// legitimate Tier-1 program while capping worst-case memory at a few
/// megabytes of stack slots; mainstream engines converge on the same order.
const MAX_CALL_DEPTH: usize = 10_000;

/// Maximum simultaneous native `execute()` re-entries (generator resumes and
/// similar host→JS re-entry). Each level costs several KiB of *native* stack,
/// so this cap — not [`MAX_CALL_DEPTH`] — is what keeps a self-resuming
/// generator (`function* g() { g().next(); }`) from overflowing the thread
/// stack and aborting the process; ordinary recursion stays governed by
/// `MAX_CALL_DEPTH`, which this budget will not exhaust first.
const MAX_RESUME_DEPTH: usize = 1_000;

/// How often (in dispatch iterations) the cooperative deadline is sampled.
///
/// A tight bytecode loop never yields to the runtime, so the deadline is
/// checked every N iterations: a 5s budget is enforced within N further
/// iterations of elapsing it, which adds one `Instant::now` syscall per
/// ~8k instructions — negligible for normal tests but enough to guarantee
/// a runaway loop never blocks the harness indefinitely.
const DEADLINE_CHECK_INTERVAL: u64 = 1 << 13;

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

impl JSException {
    /// Resolves a [`v12_native::Throw`] into a `JSException`, interning any
    /// pending message against `heap`.
    pub fn from_throw(heap: &mut Heap, t: v12_native::Throw) -> Self {
        JSException(t.into_js(heap))
    }
}

/// Seam for host-provided native functions.
///
/// A call whose function index lies beyond [`Program::functions`] denotes a
/// native: the interpreter hands the receiver, arguments, and heap to the
/// registry and takes back the result or the value to throw.
///
/// The trait lives in `v12-native` (the shared dispatch seam); this is a
/// re-export so existing `use v12_interp::NativeRegistry` sites keep working.
pub use v12_native::{EmptyNativeRegistry, NativeRegistry};

/// Outcome of preparing a call.
enum CallOutcome {
    /// A bytecode frame was pushed; dispatch continues into it.
    Pushed,
    /// The call completed inline (native path); the value is the result.
    Value(JsValue),
}

/// What the await-resume driver does with one parked await (see
/// `Interp::await_resume_value`).
enum AwaitResume {
    /// The awaited promise is still pending — poll again on a later pass.
    Skip,
    /// The awaited promise fulfilled with another promise — adopt it.
    Adopt(JsValue),
    /// Resume the frame (value, reject?).
    Run(JsValue, bool),
}

/// One registered program: its function table and the string table that
/// `Const::Str32` ids in that program resolve through. Kept as a pair so a
/// cross-program closure resolves both its bytecode and its constants
/// against its own program.
type ProgramTable = (Rc<[FunctionBytecode]>, Rc<[String]>);

/// Display width: how many innermost environments each [`Frame`] caches
/// for O(1) `Get/SetEnvSlot` resolution. Depth 8 covers virtually all
/// real closures (functions nest far shallower); deeper walks fall back
/// to the parent-link chain. Eight words per frame — accepted, speed-first
/// (frames already hold register windows).
const ENV_DISPLAY_CAP: usize = 8;

/// One JavaScript activation: a function body, its register window on the
/// shared stack, its pc, and the head of its environment chain.
struct Frame {
    fn_idx: u32,
    /// The program whose function table `fn_idx` indexes. 0 is the default
    /// program; cross-program calls (eval closures) carry the eval program's
    /// id. The dispatch loop resolves instructions through this.
    program: u32,
    /// Absolute bytecode pc of the next instruction. On entry to a handler
    /// this is reset to the handler target; a completing call advances the
    /// caller's pc past its `Call` header.
    pc: usize,
    base: usize,
    max_regs: u16,
    /// Innermost environment object (`None` until `NewEnvironment` runs).
    /// Kept here rather than derived so closure capture has one source.
    env: Option<Handle<JsObject>>,
    /// Cached innermost environments: `env_display[i]` is the environment
    /// `i` hops out, mirroring the parent-link chain exactly. Built once
    /// at frame creation ([`Interp::env_display_for`]) and head-shifted by
    /// `NewEnvironment` only — never otherwise mutated — so it stays exact
    /// for the frame's whole life. Entries are chain members, hence rooted
    /// transitively through `env` at every safepoint; no extra rooting.
    env_display: [Option<Handle<JsObject>>; ENV_DISPLAY_CAP],
    /// Associated generator object when this frame is a generator activation.
    generator: Option<Handle<JsObject>>,
    /// Destination register of the SuspendYield that suspended this frame (for resume value delivery).
    /// Currently written but not read: the resume path reads the
    /// destination from the generator object's slot directly, leaving
    /// this field as a forward-compatibility hook. Suppress the
    /// `dead_code` warning so the build stays clean.
    #[allow(dead_code)]
    yield_dst: Option<u16>,
    /// The `new.target` value for this frame's function activation.
    /// `Some` for constructor calls, `None` for regular calls and arrow functions.
    new_target: Option<JsValue>,
    /// The materialized `arguments` object for this activation (`None` when
    /// the function never references it). Stored here — rather than in a
    /// register the compiler did not reserve — and surfaced through the
    /// `GetGlobal`/`SetGlobal` `arguments` binding.
    arguments: Option<JsValue>,
}

/// Head-shifts an environment display for `NewEnvironment`: slot 0 takes
/// the new head, every older entry moves one deeper, the 8th falls off.
/// O(1), allocation-free. Equivalent to re-walking — the new head's
/// parent IS the previous head by construction — while the parent-link
/// chain stays the source of truth.
fn env_display_push(
    display: &mut [Option<Handle<JsObject>>; ENV_DISPLAY_CAP],
    head: Handle<JsObject>,
) {
    let mut prev = Some(head);
    for slot in display.iter_mut() {
        core::mem::swap(slot, &mut prev);
    }
}

pub mod generator;

pub struct Interp<'a> {
    functions: Rc<[FunctionBytecode]>,
    main: u32,
    /// Compiler string table: `Const::Str32` ids resolve through this.
    strings: Rc<[String]>,
    /// This interpreter's program id: 0 for the built program, higher for
    /// eval-registered programs. Closure objects stamp it so a call can
    /// resolve `Bytecode(fn_idx)` against the right function table.
    program_id: u32,
    /// Programs registered for cross-program calls, indexed by program id.
    /// Index 0 is `self.functions`/`self.strings`; eval programs are appended
    /// by the engine. Each entry pairs the function table with its string
    /// table (both resolve through the *callee's* program, not the current
    /// interpreter's). Lets a closure created in one program (e.g. `eval`) be
    /// invoked from another. Owned by the interpreter; a nested eval
    /// interpreter shares it via `Rc`.
    programs: std::rc::Rc<std::cell::RefCell<std::vec::Vec<ProgramTable>>>,
    heap: &'a mut Heap,

    /// Interned heap string per `(program, Str32)` constant id, filled lazily.
    /// Program-scoped: the same `Str32` id means different text in different
    /// programs (eval), so the key carries the program id.
    const_strings: std::collections::HashMap<(u32, u32), Handle<V12Str>>,
    /// Interned `typeof` names, lazily filled in [`TYPE_NAMES`] order.
    typeof_names: [Option<Handle<V12Str>>; TYPE_NAME_COUNT],
    /// Cached canonical keys for every well-known name the `get_property`
    /// surface chain probes (see [`WELL_KNOWN_NAMES`]). Integer `==`
    /// against these replaces per-probe `key_is` flatten+memcmp; slots
    /// fill lazily via [`Interp::wk_key`]. Replaces the old one-off
    /// `length_key`/`prototype_key` option fields (those accessors now
    /// read their table slots).
    wk_keys: [Option<PropKey>; WK_COUNT],
    /// Cached `root --length--> child` shape shared by every array.
    length_shape: Option<ShapeHandle>,
    /// Shape indexes already pinned via `add_shape_root` (pinning is
    /// idempotent-averse: repeated pins would grow the root vector forever).
    pinned_shapes: HashSet<u32>,

    /// The one contiguous value stack; frames window slices of it.
    stack: Vec<JsValue>,
    frames: Vec<Frame>,
    /// When set, `execute` returns as soon as the frame count drops to this
    /// value (a re-entrant accessor call stops after its own frame, leaving
    /// the caller's frames in place). `None` normally: `execute` runs to the
    /// bottom frame.
    stop_at_frames: Option<usize>,

    /// Number of `execute()` re-entries currently on the native stack
    /// (generator resumes and other host→JS re-entry). Each re-entry costs
    /// several KiB of native stack — unlike ordinary JS calls, which are
    /// iterative — so `MAX_CALL_DEPTH` alone lets a self-resuming generator
    /// grow the native stack past any thread limit before the frame cap
    /// fires. Capped by [`MAX_RESUME_DEPTH`].
    resume_depth: usize,

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
    /// `get_property` for `console.log`. The object is a `Kind::Function`
    /// whose `elements[0]` is `NativeId::ConsoleLog`, so `prepare_call` routes
    /// it through the `NativeRegistry`.
    console_log: Option<JsValue>,
    /// Cached native function objects for the Promise surface and the array
    /// `push`/`join` methods, synthesized lazily like `console_log` (see the
    /// `get_property` fast paths).
    promise_resolve_fn: Option<JsValue>,
    promise_reject_fn: Option<JsValue>,
    promise_then_fn: Option<JsValue>,
    promise_catch_fn: Option<JsValue>,
    array_push_fn: Option<JsValue>,
    array_join_fn: Option<JsValue>,
    enumerable_own_keys_fn: Option<JsValue>,
    generator_next_fn: Option<JsValue>,
    generator_return_fn: Option<JsValue>,
    generator_throw_fn: Option<JsValue>,
    /// The realm's `Symbol.iterator` well-known symbol, allocated lazily on
    /// first `for-of`/spread use and rooted so it survives collection.
    symbol_iterator: Option<Handle<v12_heap::V12Symbol>>,
    /// Completion value of the bottom frame when the dispatch loop ends.
    ///
    /// `run` ignores it; `call_object` reads it to return the callee's result.
    top_result: Option<JsValue>,
    /// Pending async resumes as FIFO microtask queue: (generator, value, is_reject).
    pending_awaits: std::collections::VecDeque<(Handle<JsObject>, JsValue, bool)>,
    /// Async-function completions awaiting promise settlement:
    /// (completion promise, value, rejecting).
    ///
    /// The interpreter cannot schedule reaction jobs (the job machinery lives
    /// in the engine), so an async body's completion pushes here and the
    /// engine's checkpoint drain settles the promise through the full
    /// capability/reaction path (`promise::settle_async_completion`).
    pending_settlements: Vec<(Handle<JsObject>, JsValue, bool)>,
    /// Cooperative execution deadline for Test262 conformance runs. When set,
    /// the dispatch loop aborts with a catchable timeout error as soon as the
    /// budget elapses, so a runaway test can never block the harness. `None`
    /// (the default) leaves execution unbounded — used by the production
    /// engine/embed path, which manages its own budgeting.
    deadline: Option<Instant>,
    /// Dispatch iterations since the last deadline sample; wraps so a never-
    /// terminating test doesn't trip the counter. Sampled every
    /// [`DEADLINE_CHECK_INTERVAL`] iterations.
    deadline_ticks: u64,
    /// Latched `true` the first time the cooperative deadline fires inside
    /// `execute`. `resume_next_await` / `JobQueue::drain` / `drain_checkpoint`
    /// poll this instead of inspecting the swallowed `execute` result, so an
    /// async drain terminates instead of spinning on pending jobs whose
    /// bytecode can never finish.
    deadline_exceeded: bool,
}

impl<'a> Interp<'a> {
    /// Builds an interpreter over `program`, resolving `Const::Str32` ids
    /// against `strings` (as produced by
    /// `v12_bccompiler::compile_source_with_strings`).
    ///
    /// ADR-003: the interpreter borrows `heap` for its whole lifetime — the
    /// caller (the engine) owns the heap and keeps it valid. `Interp` never
    /// allocates or reclaims a heap of its own.
    ///
    /// Top-level code addresses globals through `GetGlobal`/`SetGlobal`, so a
    /// global object must exist even when no embedder provides one: without an
    /// explicit [`Self::set_global`], a private default global is allocated
    /// and rooted here. It carries the `GLOBAL_VAR_OFFSET` leading intrinsic
    /// slots (all `undefined` outside a realm) so shared `GetGlobal` fast
    /// paths stay in bounds.
    pub fn new(
        heap: &'a mut Heap,
        functions: impl Into<Rc<[FunctionBytecode]>>,
        main: u32,
        strings: impl Into<Rc<[String]>>,
    ) -> Self {
        heap.roots_mut().0.reserve(INITIAL_STACK_CAPACITY);
        let functions = functions.into();
        let strings = strings.into();
        let programs = std::rc::Rc::new(std::cell::RefCell::new(vec![(
            Rc::clone(&functions),
            Rc::clone(&strings),
        )]));
        let mut interp = Self {
            functions,
            main,
            strings,
            program_id: 0,
            programs,
            heap,
            const_strings: std::collections::HashMap::new(),
            typeof_names: [const { None }; TYPE_NAME_COUNT],
            wk_keys: [None; WK_COUNT],
            length_shape: None,
            pinned_shapes: HashSet::new(),
            stack: Vec::with_capacity(INITIAL_STACK_CAPACITY),
            frames: Vec::new(),
            stop_at_frames: None,
            resume_depth: 0,
            natives: Box::new(EmptyNativeRegistry),
            hooks: Box::new(()),
            feedback: std::collections::HashMap::new(),
            tier_up_pending: Vec::new(),
            global: None,
            console_log: None,
            promise_resolve_fn: None,
            promise_reject_fn: None,
            promise_then_fn: None,
            promise_catch_fn: None,
            array_push_fn: None,
            array_join_fn: None,
            enumerable_own_keys_fn: None,
            generator_next_fn: None,
            generator_return_fn: None,
            generator_throw_fn: None,
            symbol_iterator: None,
            top_result: None,
            pending_awaits: std::collections::VecDeque::new(),
            pending_settlements: Vec::new(),
            deadline: None,
            deadline_ticks: 0,
            deadline_exceeded: false,
        };
        interp.ensure_default_global();
        interp
    }

    /// Convenience constructor: compiles `source` and resolves its string
    /// table in one step.
    ///
    /// ADR-001: this is retained for the test suite and as a thin shim — the
    /// interpreter itself no longer depends on the front-end, so the shim
    /// is feature-gated on `compiler` (always on for tests via
    /// `[dev-dependencies] v12-bccompiler`). Production embedders should
    /// call [`v12_engine::Engine::eval`] (which builds an `Interp` from a
    /// compiled `Program`).
    #[cfg(feature = "compiler")]
    pub fn from_source(
        heap: &'a mut Heap,
        source: &str,
    ) -> Result<Interp<'a>, v12_bccompiler::CompileError> {
        let (program, strings) = v12_bccompiler::compile_source_with_strings(source)?;
        Ok(Self::new(heap, program.functions, program.main, strings))
    }

    /// Installs a native-function seam, replacing any previous registry.
    pub fn set_natives(&mut self, natives: Box<dyn NativeRegistry>) {
        self.natives = natives;
    }

    /// Sets a cooperative execution deadline. When set, the dispatch loop
    /// aborts with a timeout error once `Instant::now` exceeds `deadline`, so a
    /// runaway test (no `await`/IO to yield on) cannot block the calling host.
    /// Passing `None` restores the unbounded default. Also clears any prior
    /// `deadline_exceeded` latch set by a previous overrun.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
        self.deadline_exceeded = false;
    }

    /// Returns `true` once the cooperative deadline has fired during this
    /// interpreter's `execute` runs. The engine's async-drain loop polls this
    /// to short-circuit instead of spinning on pending jobs whose bytecode can
    /// never complete.
    #[must_use]
    pub fn is_deadline_exceeded(&self) -> bool {
        self.deadline_exceeded
    }

    /// Registers a program in the cross-program table, returning its id.
    /// The id is what a nested interpreter running that program stamps on
    /// its closure objects, so calls from other programs resolve here.
    pub fn register_program(
        &mut self,
        functions: impl Into<Rc<[FunctionBytecode]>>,
        strings: impl Into<Rc<[String]>>,
    ) -> u32 {
        let mut table = self.programs.borrow_mut();
        let id = table.len() as u32;
        table.push((functions.into(), strings.into()));
        id
    }

    /// Sets this interpreter's program id (the id returned by
    /// [`Self::register_program`] for the program it is executing).
    pub fn set_program_id(&mut self, id: u32) {
        self.program_id = id;
    }

    /// Replaces the shared cross-program table (a nested eval interpreter
    /// adopts the caller's registry so both resolve the same programs).
    pub fn set_programs(
        &mut self,
        programs: std::rc::Rc<std::cell::RefCell<std::vec::Vec<ProgramTable>>>,
    ) {
        self.programs = programs;
    }

    /// The shared cross-program table (so a nested eval interpreter can
    /// register into the same registry the outer interpreter resolves).
    pub fn programs(&self) -> std::rc::Rc<std::cell::RefCell<std::vec::Vec<ProgramTable>>> {
        Rc::clone(&self.programs)
    }

    /// The function table for a program id. The interpreter's own `functions`
    /// is authoritative for the built program (id 0); higher ids resolve
    /// through the shared registry (eval programs). Falls back to
    /// `self.functions` for unknown ids.
    fn functions_for_program(&self, id: u32) -> Rc<[FunctionBytecode]> {
        if id == 0 {
            return Rc::clone(&self.functions);
        }
        let table = self.programs.borrow();
        table
            .get(id as usize)
            .map(|(f, _)| Rc::clone(f))
            .unwrap_or_else(|| Rc::clone(&self.functions))
    }

    /// The string table for a program id (mirror of
    /// [`Self::functions_for_program`]).
    fn strings_for_program(&self, id: u32) -> Rc<[String]> {
        if id == 0 {
            return Rc::clone(&self.strings);
        }
        let table = self.programs.borrow();
        table
            .get(id as usize)
            .map(|(_, s)| Rc::clone(s))
            .unwrap_or_else(|| Rc::clone(&self.strings))
    }

    /// Allocates a closure function object, stamping this interpreter's
    /// program id so `Bytecode(fn_idx)` resolves against the right table.
    ///
    /// Non-arrow functions also get their spec-mandated `prototype` property
    /// (a fresh object whose `constructor` points back at the function) so
    /// `F.prototype` reads and `new F` work without lazy materialization.
    /// Arrow functions are not constructible and have no `prototype`.
    fn alloc_closure(&mut self, fn_idx: u32, env: Option<Handle<JsObject>>) -> Handle<JsObject> {
        let funcs = self.functions_for_program(self.program_id);
        let (is_arrow, expected_args, function_name) = funcs
            .get(fn_idx as usize)
            .map(|f| (f.is_arrow, f.expected_args, f.function_name.clone()))
            .unwrap_or((false, 0, None));
        let mut obj = JsObject::function(v12_heap::FunctionTarget::Bytecode(fn_idx), env);
        obj.program_id = self.program_id;
        let h = self.heap.alloc(obj);
        // Every closure carries its own `length` (ExpectedArgumentCount), so
        // `f.length` reads never depend on the property surfaces.
        self.install_function_length(h, expected_args);
        if let Some(name) = function_name {
            let frame = self.stack.len();
            self.stack.push(JsValue::object(h));
            self.gc_protect();
            let name_handle = self.heap.intern_text(&name);
            let key = JsValue::string(self.heap.intern_text("name"));
            let installed = self.define_own_data_attrs(
                JsValue::object(h),
                key,
                JsValue::string(name_handle),
                Attrs::new(false, false, true),
            );
            debug_assert!(installed.is_ok(), "name install cannot fail");
            self.stack.truncate(frame);
        }
        if !is_arrow {
            // Park the fresh closure on the stack: materialization allocates
            // (prototype object, shape transitions) and the collector can run
            // before the `Closure` arm stores `h` into its register.
            self.stack.push(JsValue::object(h));
            self.materialize_function_prototype(h)
                .expect("closure prototype materialization cannot fail");
            self.stack.pop();
        }
        h
    }

    /// Gives a function object its `prototype` property: a fresh ordinary
    /// object whose `constructor` property points back at `f`. Idempotent —
    /// returns early when the property already exists (e.g. the realm wired
    /// one, or `prepare_construct` materialized it earlier).
    fn materialize_function_prototype(&mut self, f: Handle<JsObject>) -> Result<(), JSException> {
        let key = self.prototype_key();
        let shape = self.shape_of(f);
        if let Some(desc) = self.heap.lookup_property(shape, key)
            && let Some(_slot) = desc.slot()
        {
            // Already present.
            return Ok(());
        }
        self.gc_protect();
        let proto = self.heap.alloc(JsObject::default());
        // gc_protect clears and repopulates the root vector, so `add_root`
        // values do not survive the allocations inside `set_property`.
        // Keep both objects alive by parking them on the value stack (which
        // gc_protect republishes as roots) for the duration.
        let proto_v = JsValue::object(proto);
        let f_v = JsValue::object(f);
        self.stack.push(proto_v);
        self.stack.push(f_v);
        // `constructor` on the prototype points back at the function.
        let ctor_key = JsValue::string(self.heap.intern_text("constructor"));
        let proto_key_v = JsValue::string(self.heap.intern_text("prototype"));
        let result = self
            .define_own_data_attrs(proto_v, ctor_key, f_v, Attrs::BUILTIN)
            .and_then(|()| {
                self.define_own_data_attrs(f_v, proto_key_v, proto_v, Attrs::FUNCTION_PROTOTYPE)
            });
        self.stack.pop();
        self.stack.pop();
        result
    }

    /// Installs the function's own `length` property from its
    /// `ExpectedArgumentCount` (stops at the rest parameter or the first
    /// parameter with an initializer). Stored as an own data property at
    /// creation so reads never reach the property surfaces.
    fn install_function_length(&mut self, h: Handle<JsObject>, expected: u16) {
        let len_v = JsValue::from_i32_smi(i32::from(expected)).expect("param count fits Smi");
        let frame = self.stack.len();
        self.stack.push(JsValue::object(h));
        self.gc_protect();
        let key = JsValue::string(self.heap.intern_text("length"));
        let installed = self.define_own_data_attrs(
            JsValue::object(h),
            key,
            len_v,
            Attrs::new(false, false, true),
        );
        debug_assert!(installed.is_ok(), "length install cannot fail");
        self.stack.truncate(frame);
    }

    /// Builds an unmapped arguments exotic object over `args` (holes read as
    /// `undefined`). Functions with a rest parameter — and every other
    /// function whose `arguments` never aliases parameters here — observe
    /// this unmapped form, so element writes never touch parameter registers.
    pub(crate) fn make_arguments_object(&mut self, args: &[JsValue]) -> JsValue {
        self.gc_protect();
        let elements: Vec<JsValue> = args
            .iter()
            .map(|&v| if v.is_hole() { JsValue::undefined() } else { v })
            .collect();
        let len = elements.len();
        let shape = self.array_shape();
        let h = self.heap.alloc(JsObject::arguments(
            smallvec::smallvec![
                JsValue::from_i32_smi(len as i32).expect("arguments length fits Smi"),
            ],
            elements,
            None,
        ));
        self.bind_shape(h, shape);
        JsValue::object(h)
    }

    /// Materializes the frame's `arguments` object when the callee's body
    /// references it. Returns `None` for functions that do not, keeping
    /// their activations allocation-free. The object is rooted for the
    /// frame's lifetime; pop paths drop the root via
    /// [`Self::drop_frame_arguments`].
    pub(crate) fn frame_arguments_for(
        &mut self,
        fn_idx: u32,
        program: u32,
        args: &[JsValue],
    ) -> Option<JsValue> {
        let needs = self
            .functions_for_program(program)
            .get(fn_idx as usize)
            .is_some_and(|f| f.needs_arguments);
        if !needs {
            return None;
        }
        let obj = self.make_arguments_object(args);
        self.heap.add_root(obj);
        Some(obj)
    }

    /// Drops the root pinning a popped frame's `arguments` object, if any.
    /// The object stays alive through ordinary reachability afterwards.
    pub(crate) fn drop_frame_arguments(&mut self, frame: &Frame) {
        if let Some(a) = frame.arguments {
            self.heap.remove_root(a);
        }
    }

    /// The visible `arguments` binding for the current activation: the
    /// nearest frame on the stack with a materialized object. `None` at the
    /// top level or inside functions that never reference it.
    pub(crate) fn frame_arguments_value(&self) -> Option<JsValue> {
        self.frames.iter().rev().find_map(|f| f.arguments)
    }

    /// Binds `val` as the current activation's `arguments` (the `arguments
    /// = v` write path). Overwrites the nearest materialized slot;
    /// otherwise parks it on the top frame so subsequent reads observe it.
    pub(crate) fn set_frame_arguments(&mut self, val: JsValue) {
        for fr in self.frames.iter_mut().rev() {
            if fr.arguments.is_some() {
                if let Some(old) = fr.arguments {
                    self.heap.remove_root(old);
                }
                self.heap.add_root(val);
                fr.arguments = Some(val);
                return;
            }
        }
        if let Some(fr) = self.frames.last_mut() {
            self.heap.add_root(val);
            fr.arguments = Some(val);
        }
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
    ///
    /// ADR-003: takes `&mut Heap` (borrowed, not owned) — the caller keeps
    /// ownership for the interpreter's whole lifetime.
    pub fn new_with_heap(
        heap: &'a mut Heap,
        global: Option<Handle<JsObject>>,
        functions: impl Into<Rc<[FunctionBytecode]>>,
        main: u32,
        strings: impl Into<Rc<[String]>>,
    ) -> Self {
        let mut interp = Self::new(heap, functions, main, strings);
        interp.global = global;
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
        let g = self
            .heap
            .alloc(JsObject::environment(GLOBAL_VAR_OFFSET, None));
        self.heap.add_root(JsValue::object(g));
        // Minimal Promise wiring for standalone interp tests (mirrors realm.rs)
        let promise_proto = self.heap.alloc(JsObject::default());
        self.heap.add_root(JsValue::object(promise_proto));
        let promise_ctor = self.heap.alloc(JsObject {
            kind: Kind::Function,
            prototype: Some(promise_proto),
            ..JsObject::default()
        });
        self.heap.add_root(JsValue::object(promise_ctor));
        {
            let props = &mut self.heap.get_mut(g).properties;
            if props.len() > 10 {
                props[10] = JsValue::object(promise_ctor);
            }
        }
        self.global = Some(g);
    }

    /// Mutable heap access for embedders that share the heap.
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut *self.heap
    }

    /// Read-only view of the underlying heap.
    pub fn heap(&self) -> &Heap {
        self.heap
    }

    #[cfg(test)]
    pub(crate) fn heap_mut_for_test(&mut self) -> &mut Heap {
        self.heap
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
        Rc::make_mut(&mut self.functions)
    }

    /// Runs the top-level script to completion.
    ///
    /// `Ok(())` on normal completion; [`Err(JSException)`] when a thrown
    /// value escaped every handler. The completion value (last evaluated
    /// expression statement, or `undefined` for empty scripts) is exposed
    /// via [`Self::completion_value`] — ADR-004 surface so embedders can
    /// surface `eval("1+1")` → `2` instead of hard-coding `undefined`.
    pub fn run(&mut self) -> Result<(), JSException> {
        // Resolve the main function against this interpreter's program (which
        // for a nested eval interpreter lives in the shared registry, not the
        // local `functions` field).
        let main_funcs = self.functions_for_program(self.program_id);
        let main_regs =
            main_funcs[usize::try_from(self.main).expect("function index fits usize")].max_regs;
        debug_assert!(self.frames.is_empty(), "run() is not reentrant");
        self.stack.clear();
        self.stack
            .resize(usize::from(main_regs), JsValue::undefined());
        // Sloppy-mode top-level `this` is the global object (ES
        // GetThisBinding for global code). Without this, `this.x = ...`
        // at the top level reads `undefined` from r0 and the
        // `SetProperty` null/undefined guard throws a TypeError.
        self.ensure_default_global();
        if let Some(g) = self.global
            && !self.stack.is_empty()
        {
            self.stack[0] = JsValue::object(g);
        }
        self.frames.push(Frame {
            fn_idx: self.main,
            program: self.program_id,
            pc: 0,
            base: 0,
            max_regs: main_regs,
            env: None,
            env_display: [None; ENV_DISPLAY_CAP],
            generator: None,
            yield_dst: None,
            new_target: None,
            arguments: None,
        });
        self.note_entry(self.main);
        self.execute()
    }

    /// The script's actual completion value (ADR-004).
    ///
    /// `None` until the interpreter has run a top-level script; `Some(v)` is
    /// the value the script's main function returned, or `undefined` if it
    /// never reached an `ExpressionStatement` whose result was captured.
    /// Cleared by every new `run()` / `run_jobs()` so embedders cannot
    /// observe stale data.
    #[must_use]
    pub fn completion_value(&self) -> Option<JsValue> {
        self.top_result
    }

    /// Calls a function by bytecode/native index from outside the machine.
    ///
    /// Host-driven activation seam (Promise reaction jobs, embedder calls):
    /// synthesizes the callee object `prepare_call` expects — a
    /// `Kind::Function` whose `elements[0]` selects the target, bytecode index
    /// below `functions.len()` or native index above — then delegates to
    /// [`Self::call_object`]. Must not be called while `run()` is active.
    pub fn call_function(
        &mut self,
        fn_idx: u32,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        self.gc_protect();
        let callee = self.heap.alloc(JsObject::function(
            v12_heap::FunctionTarget::Bytecode(fn_idx),
            None,
        ));
        self.call_object(callee, this, args)
    }

    /// Calls an existing function object from outside the machine.
    ///
    /// Going through `prepare_call` (rather than pushing a frame by hand)
    /// preserves closure environment capture and native routing; the captured
    /// environment of a closure lives in the function object's `prototype`
    /// slot. Unlike `run()`/`call_object`'s old contract, this is safe to
    /// call with frames live on the stack (e.g. a microtask checkpoint drained
    /// mid-evaluation during top-level `await`): the callee runs in a nested
    /// `execute` bounded by `stop_at_frames`, so the caller's frames survive
    /// untouched. Native/host callees return inline and never touch frames.
    pub fn call_object(
        &mut self,
        callee: Handle<JsObject>,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        // Lay out `[callee][this][args…]` on top of the current window, mirroring
        // exactly what a parked `Call` instruction deposits. `prepare_call`
        // reads `callee`/`this`/args from `base + 0/1/2..`, so `base` is the
        // current stack length and `callee_reg` is 0.
        let base = self.stack.len();
        self.stack.push(JsValue::object(callee));
        self.stack.push(this);
        self.stack.extend_from_slice(args);
        let caller_max_regs =
            u16::try_from(self.stack.len() - base).expect("arguments fit a frame window");
        let argc = u16::try_from(args.len()).expect("argument count fits u16");
        let saved = self.stop_at_frames;
        // Bound the nested run at the current frame count so a throwing/nested
        // callee unwinds only its own frame and returns to us — it must not
        // drain the caller's frames (e.g. the module frame during TLA).
        // `prepare_call` pushes exactly one callee frame on the `Pushed` path;
        // `complete_frame`/`unwind` stop when the frame count falls back to
        // this value (matching the accessor-call contract in `call_accessor_with`).
        self.stop_at_frames = Some(self.frames.len());
        let boundary = self.stop_at_frames;
        self.top_result = None;
        let outcome = self.prepare_call(base, caller_max_regs, 0, argc);
        let result = match outcome {
            Ok(CallOutcome::Pushed) => {
                let exec = self.execute();
                // Some escape paths (the cooperative-deadline force-return;
                // historically, bare `return Err` guards in the dispatch loop)
                // leave frames live above the boundary. Shed them here so the
                // stack truncation below cannot orphan a live register window —
                // the corrupt machine state behind the register-window OOB
                // panics.
                if let Some(b) = boundary
                    && self.frames.len() > b
                {
                    while self.frames.len() > b {
                        if let Some(f) = self.frames.pop() {
                            self.drop_frame_arguments(&f);
                        }
                    }
                    self.stack.truncate(base);
                }
                exec.and_then(|()| {
                    self.top_result.take().ok_or_else(|| {
                        JSException(
                            self.error_value("InternalError: call completed without a result"),
                        )
                    })
                })
            }
            Ok(CallOutcome::Value(v)) => Ok(v),
            Err(e) => Err(e),
        };
        self.stop_at_frames = saved;
        // Shed the `[callee][this][args…]` window we appended (prepare_call's
        // pushed frame already got popped by complete_frame/unwind; native and
        // CallOutcome::Value paths leave the window untouched).
        self.stack.truncate(base);
        result
    }

    /// Registers a compiled program in the cross-program table and runs its
    /// main function on this interpreter, returning its completion value.
    ///
    /// This is the module-evaluation seam: the engine loads a dependency
    /// module's bytecode while the importing program's interpreter is live and
    /// executes the module body through the normal call machinery. The
    /// registered program id stamps the synthetic main closure, so functions
    /// exported by the module (closures created inside it) later called from
    /// any program sharing this interpreter's program table resolve against
    /// the module's own function/string tables.
    pub fn call_program_main(
        &mut self,
        functions: impl Into<Rc<[FunctionBytecode]>>,
        strings: impl Into<Rc<[String]>>,
        main: u32,
    ) -> Result<JsValue, JSException> {
        let id = self.register_program(functions, strings);
        let callee = self.heap.alloc(JsObject::function(
            v12_heap::FunctionTarget::Bytecode(main),
            None,
        ));
        self.heap.get_mut(callee).program_id = id;
        self.call_object(callee, JsValue::undefined(), &[])
    }

    /// Adopts a shared cross-program table and registers this interpreter's
    /// own program into it, returning the assigned program id.
    ///
    /// Engine-created interpreters share one table for the engine's lifetime:
    /// programs registered while one interpreter is live (eval'd sources,
    /// imported modules) must still resolve after that interpreter is dropped
    /// and a fresh one is rebuilt for `run_jobs`/`call_function`. Without the
    /// shared table, `functions_for_program` silently falls back to the
    /// rebuilt interpreter's own functions — foreign bytecode executed in a
    /// wrong-sized register window (the register-window OOB panic class).
    ///
    /// Ids start at 1: the host table reserves a dummy slot at index 0
    /// because id 0 means "the interpreter's own built-in table" in
    /// standalone (non-adopted) use.
    pub fn adopt_shared_programs(
        &mut self,
        programs: std::rc::Rc<std::cell::RefCell<std::vec::Vec<ProgramTable>>>,
    ) -> u32 {
        let id = {
            let mut table = programs.borrow_mut();
            let id = table.len() as u32;
            table.push((Rc::clone(&self.functions), Rc::clone(&self.strings)));
            id
        };
        self.programs = programs;
        self.program_id = id;
        id
    }

    fn private_get(
        &mut self,
        obj_v: crate::JsValue,
        class_id: u32,
        name_id: u32,
    ) -> Result<crate::JsValue, JSException> {
        let Some(h) = obj_v.as_object() else {
            return Err(JSException(self.error_value("TypeError: Cannot read private member from an object whose class did not declare it")));
        };
        let o = self.heap.get(h);
        if o.private_brand != Some(class_id) {
            return Err(JSException(self.error_value("TypeError: Cannot read private member from an object whose class did not declare it")));
        }
        if let Some(m) = &o.private_fields
            && let Some(v) = m.get(&name_id)
        {
            return Ok(*v);
        }
        Ok(crate::JsValue::undefined())
    }
    fn private_has(&self, obj_v: crate::JsValue, class_id: u32, name_id: u32) -> bool {
        if let Some(h) = obj_v.as_object() {
            let o = self.heap.get(h);
            if o.private_brand != Some(class_id) {
                return false;
            }
            if let Some(m) = &o.private_fields {
                return m.contains_key(&name_id);
            }
        }
        false
    }
    fn private_define(
        &mut self,
        obj_v: crate::JsValue,
        class_id: u32,
        name_id: u32,
        val: crate::JsValue,
    ) -> Result<(), JSException> {
        let Some(h) = obj_v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: Cannot define private field on non-object",
            )));
        };
        let o = self.heap.get_mut(h);
        if o.private_brand.is_none() {
            o.private_brand = Some(class_id);
        }
        if o.private_brand != Some(class_id) {
            return Err(JSException(self.error_value("TypeError: Cannot read private member from an object whose class did not declare it")));
        }
        let m = o
            .private_fields
            .get_or_insert_with(|| Box::new(rustc_hash::FxHashMap::default()));
        m.insert(name_id, val);
        Ok(())
    }
    fn private_set(
        &mut self,
        obj_v: crate::JsValue,
        class_id: u32,
        name_id: u32,
        val: crate::JsValue,
    ) -> Result<(), JSException> {
        let Some(h) = obj_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: Cannot set private member on non-object"),
            ));
        };
        let o = self.heap.get_mut(h);
        if o.private_brand != Some(class_id) {
            return Err(JSException(self.error_value("TypeError: Cannot read private member from an object whose class did not declare it")));
        }
        let m = o
            .private_fields
            .get_or_insert_with(|| Box::new(rustc_hash::FxHashMap::default()));
        m.insert(name_id, val);
        Ok(())
    }

    /// Applies ES `ToString` from outside the machine — diagnostics and test
    /// harnesses, not executable semantics. Error objects render as
    /// `"Name: message"`.
    pub fn to_display_string(&mut self, v: JsValue) -> String {
        if v.is_object()
            && let Some(obj) = v.as_object()
            && self.heap.get(obj).kind == Kind::Error
        {
            // Snapshot the name/message handles first so the text decode
            // below (which needs `&mut self`) doesn't fight the borrow.
            let name_h = self
                .heap
                .get(obj)
                .properties
                .first()
                .and_then(|v| v.as_string());
            let msg_h = self
                .heap
                .get(obj)
                .properties
                .get(1)
                .and_then(|v| v.as_string());
            let name = name_h
                .map(|h| self.string_text(h))
                .unwrap_or_else(|| "Error".to_string());
            let msg = msg_h.map(|h| self.string_text(h)).unwrap_or_default();
            if msg.is_empty() {
                return name;
            }
            return format!("{name}: {msg}");
        }
        // Plain-object errors (e.g. Test262Error): render `message`/`name` instead of opaque fallthrough.
        if v.is_object()
            && let Some(obj) = v.as_object()
        {
            let shape = self.heap.shape_of_mut(obj);
            let lookup = |heap: &mut v12_heap::Heap,
                          shape: v12_heap::ShapeHandle,
                          key: &str|
             -> Option<v12_heap::Handle<v12_heap::V12Str>> {
                let h = heap.intern_string(v12_heap::V12Str::latin1(key.as_bytes().to_vec()));
                let pk = v12_heap::PropKey::from_string(h);
                let desc = heap.lookup_property(shape, pk)?;
                let slot = desc.slot()?;
                // Interp objects have no GLOBAL_VAR_OFFSET bias (only engine global does).
                let idx = slot as usize;
                heap.get(obj)
                    .properties
                    .get(idx)
                    .and_then(|val| val.as_string())
            };
            let shape2 = self.heap.shape_of_mut(obj);
            // Need two separate lookups without overlapping mutable borrows.
            let msg_h = lookup(self.heap, shape, "message");
            if let Some(mh) = msg_h {
                let name_h = lookup(self.heap, shape2, "name");
                let msg = self.string_text(mh);
                if let Some(nh) = name_h {
                    let name = self.string_text(nh);
                    if msg.is_empty() {
                        return name;
                    }
                    return format!("{name}: {msg}");
                }
                if !msg.is_empty() {
                    return msg;
                }
            }
            // Empty/missing message (e.g. `new Test262Error()` with no
            // message argument): fall back to `constructor.name` so the
            // runner can classify the throw (it requires "Test262Error").
            if let Some(co) = self
                .chain_prop(obj, "constructor")
                .and_then(|v| v.as_object())
                && let Some(nh) = self.chain_prop(co, "name").and_then(|v| v.as_string())
            {
                let name = self.string_text(nh);
                if !name.is_empty() {
                    return name;
                }
            }
        }
        match ops::to_js_string(self.heap, v) {
            Ok(h) => {
                let units = ops::string_units(self.heap, h);
                String::from_utf16_lossy(&units)
            }
            Err(_) => "<unprintable>".into(),
        }
    }

    // ------------------------------------------------------------------
    // Dispatch loop
    // ------------------------------------------------------------------

    fn env_read(&mut self, depth: u16, slot: u16) -> Result<JsValue, JSException> {
        let env = self.env_at_expect(depth)?;
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
        let env = self.env_at_expect(depth)?;
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

    /// Display for a fresh frame whose environment head is `env`: the
    /// head plus up to `ENV_DISPLAY_CAP - 1` ancestors, walked once here
    /// (bounded O(8) per call — calls are cold, slot accesses hot).
    fn env_display_for(
        &self,
        env: Option<Handle<JsObject>>,
    ) -> [Option<Handle<JsObject>>; ENV_DISPLAY_CAP] {
        let mut out = [None; ENV_DISPLAY_CAP];
        let mut cur = env;
        for slot in out.iter_mut() {
            let h = match cur {
                Some(h) => h,
                None => break,
            };
            *slot = Some(h);
            cur = self.heap.get(h).prototype;
        }
        out
    }

    /// Environment `depth` hops out from the current frame: display hit
    /// for `depth < ENV_DISPLAY_CAP` (one slot read, O(1)), parent-link
    /// walk past it. The display mirrors the chain exactly (built at
    /// frame creation, head-shifted only by `NewEnvironment`), so both
    /// arms agree; the walk is the source of truth the display shadows.
    fn env_at(&self, depth: u16) -> Option<Handle<JsObject>> {
        let d = usize::from(depth);
        if d < ENV_DISPLAY_CAP {
            self.frames.last()?.env_display[d]
        } else {
            self.walk_env(depth)
        }
    }

    fn env_at_expect(&mut self, depth: u16) -> Result<Handle<JsObject>, JSException> {
        self.env_at(depth).ok_or_else(|| {
            JSException(self.error_value(
                "InternalError: environment depth exceeds live chain or missing environment",
            ))
        })
    }

    /// Walks `depth` parent links from the current frame's environment.
    fn walk_env(&self, depth: u16) -> Option<Handle<JsObject>> {
        let mut cur = self.frames.last()?.env?;
        for _ in 0..depth {
            cur = self.heap.get(cur).prototype?;
        }
        Some(cur)
    }

    // ------------------------------------------------------------------
    // Property access
    // ------------------------------------------------------------------

    /// The shape describing `obj`: looked up via [`Heap::shape_of_mut`],
    /// defaulting to the pinned empty-object root. ADR-002: the table lives
    /// inside the heap, so this is a one-line delegation.
    fn shape_of(&mut self, obj: Handle<JsObject>) -> ShapeHandle {
        self.heap.shape_of_mut(obj)
    }

    /// Records `obj`'s shape. Pinning happens inside [`Heap::bind_shape`].
    pub(crate) fn bind_shape(&mut self, obj: Handle<JsObject>, shape: ShapeHandle) {
        self.heap.bind_shape(obj, shape);
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

    /// Canonical [`PropKey`] for `WELL_KNOWN_NAMES[wk]`, interned once and
    /// cached. One hash+lookup on first use per name; O(1) slot read after.
    fn wk_key(&mut self, wk: usize) -> PropKey {
        if let Some(k) = self.wk_keys[wk] {
            return k;
        }
        let h = self.heap.intern_text(WELL_KNOWN_NAMES[wk]);
        let k = PropKey::from_string(h);
        self.wk_keys[wk] = Some(k);
        k
    }

    /// Integer-compare probe against a well-known name: true when the
    /// entry-interned `key` equals `WELL_KNOWN_NAMES[wk]`. Replaces `key_is`
    /// (flatten + memcmp) wherever the caller already holds the entry key;
    /// `None` (non-string key) never matches — mirroring `key_is`, which
    /// returns false for non-strings.
    fn key_is_wk(&mut self, key: Option<PropKey>, wk: usize) -> bool {
        match key {
            Some(k) => k == self.wk_key(wk),
            None => false,
        }
    }

    fn length_key(&mut self) -> PropKey {
        self.wk_key(WK_LENGTH)
    }

    fn prototype_key(&mut self) -> PropKey {
        self.wk_key(WK_PROTOTYPE)
    }

    /// The realm's `%Array.prototype%`, resolved through the `Array`
    /// constructor's linked `prototype` field (installed by the realm via
    /// `install_ctor`). Array makers stamp it as the fresh instance's
    /// `[[Prototype]]` so `instance.constructor === Array` resolves through
    /// the prototype's `constructor` back-link. `None` before realm install
    /// or for embedder globals without the intrinsic prefix.
    pub(crate) fn array_prototype(&self) -> Option<Handle<JsObject>> {
        let global = self.global?;
        let idx = intrinsic_slot("Array")?;
        let ctor_v = *self.heap.get(global).properties.get(idx)?;
        if ctor_v.is_hole() {
            return None;
        }
        let ctor = ctor_v.as_object()?;
        self.heap.get(ctor).prototype
    }

    /// Stamps the realm's `%Array.prototype%` as `h`'s `[[Prototype]]`
    /// (no-op when unresolvable; see [`Self::array_prototype`]).
    pub(crate) fn link_array_proto(&mut self, h: Handle<JsObject>) {
        if let Some(p) = self.array_prototype() {
            self.heap.get_mut(h).prototype = Some(p);
        }
    }

    /// Reads a property `key` walking the prototype chain (own shape first,
    /// then each `[[Prototype]]`): the first hit. Shape lookup is own-shape
    /// only, so inherited links like `instance.constructor` (living on the
    /// class prototype) need the walk. (Interp objects carry no
    /// `GLOBAL_VAR_OFFSET` bias, so no index adjustment.)
    pub(crate) fn chain_prop(&mut self, obj: Handle<JsObject>, key: &str) -> Option<JsValue> {
        let h = self
            .heap
            .intern_string(v12_heap::V12Str::latin1(key.as_bytes().to_vec()));
        let pk = v12_heap::PropKey::from_string(h);
        let mut cur = Some(obj);
        while let Some(o) = cur {
            let shape = self.heap.shape_of_mut(o);
            if let Some(desc) = self.heap.lookup_property(shape, pk)
                && let Some(slot) = desc.slot()
            {
                let hit = self.heap.get(o).properties.get(slot as usize).copied();
                if let Some(v) = hit
                    && !v.is_hole()
                {
                    return Some(v);
                }
            }
            cur = self.heap.get(o).prototype;
        }
        None
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

    /// Synthesizes a Map/Set method function object whose callable routes
    /// through the engine's native registry (`id` is an out-of-range
    /// bytecode index the registry dispatches). Cached per id.
    fn map_set_method(&mut self, id: NativeId) -> JsValue {
        self.gc_protect();
        let func = self.heap.alloc(JsObject::function(
            v12_heap::FunctionTarget::Bytecode(u32::from(id)),
            None,
        ));
        let value = JsValue::object(func);
        self.heap.add_root(value);
        value
    }

    /// Canonical array index for a key value: unsigned small integers (Smi or
    /// integral double) or their decimal-string spellings.
    ///
    /// Zero-allocation: the caller flattens once per entry
    /// ([`Self::flatten_key`]), then the flat storage scans in place — no
    /// `String` materialization. The defensive `flatten` below is an O(1)
    /// no-op once the entry flattened, and keeps this correct for direct
    /// callers. Digit semantics match the old `str::parse` path
    /// (empty/too-long/non-digit/overflow → `None`, leading zeros fold).
    fn array_index_of(&mut self, key_v: JsValue) -> Option<u32> {
        if let Some(n) = key_v.as_smi() {
            return u32::try_from(n).ok();
        }
        if let Some(n) = integral_index(key_v) {
            return Some(n);
        }
        let h = key_v.as_string()?;
        self.heap.flatten(h);
        match &self.heap.get(h).storage {
            v12_heap::StrStorage::Latin1(bytes) => {
                if bytes.is_empty() || bytes.len() > 10 {
                    return None;
                }
                let mut acc: u32 = 0;
                for &b in bytes {
                    if !b.is_ascii_digit() {
                        return None;
                    }
                    acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
                }
                Some(acc)
            }
            v12_heap::StrStorage::Utf16(units) => {
                if units.is_empty() || units.len() > 10 {
                    return None;
                }
                let mut acc: u32 = 0;
                for &u in units {
                    if u > 127 || !(u as u8).is_ascii_digit() {
                        return None;
                    }
                    acc = acc
                        .checked_mul(10)?
                        .checked_add(u32::from(u as u8 - b'0'))?;
                }
                Some(acc)
            }
            _ => None,
        }
    }

    /// Decodes a heap string to Rust text (key handling and diagnostics).
    fn string_text(&mut self, h: Handle<V12Str>) -> String {
        let units = ops::string_units(self.heap, h);
        String::from_utf16_lossy(&units)
    }

    /// Resolves a key value to a named-property key. Numbers coerce through
    /// their canonical decimal spelling; everything else goes through
    /// ES `ToString`.
    ///
    /// Intern-once: exactly one `intern_string` per call — callers never
    /// pre-materialize (entry [`Self::flatten_key`] is an in-place,
    /// allocation-free flatten, not a copy). Skipping the intern for
    /// already-canonical handles in O(1) needs a heap canonical-bit (the
    /// `Heap::REALM_GLOBAL_FLAG` precedent); that heap change is outside
    /// this lane and stays a follow-up.
    fn property_key(&mut self, key_v: JsValue) -> Result<PropKey, JSException> {
        if let Some(h) = key_v.as_string() {
            // Canonicalize through the intern table: PropKey identity is
            // reference identity, and dynamically-built strings (concat,
            // computed keys) must alias the canonical instance for the
            // property to be found.
            let units = ops::string_units(self.heap, h);
            let canonical = self.heap.intern_string(v12_heap::V12Str::utf16(units));
            return Ok(PropKey::from_string(canonical));
        }
        if let Some(y) = key_v.as_symbol() {
            return Ok(PropKey::from_symbol(y));
        }
        let h = ops::to_js_string(self.heap, key_v)?;
        let units = ops::string_units(self.heap, h);
        let canonical = self.heap.intern_string(v12_heap::V12Str::utf16(units));
        Ok(PropKey::from_string(canonical))
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
    /// The function is a `Kind::Function` whose `elements[0]` is
    /// `NativeId::ConsoleLog`, so `prepare_call` routes it through the
    /// `NativeRegistry`. The object is cached in `self.console_log` and
    /// rooted, so repeated `get_property` for `console.log` returns the
    /// same handle.
    fn console_log_fn(&mut self) -> JsValue {
        if let Some(cached) = self.console_log {
            return cached;
        }
        self.gc_protect();
        let func = self.heap.alloc(JsObject::function(
            v12_heap::FunctionTarget::Bytecode(u32::from(NativeId::ConsoleLog)),
            None,
        ));
        let value = JsValue::object(func);
        self.heap.add_root(value);
        self.console_log = Some(value);
        value
    }

    /// Decodes a raw bytecode index into the shared [`NativeId`] enum.
    ///
    /// Out-of-range indices beyond the program's function table are the
    /// native seam; an index that names no native is an error the registry
    /// reports (mirrors the old "not registered" TypeError).
    fn native_id_for(&mut self, idx: u32) -> Result<NativeId, JSException> {
        NativeId::try_from(idx).map_err(|unknown| {
            // Sentinel 0xFFFFFFFF is the realm placeholder for unimplemented
            // intrinsics — surface as "not a function" so the bucket
            // "native function #4294967295 is not registered" clears.
            if unknown.0 == 0xFFFF_FFFF {
                return JSException(self.error_value("TypeError: not a function"));
            }
            JSException(self.error_value(&format!(
                "TypeError: native function #{} is not registered",
                unknown.0
            )))
        })
    }

    /// Flattens a property-key string in place (allocation-free; an O(1)
    /// no-op when already flat). Every `get/set_property`-family entry
    /// calls this once, so the surface probes below (`key_is`,
    /// `method_native`, `array_index_of`) never re-materialize: their
    /// defensive flattens become no-ops on the hot path while staying
    /// correct for direct callers that skip the entry.
    fn flatten_key(&mut self, key_v: JsValue) {
        if let Some(h) = key_v.as_string() {
            self.heap.flatten(h);
        }
    }

    /// The native for `key` on receiver kind `kind`, from the const method
    /// table. Returns `None` when the key is not a method of that kind.
    ///
    /// O(1): flattens the key once, then a single [`v12_native::lookup_method`]
    /// (a `Kind` jump table + a bounded switch over the method names).
    fn method_native(&mut self, kind: Kind, key_v: JsValue) -> Option<NativeId> {
        let handle = key_v.as_string()?;
        self.heap.flatten(handle);
        match &self.heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => {
                v12_native::lookup_method(kind, std::str::from_utf8(bytes).ok()?)
            }
            v12_heap::StrStorage::Utf16(units) => {
                // Method names are ASCII; a UTF-16 key with non-ASCII units
                // cannot be a declared method.
                if !units.iter().all(|&u| u < 128) {
                    return None;
                }
                let bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
                v12_native::lookup_method(kind, std::str::from_utf8(&bytes).ok()?)
            }
            _ => None,
        }
    }

    /// Compares a key value's string text against `text` (flattening first).
    /// The flatten is defensive — an O(1) no-op when the entry already ran
    /// [`Self::flatten_key`] — so every surface probe stays correct whether
    /// or not the caller pre-flattened.
    fn key_is(&mut self, key_v: JsValue, text: &str) -> bool {
        let Some(handle) = key_v.as_string() else {
            return false;
        };
        self.heap.flatten(handle);
        match &self.heap.get(handle).storage {
            v12_heap::StrStorage::Latin1(bytes) => bytes == text.as_bytes(),
            v12_heap::StrStorage::Utf16(units) => units.iter().copied().eq(text.encode_utf16()),
            _ => false,
        }
    }

    /// True when `key_v` is the realm's `Symbol.iterator` well-known symbol
    /// (identity-compared against the lazily-allocated handle). Symbols have
    /// no text, so this is the symbol analog of `key_is`.
    fn key_is_symbol_iterator(&mut self, key_v: JsValue) -> bool {
        let Some(key_sym) = key_v.as_symbol() else {
            return false;
        };
        let wk = self.symbol_iterator_key();
        key_sym == wk
    }

    /// Which lazily-synthesized native function object to produce.
    ///
    /// Grouped so the synthesis body is written once; `console_log_fn`
    /// predates it and keeps its own copy.
    fn cached_native(&mut self, which: NativeId) -> JsValue {
        let (index, cached) = match which {
            NativeId::PromiseResolve => {
                (u32::from(NativeId::PromiseResolve), self.promise_resolve_fn)
            }
            NativeId::PromiseReject => (u32::from(NativeId::PromiseReject), self.promise_reject_fn),
            NativeId::PromiseThen => (u32::from(NativeId::PromiseThen), self.promise_then_fn),
            NativeId::PromiseCatch => (u32::from(NativeId::PromiseCatch), self.promise_catch_fn),
            NativeId::ArrayPush => (u32::from(NativeId::ArrayPush), self.array_push_fn),
            NativeId::ArrayJoin => (u32::from(NativeId::ArrayJoin), self.array_join_fn),
            NativeId::ObjectEnumerableOwnKeys => (
                u32::from(NativeId::ObjectEnumerableOwnKeys),
                self.enumerable_own_keys_fn,
            ),
            NativeId::GeneratorNext => (u32::from(NativeId::GeneratorNext), self.generator_next_fn),
            NativeId::GeneratorReturn => (
                u32::from(NativeId::GeneratorReturn),
                self.generator_return_fn,
            ),
            NativeId::GeneratorThrow => {
                (u32::from(NativeId::GeneratorThrow), self.generator_throw_fn)
            }
            // console.log is synthesized via `console_log_fn`, not here.
            NativeId::ConsoleLog => (u32::from(NativeId::ConsoleLog), self.console_log),
            // Only the ids below are ever synthesized through `cached_native`
            // (see the call sites); the rest of the enum is out of contract.
            _ => unreachable!("cached_native called with an unsynthesized id"),
        };
        if let Some(cached) = cached {
            return cached;
        }
        self.gc_protect();
        let func = self.heap.alloc(JsObject::function(
            v12_heap::FunctionTarget::Bytecode(index),
            None,
        ));
        let value = JsValue::object(func);
        self.heap.add_root(value);
        match which {
            NativeId::PromiseResolve => self.promise_resolve_fn = Some(value),
            NativeId::PromiseReject => self.promise_reject_fn = Some(value),
            NativeId::PromiseThen => self.promise_then_fn = Some(value),
            NativeId::PromiseCatch => self.promise_catch_fn = Some(value),
            NativeId::ArrayPush => self.array_push_fn = Some(value),
            NativeId::ArrayJoin => self.array_join_fn = Some(value),
            NativeId::ObjectEnumerableOwnKeys => self.enumerable_own_keys_fn = Some(value),
            NativeId::GeneratorNext => self.generator_next_fn = Some(value),
            NativeId::GeneratorReturn => self.generator_return_fn = Some(value),
            NativeId::GeneratorThrow => self.generator_throw_fn = Some(value),
            NativeId::ConsoleLog => self.console_log = Some(value),
            _ => unreachable!("cached_native called with an unsynthesized id"),
        }
        value
    }

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

    /// Drains pending async awaits FIFO (microtask checkpoint). Returns number executed.
    pub fn run_jobs(&mut self) -> usize {
        let mut count = 0;
        loop {
            // One pass: try each queued await once. Entries whose promise is
            // still pending are re-queued; a pass with zero resumes means
            // every await is parked on an unsettled promise — quiescent.
            let attempts = self.pending_awaits.len();
            if attempts == 0 {
                break;
            }
            let mut progressed = 0;
            for _ in 0..attempts {
                if self.resume_next_await() {
                    progressed += 1;
                }
                if self.deadline_exceeded || count + progressed > 10000 {
                    break;
                }
            }
            count += progressed;
            if progressed == 0 || self.deadline_exceeded || count > 10000 {
                break;
            }
        }
        count
    }

    /// Decides how a parked await proceeds. `Skip` = the await parked on a
    /// promise that is still pending (re-queue and poll again later);
    /// `Adopt` = the awaited promise fulfilled with another promise, so the
    /// frame parks on that one instead (thenable adoption); `Run` resumes
    /// the frame.
    fn await_resume_value(&mut self, val: JsValue, is_reject: bool) -> AwaitResume {
        if is_reject {
            // A rejection reason passes through untouched — no adoption.
            return AwaitResume::Run(val, true);
        }
        if self.is_promise(val) {
            let obj = val.as_object().expect("checked above");
            let (state, payload) = {
                let o = self.heap.get(obj);
                (o.properties[0].as_smi().unwrap_or(0), o.properties[1])
            };
            match state {
                0 => AwaitResume::Skip,
                1 if self.is_promise(payload) => AwaitResume::Adopt(payload),
                1 => AwaitResume::Run(payload, false),
                _ => AwaitResume::Run(payload, true),
            }
        } else {
            AwaitResume::Run(val, false)
        }
    }

    /// Resumes exactly one pending await (the oldest), if any. Returns `true`
    /// when a resume ran. The engine's single microtask checkpoint calls this
    /// between draining host jobs, so generator/async resumes and host jobs
    /// interleave per microtask semantics.
    ///
    /// An await parked on a still-pending promise returns `false` and is
    /// re-queued at the back — the driver's stall detection treats a full
    /// cycle of no progress as quiescence, so a promise that never settles
    /// ends the drain instead of spinning.
    ///
    /// Short-circuits to `false` once the cooperative deadline has fired: a
    /// resumed generator/async body that hits the deadline will abort its
    /// `execute` with a timeout error (swallowed here as `let _ = res`), but
    /// the latch lets us skip the *remaining* awaits whose bodies can never
    /// finish within the budget.
    pub fn resume_next_await(&mut self) -> bool {
        if self.deadline_exceeded {
            return false;
        }
        let Some((r#gen, val, is_reject)) = self.pending_awaits.pop_front() else {
            return false;
        };
        match self.await_resume_value(val, is_reject) {
            AwaitResume::Skip => {
                // Promise still pending — re-queue at the back and poll later.
                self.pending_awaits.push_back((r#gen, val, is_reject));
                return false;
            }
            AwaitResume::Adopt(payload) => {
                // The awaited promise resolved to another promise — park the
                // frame on that one instead (thenable adoption for promises).
                self.pending_awaits.push_back((r#gen, payload, false));
                return false;
            }
            AwaitResume::Run(resume_val, resume_reject) => {
                let res = if resume_reject {
                    self.resume_async_throw(r#gen, resume_val)
                } else {
                    self.resume_async(r#gen, resume_val)
                };
                if let Err(JSException(e)) = res {
                    // A resumed async body that throws must *reject* its
                    // completion promise, not surface the throw to the drain
                    // driver (spec 27.5.3.6: the error rides the promise).
                    self.pending_settlements.push((r#gen, e, true));
                }
            }
        }
        // A deadline can fire *during* the resume above; latch so the drain
        // loop sees it before scheduling more awaits.
        if self.deadline_exceeded {
            return false;
        }
        true
    }

    /// True when any async/generator resume is pending.
    pub fn has_pending_awaits(&self) -> bool {
        !self.pending_awaits.is_empty()
    }

    /// True when async-function completions await promise settlement.
    pub fn has_pending_settlements(&self) -> bool {
        !self.pending_settlements.is_empty()
    }

    /// Takes all queued async-completion settlements: (promise, value,
    /// rejecting). The engine's checkpoint drain feeds each into the full
    /// capability/reaction settlement path.
    pub fn take_pending_settlements(&mut self) -> Vec<(Handle<JsObject>, JsValue, bool)> {
        std::mem::take(&mut self.pending_settlements)
    }

    /// Number of pending async jobs.
    pub fn pending_jobs(&self) -> usize {
        self.pending_awaits.len()
    }

    /// Republishes every live reference as a GC root — the whole value stack
    /// plus each active frame's environment — at a safepoint.
    ///
    /// Phase 2 safepoint model: collection never runs inside `Heap::alloc`;
    /// it runs only at explicit safepoints. This method is the interpreter's
    /// safepoint: it first republishes the roots (so the collection observes
    /// the current stack/frames/pending awaits), then runs `Heap::safepoint`,
    /// which collects if the growth policy or stress cadence says so.
    ///
    /// Finding #5: roots from `heap.add_root(promise/reactions/g)` are transient.
    /// Republishing `stack` + `frames` + `pending_awaits` (generator + payload)
    /// + `top_result` + persistent globals discards stale `add_root` entries.
    ///
    /// After `complete_frame` settles an async promise the generator leaves
    /// `pending_awaits`, so the next pass drops its promise/reactions roots and
    /// the promise remains reachable only via the generator's `properties[4]`
    /// until the generator itself becomes unreachable.
    pub(crate) fn gc_protect(&mut self) {
        let roots = &mut self.heap.roots_mut().0;
        // Long-lived interpreter state kept outside the stack must be re-rooted on
        // every safepoint. `roots_mut` borrows only `self.heap`; the cached native
        // fields below are disjoint fields, so they can be read here (direct field
        // reads, not a `&self` method) without conflicting.
        // Finding #5 / stale-handle root cause: a previous version listed only 5 of
        // the 11 cached natives here, so a collection between allocations could free
        // an unregistered one (e.g. `Promise.then`) and leave a stale handle.
        let persistent: [Option<JsValue>; 11] = [
            self.global.map(JsValue::object),
            self.console_log,
            self.promise_resolve_fn,
            self.promise_reject_fn,
            self.promise_then_fn,
            self.array_push_fn,
            self.array_join_fn,
            self.enumerable_own_keys_fn,
            self.generator_next_fn,
            self.generator_return_fn,
            self.generator_throw_fn,
        ];
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
        // Async-completion settlements: the promise must survive until the
        // engine's drain settles it (the value rides along).
        for (ph, v, _) in &self.pending_settlements {
            roots.push(JsValue::object(*ph));
            roots.push(*v);
        }
        if let Some(v) = self.top_result {
            roots.push(v);
        }
        if let Some(sym) = self.symbol_iterator {
            roots.push(JsValue::symbol(sym));
        }
        roots.extend(persistent.into_iter().flatten());
        // Collection runs here, at the safepoint, with the freshly
        // republished roots visible — never inside `Heap::alloc`.
        self.heap.safepoint();
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

/// The function object an accessor value denotes, or `None` for
/// `undefined`/non-functions (absent accessor).
fn accessor_target(heap: &Heap, v: JsValue) -> Option<Handle<JsObject>> {
    let obj = v.as_object()?;
    if heap.get(obj).kind != Kind::Function {
        return None;
    }
    Some(obj)
}

/// The slot index of the newest descriptor of `shape` (its `num_own - 1`).
fn child_slot(heap: &Heap, shape: ShapeHandle) -> usize {
    usize::try_from(heap.get(shape).num_own.saturating_sub(1)).unwrap_or(0)
}
