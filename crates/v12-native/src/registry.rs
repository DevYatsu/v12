//! The native dispatch seam: the [`NativeRegistry`] trait and the default
//! empty registry.

use v12_bytecode::FunctionBytecode;
pub use v12_bytecode::{EvalBindingKind, EvalCallerFacts, EvalCompileFacts};
use v12_heap::{Heap, JsValue};

use crate::id::NativeId;
use crate::throw::Throw;

/// A compiled eval program. The registry only compiles; the interpreter owns
/// registration and frame execution.
#[derive(Debug, Clone)]
pub struct CompiledEval {
    pub main: u32,
    pub functions: std::rc::Rc<[FunctionBytecode]>,
    pub strings: std::rc::Rc<[String]>,
}

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

    /// Compiles eval without executing it. Direct callers use the returned
    /// program in the current interpreter; the existing `eval` method remains
    /// the global/indirect execution seam.
    fn compile_eval(
        &mut self,
        _heap: &mut Heap,
        _source: &str,
        _global: Option<v12_heap::Handle<v12_heap::JsObject>>,
        _facts: &EvalCompileFacts,
    ) -> Result<CompiledEval, Throw> {
        Err(Throw::Message("TypeError: eval is not supported".into()))
    }

    /// `Function(params…, body)`: compile `params`/`body` into a program and
    /// return a genuinely callable closure for it. `global` is the caller's
    /// realm global (so compile failures throw realm-linked error objects
    /// carrying `constructor`, which `assert.throws` requires).
    /// `programs` is the caller's
    /// cross-program registry — the compiled program is registered there and
    /// the returned closure stamped with its id, so the result executes (and
    /// its inner closures resolve) in the caller's interpreter. The default
    /// implementation refuses (no runtime compilation).
    fn function_construct(
        &mut self,
        _heap: &mut Heap,
        _args: &[JsValue],
        _global: Option<v12_heap::Handle<v12_heap::JsObject>>,
        _programs: std::rc::Rc<std::cell::RefCell<Vec<ProgramTable>>>,
    ) -> Result<JsValue, Throw> {
        Err(Throw::Message(
            "TypeError: Function constructor is not supported".into(),
        ))
    }
    /// `NewPromiseCapability(C)` for `new Promise(executor)`: the pending
    /// promise plus the `[[Resolve]]`/`[[Reject]]` pair the executor receives
    /// (spec 27.5.1.1 step 3, `NewPromiseCapability` 27.2.3.1).
    ///
    /// Returned as a three-element array `[promise, resolve, reject]` — the
    /// same carrier `combinator_step` uses for its pair — because a registry
    /// seam hands over one value. The executor itself is *not* called here:
    /// that needs the machine, so 27.5.1.1 step 5 runs in the interpreter
    /// (`Interp::run_promise_construct`), which is what makes it synchronous.
    ///
    /// The default implementation refuses: a registry that does not implement
    /// it has no promise constructor.
    fn promise_capability(&mut self, _heap: &mut Heap, _ctor: JsValue) -> Result<JsValue, Throw> {
        Err(Throw::Message(
            "TypeError: the Promise constructor is not supported".into(),
        ))
    }

    /// Settles a capability pair built by [`Self::promise_capability`] —
    /// 27.5.1.1 step 6 `IfAbruptRejectPromise`, the executor having thrown.
    /// First settlement wins, so calling this on a promise the executor
    /// already settled is a no-op.
    ///
    /// The default implementation refuses: a registry that does not implement
    /// it has no promise constructor.
    #[allow(clippy::too_many_arguments)]
    fn capability_settle(
        &mut self,
        _heap: &mut Heap,
        _promise: JsValue,
        _resolve: JsValue,
        _reject: JsValue,
        _value: JsValue,
        _rejecting: bool,
    ) -> Result<JsValue, Throw> {
        Err(Throw::Message(
            "TypeError: the Promise constructor is not supported".into(),
        ))
    }

    /// The `GetCapabilitiesExecutor` builtin of `NewPromiseCapability` (spec
    /// 27.5.1.5 steps 3-4): a `CreateBuiltinFunction(executorClosure, 2, "",
    /// «»)` that records `(resolve, reject)` into `state`'s two slots, throws a
    /// `TypeError` if either slot is already set, and otherwise returns
    /// `undefined`.
    ///
    /// `state` is the `PromiseCapability Record` the interpreter allocated in
    /// step 2; it is a plain two-slot object, and the collector cannot see the
    /// handle the host closure captures, so an implementation must root it (see
    /// `Heap::add_pinned_root`).
    ///
    /// The default implementation refuses: a registry with no promise
    /// machinery cannot build a capability executor.
    fn capability_executor(&mut self, _heap: &mut Heap, _state: JsValue) -> Result<JsValue, Throw> {
        Err(Throw::Message(
            "TypeError: promise capabilities are not supported".into(),
        ))
    }

    /// The recorded `(resolve, reject)` pair of a `PromiseCapability Record`
    /// (spec 27.5.1.5 step 7, the two `IsCallable` checks left to the caller).
    /// Both halves are `undefined` until the executor from
    /// [`Self::capability_executor`] has been called.
    ///
    /// The default implementation refuses, like [`Self::capability_executor`].
    fn capability_pair(
        &mut self,
        _heap: &mut Heap,
        _state: JsValue,
    ) -> Result<(JsValue, JsValue), Throw> {
        Err(Throw::Message(
            "TypeError: promise capabilities are not supported".into(),
        ))
    }

    /// `Promise.all` / `Promise.race` — the streaming seam (spec
    /// 27.2.4.1 / 27.2.4.2, `PerformPromiseAll` / `PerformPromiseRace`).
    ///
    /// The combinator runs from both sides at once: the interpreter pulls
    /// iterator steps and invokes `promiseResolve` and `then` (each needs the
    /// machine), while the engine owns the aggregate promise, the results
    /// array and the reaction watchers (jobs live there). So the three calls
    /// below carry a state object between them — see
    /// [`crate::promise_combinator`] for its slots — and the interpreter drives
    /// them one element at a time, which is what lets an abrupt element settle
    /// the combinator instead of stalling on an unbounded iterator.
    ///
    /// The aggregate promise is *not* allocated here: it is
    /// `promiseCapability.[[Promise]]`, the object `NewPromiseCapability(C)`
    /// had `C` construct, which is why `promise` and `reject` are parameters.
    ///
    /// The default implementation refuses: a registry that does not
    /// implement it has no promise combinators.
    fn combinator_begin(
        &mut self,
        _heap: &mut Heap,
        _kind: i32,
        _promise: JsValue,
        _reject: JsValue,
    ) -> Result<JsValue, Throw> {
        Err(Throw::Message(
            "TypeError: Promise combinators are not supported".into(),
        ))
    }

    /// One element of a combinator: counts it unsettled, then hands back either
    /// the `[onFulfilled, onRejected]` pair for a callable `then` or, when
    /// `then_v` is not callable, a one-element carrier naming the value the
    /// element settles with ([`crate::promise_combinator::carrier`]).
    ///
    /// `then_v` is the value the interpreter already read off the element (the
    /// read needs the machine: a getter or a proxy `get` trap can run user
    /// code), so the engine decides callability and no side effect is observed
    /// twice.
    ///
    /// `resolve_v` is `promiseCapability.[[Resolve]]`. For `KIND_RACE` the
    /// spec builds no per-element pair at all (27.5.4.5.1 step 8-e hands the
    /// capability's own functions to `then` verbatim), so the pair this
    /// returns is that pair. For `KIND_ALL` the pair is
    /// `[onFulfilledElement, promiseCapability.[[Reject]]]` (27.5.4.1 step
    /// 6-m-i), where `onFulfilledElement` is an interpreter-intercepted native
    /// carrying this state object.
    fn combinator_step(
        &mut self,
        _heap: &mut Heap,
        _state: JsValue,
        _index: u32,
        _value: JsValue,
        _then_v: JsValue,
        _resolve_v: JsValue,
    ) -> Result<JsValue, Throw> {
        Err(Throw::Message(
            "TypeError: Promise combinators are not supported".into(),
        ))
    }

    /// `PerformPromiseAll` / `PerformPromiseAllSettled` element bookkeeping:
    /// records the element's outcome and takes it off the unsettled count,
    /// atomically.
    ///
    /// Returns the value the aggregate's `[[Resolve]]` must be *called* with
    /// when this element settled it — the results array for the last
    /// outstanding `KIND_ALL` element, and the element's own value for
    /// `KIND_RACE` — or `None` when the aggregate stays pending. The call
    /// itself is the interpreter's, because `[[Resolve]]` is user code for a
    /// subclassed receiver.
    ///
    /// `fulfilled` selects which result entry the outcome belongs to. For
    /// `KIND_ALL` the pair's reject half is `promiseCapability.[[Reject]]`
    /// verbatim, so only the fulfilled half reaches this seam; the flag is
    /// carried for `allSettled`, whose pair records both outcomes here.
    fn combinator_element_settled(
        &mut self,
        _heap: &mut Heap,
        _state: JsValue,
        _index: u32,
        _fulfilled: bool,
        _value: JsValue,
    ) -> Result<Option<JsValue>, Throw> {
        Err(Throw::Message(
            "TypeError: Promise combinators are not supported".into(),
        ))
    }

    /// Exhaustion (`exhausted` is true) or an abrupt element/iterator
    /// completion: returns the value the aggregate's `[[Resolve]]` must be
    /// called with, if any.
    ///
    /// The abrupt path is *not* settled here — it is the interpreter's
    /// `Call(promiseCapability.[[Reject]], undefined, « reason »)`, which may
    /// be user code — and neither is `KIND_RACE` at exhaustion, whose spec
    /// loop returns the capability promise as it stands.
    fn combinator_settle(
        &mut self,
        _heap: &mut Heap,
        _state: JsValue,
        _exhausted: bool,
    ) -> Result<Option<JsValue>, Throw> {
        Err(Throw::Message(
            "TypeError: Promise combinators are not supported".into(),
        ))
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
