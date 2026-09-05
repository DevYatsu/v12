//! The native dispatch seam: the [`NativeRegistry`] trait and the default
//! empty registry.

use v12_heap::{Heap, JsValue};

use crate::id::NativeId;
use crate::throw::Throw;

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
