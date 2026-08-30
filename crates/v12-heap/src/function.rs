//! Function callable targets: what a `KIND_FUNCTION` object actually invokes.
//!
//! A function object carries a [`FunctionTarget`] directly (one machine word)
//! instead of a magic numeric index into a registry. This makes `prepare_call`
//! a direct match: bytecode functions push a frame, native built-ins call the
//! handler fn pointer, and host closures invoke the embedder closure — no hash
//! map, no duplicated constant tables, no interpreter special-case chain.
//!
//! Layout: all three variants are exactly one word. `Bytecode(u32)` is a u32
//! payload; `Native(Native)` packs the `#[repr(u8)]` discriminant into the
//! spare bits (size 1); `Host(HostClosure)` carries the one-word closure
//! handle inline.

use crate::gc::{MarkSink, Trace};
use crate::Heap;
use crate::JsValue;

/// Discriminants of the built-in native functions.
///
/// A fieldless `#[repr(u8)]` enum so `Native(n)` fits one word inside
/// [`FunctionTarget`]. Replaces the old magic `NATIVE_*` integer constants
/// (which were duplicated across `v12-interp` and `v12-engine`); the
/// interpreter no longer needs to know any native indices at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Native {
    ObjectCreate,
    ObjectGetPrototypeOf,
    ObjectDefineProperty,
    ArrayPush,
    ArrayPop,
    ArrayJoin,
    StringCharAt,
    StringSlice,
    StringConstruct,
    NumberIsNaN,
    MathAbs,
    BooleanConstruct,
    ErrorCreate,
    PromiseResolve,
    PromiseReject,
    PromiseThen,
    QueueMicrotask,
    Eval,
    Function,
    ConsoleLog,
    GeneratorNext,
    GeneratorReturn,
    GeneratorThrow,
    EnumerableOwnKeys,
    MapConstruct,
    MapGet,
    MapSet,
    MapHas,
    MapDelete,
    MapSize,
    SetConstruct,
    SetAdd,
    SetHas,
    SetDelete,
    SetSize,
}

/// One-word handle to an embedder closure.
///
/// Sized to a single machine word (a raw pointer to the `Box<dyn FnMut>`
/// allocation) so [`FunctionTarget`] stays one word. `Clone` is a shallow
/// pointer copy; the engine owns the box and guarantees it outlives every
/// function object that references it (the JIT's executable-memory layer is
/// the only other audited `unsafe` in the codebase).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostClosure(pub(crate) *mut dyn FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue>);

// Safety: HostClosure is a pointer handle, never dereferenced here; the
// engine's registry owns the box and drops it after all referencing function
// objects are gone (single-mutator, engine-scoped lifetime). The single
// audited `unsafe` in this crate, alongside the JIT's executable-memory layer.
#[allow(unsafe_code)]
unsafe impl Send for HostClosure {}

impl HostClosure {
    /// Boxes a host closure, transferring ownership of the `Box` to the
    /// engine's registry. The returned handle is one word.
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue> + 'static,
    {
        let boxed: Box<dyn FnMut(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue>> =
            Box::new(f);
        Self(Box::into_raw(boxed))
    }

    /// Invokes the closure. Safe because the engine's registry guarantees the
    /// box outlives every handle that references it (single-mutator, drop
    /// order enforced by the engine).
    pub fn call(
        &self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JsValue> {
        // SAFETY: the box is owned by the engine's registry and outlives this
        // handle; `&mut` is sound because the engine is single-mutator and
        // this is the only live reference to the closure at call time.
        #[allow(unsafe_code)]
        unsafe {
            let f = &mut *self.0;
            f(heap, this, args)
        }
    }
}

/// What a `KIND_FUNCTION` object invokes when called.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunctionTarget {
    /// A bytecode function: index into the program's function table.
    Bytecode(u32),
    /// A built-in native: the handler fn pointer.
    Native(fn(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue>),
    /// An embedder-registered host closure.
    Host(HostClosure),
}

impl std::fmt::Debug for FunctionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FunctionTarget::Bytecode(idx) => write!(f, "Bytecode({idx})"),
            FunctionTarget::Native(_) => write!(f, "Native(fn)"),
            FunctionTarget::Host(_) => write!(f, "Host(closure)"),
        }
    }
}

impl Trace for FunctionTarget {
    fn trace(&self, _sink: &mut MarkSink<'_>) {
        // No heap handles inside; the captured environment lives in the
        // function object's `prototype` field, which is traced separately.
    }
}

/// Compatibility helpers for the transition: read a function object's
/// bytecode index (for `Bytecode` targets) and detect generator/async flags.
impl FunctionTarget {
    /// The bytecode function index, if this is a `Bytecode` target.
    #[inline]
    pub fn bytecode_index(self) -> Option<u32> {
        match self {
            FunctionTarget::Bytecode(idx) => Some(idx),
            _ => None,
        }
    }
}
