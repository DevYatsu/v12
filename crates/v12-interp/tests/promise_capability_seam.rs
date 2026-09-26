//! `Interp::new_promise_capability` — the 27.5.1.5 `NewPromiseCapability` seam.
//!
//! `NewPromiseCapability(C)` is the spec's own constructor path, and it is the
//! one place where the resolving pair is *not* created up front: it makes a
//! `PromiseCapability Record`, hands `C` a `GetCapabilitiesExecutor` builtin,
//! and reads the pair back out after `Construct` returned. So the pair it ends
//! up holding is exactly the pair the constructor's own code was given — the
//! difference from 27.5.1.1, where the executor merely receives them.
//!
//! The work splits across the registry seam: the interpreter owns step 1
//! (`IsConstructor`), step 6 (`Construct`, which runs user code and is the only
//! reason this needs the machine at all) and steps 7-8 (`IsCallable` on both
//! halves of the record); the engine owns the executor object and the record
//! itself. This file pins the interpreter half. The engine half is pinned from
//! the other side in `v12-engine`'s `promise_capability.rs`, against the real
//! `NativeRegistry` impls — the registry here is a faithful second copy of the
//! two trait methods, because `v12-interp` cannot depend on `v12-engine` (that
//! dependency runs the other way).
//!
//! There is no production caller yet: `Promise.all`/`race` still reach a
//! hand-allocated aggregate through `combinator::begin` until the combinator
//! rewiring that is its own change lands. That is why the seam is driven from
//! here rather than through a `built-ins/Promise` filter.

use v12_heap::{FunctionTarget, GcPolicy, Handle, Heap, JsObject, JsValue};
use v12_interp::{Interp, JSException};
use v12_native::{NativeId, NativeRegistry as NativeRegistryTrait, Throw};

/// The two `NativeRegistry` capability methods, reimplemented faithfully.
///
/// Nothing else in the trait is implemented: every other method has a default
/// that refuses, which is what a bare `Interp` wants.
#[derive(Default)]
struct Seam;

impl NativeRegistryTrait for Seam {
    fn call_native(
        &mut self,
        _heap: &mut Heap,
        _this: JsValue,
        _args: &[JsValue],
        _id: NativeId,
    ) -> Result<JsValue, Throw> {
        Err(Throw::Message(
            "TypeError: no natives in this registry".into(),
        ))
    }

    fn capability_executor(&mut self, heap: &mut Heap, state: JsValue) -> Result<JsValue, Throw> {
        let Some(record) = state.as_object() else {
            return Err(Throw::Message(
                "TypeError: the capability record is not an object".into(),
            ));
        };
        let func = heap.alloc(JsObject::function(
            FunctionTarget::Host(v12_heap::HostClosure::new(move |heap, _this, args| {
                // 27.5.1.5 step 3: the guard first, then the two stores, then
                // `undefined` — so a second call throws *without* overwriting.
                let unset = heap.get(record).properties[0].is_undefined()
                    && heap.get(record).properties[1].is_undefined();
                if !unset {
                    let name = heap.intern_text("TypeError");
                    let message = heap.intern_text("capability executor called twice");
                    return Err(JsValue::object(heap.alloc(JsObject::error(name, message))));
                }
                heap.get_mut(record).properties[0] =
                    args.first().copied().unwrap_or_else(JsValue::undefined);
                heap.get_mut(record).properties[1] =
                    args.get(1).copied().unwrap_or_else(JsValue::undefined);
                Ok(JsValue::undefined())
            })),
            None,
        ));
        heap.add_pinned_root(func);
        Ok(JsValue::object(func))
    }

    fn capability_pair(
        &mut self,
        heap: &mut Heap,
        state: JsValue,
    ) -> Result<(JsValue, JsValue), Throw> {
        let record = state
            .as_object()
            .ok_or_else(|| Throw::Message("TypeError: not a record".into()))?;
        Ok((
            heap.get(record).properties[0],
            heap.get(record).properties[1],
        ))
    }
}

