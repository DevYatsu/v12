//! The native dispatch seam: the [`NativeRegistry`] trait, the runtime-only
//! [`RuntimeRegistry`], and the shared [`Handler`] fn-pointer type.

use std::collections::HashMap;

use v12_heap::{Heap, JsValue};

use crate::id::NativeId;
use crate::throw::Throw;

/// A native implementation: a plain fn pointer.
///
/// "Typed" vs "raw" is erased at declaration — typed handlers are wrapped
/// (via [`NativeSig`]) into this same fn-pointer shape. `Copy`, so a runtime
/// registry clones by value.
pub type Handler = fn(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, Throw>;

/// The dispatch seam between the interpreter and a native provider.
///
/// A call whose function index lies beyond the program's functions denotes a
/// native: the interpreter hands the receiver, arguments, and heap to the
/// registry and takes back the result or the value to throw. The default
/// registry is empty — every native index throws `TypeError` — so programs
/// compiled without built-ins behave identically whether or not a registry
/// is wired in.
pub trait NativeRegistry {
    /// Executes native function `id`. `args` excludes the receiver.
    fn call_native(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        id: NativeId,
    ) -> Result<JsValue, Throw>;

    /// Direct `eval(source)`: compile and execute `source` against `heap`,
    /// returning the completion value. `global` is the realm's global object
    /// (so eval's `var`/assignments share the caller's global). `programs` is
    /// the caller's cross-program registry; the engine registers the eval
    /// program there so eval-created closures resolve from the caller. The
    /// default implementation refuses (no eval support).
    fn eval(
        &mut self,
        _heap: &mut Heap,
        _source: &str,
        _this: JsValue,
        _global: Option<v12_heap::Handle<v12_heap::JsObject>>,
        _programs: std::rc::Rc<std::cell::RefCell<Vec<ProgramTable>>>,
    ) -> Result<JsValue, Throw> {
        Err(Throw::Message("TypeError: eval is not supported".into()))
    }
}

/// A registered program: its function table plus the interned string table.
///
/// Re-exported here so the trait's `eval` signature can name it without a
/// `v12-interp` dependency (the interp defines the actual table type).
pub type ProgramTable = (
    std::rc::Rc<[v12_bytecode::FunctionBytecode]>,
    std::rc::Rc<[String]>,
);

/// The default [`NativeRegistry`]: no natives exist.
#[derive(Default)]
pub struct EmptyNativeRegistry;

impl NativeRegistry for EmptyNativeRegistry {
    fn call_native(
        &mut self,
        heap: &mut Heap,
        _this: JsValue,
        _args: &[JsValue],
        id: NativeId,
    ) -> Result<JsValue, Throw> {
        Err(Throw::type_error(
            heap,
            format!("native function {id:?} is not registered"),
        ))
    }
}

/// The runtime-only half of native dispatch.
///
/// Builtins do NOT appear here — they live in the compile-time
/// [`builtin_dispatch`](crate::builtin_dispatch) match. This map holds only
/// runtime insertions: embedder host functions and per-engine stateful
/// natives.
#[derive(Clone, Default)]
pub struct RuntimeRegistry {
    handlers: HashMap<NativeId, Handler>,
}

impl RuntimeRegistry {
    /// Registers a runtime function (host fn, stateful native).
    pub fn register(&mut self, id: NativeId, f: Handler) {
        self.handlers.insert(id, f);
    }

    /// Looks up a runtime handler by id, if present.
    pub fn get(&self, id: NativeId) -> Option<Handler> {
        self.handlers.get(&id).copied()
    }
}

/// Wraps a typed handler into the raw [`Handler`] shape *at the call site*.
///
/// Expands to a closure that decodes the argument slice through
/// [`NativeSig`](crate::NativeSig) and calls the typed handler. The closure
/// captures only the fn item (zero-sized) and is used directly as a
/// `native_table!` entry — the wrap is inlined into the match arm at compile
/// time, with no indirection beyond the call itself.
///
/// The tuple type is explicit (e.g. `(f64, f64)`): Rust cannot recover the
/// argument tuple from a bare fn path, so the declared signature is spelled
/// once here.
///
/// ```rust
/// use v12_native::{NativeSig, Throw};
/// # fn dummy(_h: &mut v12_native::Heap, _t: v12_native::JsValue, _a: (f64, f64)) -> Result<v12_native::JsValue, v12_native::Throw> {
/// #     Ok(v12_native::JsValue::undefined())
/// # }
/// // Used inside `native_table!`:
/// //   NativeId::X => v12_native::typed_wrapper!(dummy, (f64, f64)),
/// let _ = v12_native::typed_wrapper!(dummy, (f64, f64));
/// ```
#[macro_export]
macro_rules! typed_wrapper {
    ($fn_path:path, $tuple:ty) => {{
        |heap: &mut $crate::Heap,
         this: $crate::JsValue,
         args: &[$crate::JsValue]|
         -> Result<$crate::JsValue, $crate::Throw> {
            // `S` is the declared signature; `from_js` decodes positionally.
            fn decode<S: $crate::NativeSig>(
                heap: &mut $crate::Heap,
                this: $crate::JsValue,
                args: &[$crate::JsValue],
                f: fn(&mut $crate::Heap, $crate::JsValue, S) -> Result<$crate::JsValue, $crate::Throw>,
            ) -> Result<$crate::JsValue, $crate::Throw> {
                let decoded = S::from_js(heap, args)?;
                f(heap, this, decoded)
            }
            decode::<$tuple>(heap, this, args, $fn_path)
        }
    }};
}