/// What a `NewPromiseCapability` run observed, read out while the interpreter
/// still holds the heap.
struct Observed {
    /// `(promise, resolve, reject)`, or the thrown value.
    outcome: Result<(JsValue, JsValue, JsValue), JsValue>,
    /// The three elements of whatever the constructor returned — for the test
    /// constructors below that array is `[executor, resolve, reject]`.
    returned: Option<Vec<JsValue>>,
}

/// Compiles `source` (whose last expression is the constructor under test),
/// then runs `NewPromiseCapability` on it.
fn observe(source: &str, stress: Option<u32>) -> Observed {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let (outcome, returned) = {
        let mut interp = Interp::from_source(&mut heap, source).expect("compile");
        if let Some(every) = stress {
            interp.heap_mut().gc_stress(Some(every));
        }
        interp.set_natives(Box::new(Seam));
        interp.run().expect("run");
        let ctor = interp.completion_value().expect("a completion value");
        let outcome = match interp.new_promise_capability(ctor) {
            Ok(triple) => Ok(triple),
            Err(JSException(thrown)) => Err(thrown),
        };
        // Read the constructor's return value through the interpreter: the
        // `&mut Heap` borrow it owns is what makes the direct route impossible
        // here.
        let returned = outcome.as_ref().ok().and_then(|(promise, _, _)| {
            let arr: Handle<JsObject> = promise.as_object()?;
            Some(interp.heap().get(arr).elements_snapshot())
        });
        (outcome, returned)
    };
    Observed { outcome, returned }
}

/// The error name of a thrown value, off the `[name, message]` layout
/// `JsObject::error` uses.
fn error_name(heap: &mut Heap, thrown: JsValue) -> String {
    let h = thrown.as_object().expect("a TypeError is an object");
    let name = heap.get(h).properties[0];
    let h = name.as_string().expect("the error name is a string");
    heap.flatten(h);
    match &heap.get(h).storage {
        v12_heap::StrStorage::Latin1(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
        _ => String::new(),
    }
}

/// A constructor that calls the executor once with two functions of its own and
/// returns `[executor, resolve, reject]`, so the test can compare identities.
const CALLS_ONCE: &str = "(function Ctor(executor) { \
    var res = function res() {}; \
    var rej = function rej() {}; \
    executor(res, rej); \
    return [executor, res, rej]; \
  })";

/// A constructor that never calls its executor.
const IGNORES_EXECUTOR: &str = "(function Ctor(executor) { return { tag: 1 }; })";

/// A constructor that calls the executor twice — the 27.5.1.5 step-3 guard.
const CALLS_TWICE: &str = "(function Ctor(executor) { \
    executor(function () {}, function () {}); \
    executor(function () {}, function () {}); \
    return 'unreachable'; \
  })";

/// `CALLS_ONCE` plus enough allocation after the executor call to make the
/// collector reuse a freed slot.
///
/// The sweep is lazy (`Heap::force_collect` marks and counts; `sweep_step`
/// drains dead slots as later allocations need them), so an unrooted record is
/// still *readable* right after a collection — a test that only reads it back
/// would pass either way. It has to be read back after enough allocation has
/// gone through the free list for its slot to be handed to something else,
/// which is what turns the stale handle into a wrong answer instead of a
/// correct-looking one.
const CALLS_ONCE_THEN_CHURNS: &str = "(function Ctor(executor) { \
    var res = function res() {}; \
    var rej = function rej() {}; \
    executor(res, rej); \
    var keep = []; \
    for (var i = 0; i < 400; i++) { keep.push({ n: i, s: 'churn' + i }); } \
    return [executor, res, rej, keep.length]; \
  })";

#[test]
fn the_pair_is_the_exact_functions_the_constructor_received() {
    // The whole point of the record round-trip: `NewPromiseCapability` never
    // makes the resolving functions itself, so what it returns is what the
    // constructor's code was handed. A `27.5.1.1`-style implementation — one
    // that built the pair up front and passed it in — would still satisfy
    // 27.5.1.1, so this is the assertion that separates the two algorithms.
    let observed = observe(CALLS_ONCE, None);
    let (_promise, resolve, reject) = observed.outcome.expect("a capability");
    let returned = observed.returned.expect("the constructor's array");
    assert_eq!(returned.len(), 3);
    assert_eq!(resolve, returned[1]);
    assert_eq!(reject, returned[2]);
    // The first element is the executor the seam handed to `Construct`, and
    // `returned` came out of `promise` — so `Construct`'s own return value is
    // what step 8 records as `[[Promise]]`. `NewPromiseCapability` validates
    // the *pair*, not the constructed object: a constructor that returns a
    // plain array is accepted, which is what makes the
    // "returned value is not a promise" test262 case possible at all.
    assert!(
        returned[0].as_object().is_some(),
        "the executor is a function object"
    );
}

#[test]
fn a_non_constructor_receiver_throws_a_type_error() {
    // 27.5.1.5 step 1. The arrow function is the interesting half: it is
    // callable but has no `[[Construct]]`, so the engine's constructor oracle —
    // not the call seam — is what has to reject it, and it has to do so before
    // the executor is ever built.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let thrown = {
        let mut interp = Interp::from_source(&mut heap, "(executor) => 1").expect("compile");
        interp.set_natives(Box::new(Seam));
        interp.run().expect("run");
        let ctor = interp.completion_value().expect("a completion value");
        match interp.new_promise_capability(ctor) {
            Ok(_) => panic!("an arrow function is not a constructor"),
            Err(JSException(thrown)) => thrown,
        }
    };
    assert_eq!(error_name(&mut heap, thrown), "TypeError");
}

#[test]
fn a_constructor_that_never_calls_the_executor_throws_a_type_error() {
    // 27.5.1.5 steps 7-8: the recorded `[[Resolve]]` is still `undefined`, so
    // `IsCallable` fails. This is the strict rule the plan's earlier draft
    // misattributed to 27.5.1.1 — `new Promise(function () {})` is legal
    // there because the pair is created before the executor runs.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let thrown = {
        let mut interp = Interp::from_source(&mut heap, IGNORES_EXECUTOR).expect("compile");
        interp.set_natives(Box::new(Seam));
        interp.run().expect("run");
        let ctor = interp.completion_value().expect("a completion value");
        match interp.new_promise_capability(ctor) {
            Ok(_) => panic!("a constructor that ignores its executor must throw"),
            Err(JSException(thrown)) => thrown,
        }
    };
    assert_eq!(error_name(&mut heap, thrown), "TypeError");
}

#[test]
fn a_constructor_that_calls_the_executor_twice_throws() {
    // 27.5.1.5 step 3's guard, reached through the `Construct`: the second
    // call is the executor's own `TypeError`, and it propagates out of
    // `NewPromiseCapability` rather than being swallowed.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let thrown = {
        let mut interp = Interp::from_source(&mut heap, CALLS_TWICE).expect("compile");
        interp.set_natives(Box::new(Seam));
        interp.run().expect("run");
        let ctor = interp.completion_value().expect("a completion value");
        match interp.new_promise_capability(ctor) {
            Ok(_) => panic!("the second executor call must throw"),
            Err(JSException(thrown)) => thrown,
        }
    };
    assert_eq!(error_name(&mut heap, thrown), "TypeError");
}

#[test]
fn the_record_survives_collection_inside_the_construct() {
    // `Interp::gc_protect` republishes the root set wholesale, so a record held
    // only in a Rust local across the `Construct` is unreachable for the whole
    // user-constructor body. Collection on every safepoint makes the window the
    // whole body rather than one lucky allocation budget, and the churn in
    // `CALLS_ONCE_THEN_CHURNS` is what turns the freed slot into a reused one so
    // the read back is a *wrong* answer rather than a stale-but-correct-looking
    // one (see that constant's comment).
    let observed = observe(CALLS_ONCE_THEN_CHURNS, Some(1));
    let (_, resolve, reject) = observed.outcome.expect("a capability");
    let returned = observed.returned.expect("the constructor's array");
    assert_eq!(returned.len(), 4);
    assert_eq!(resolve, returned[1]);
    assert_eq!(reject, returned[2]);
}
