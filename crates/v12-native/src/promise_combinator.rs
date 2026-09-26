//! Layout of a `Promise.all` / `Promise.race` combinator state object.
//!
//! The combinator is driven from two places at once: the interpreter pulls
//! iterator steps and invokes `resolve`/`then` (both need the machine), while
//! the engine owns the promise bookkeeping (reaction jobs live there). The two
//! sides talk through this state object — an ordinary heap object whose slots
//! are named here so neither hardcodes an index.
//!
//! One state object per `Promise.all`/`Promise.race` call. The interpreter
//! holds the handle for the whole loop, so the object must be rooted: the
//! engine roots it at creation, and the interpreter parks it on the value
//! stack across safepoints.

use v12_heap::JsValue;

/// Which combinator this state drives, stored as an Smi in [`SLOT_KIND`].
pub const KIND_ALL: i32 = 0;
/// See [`KIND_ALL`].
pub const KIND_RACE: i32 = 1;

/// Reads `KIND_ALL`/`KIND_RACE` back out of a kind slot; `None` for a slot
/// that is not one of them (a corrupt state object).
#[must_use]
pub fn combinator_kind(slot: JsValue) -> Option<i32> {
    let raw = slot.as_smi()?;
    if raw == KIND_ALL {
        return Some(KIND_ALL);
    }
    if raw == KIND_RACE {
        return Some(KIND_RACE);
    }
    None
}

/// `KIND_ALL` or `KIND_RACE`.
pub const SLOT_KIND: i32 = 0;
/// The aggregate promise the combinator settles. It is
/// `promiseCapability.[[Promise]]` — the object `NewPromiseCapability(C)` had
/// `C` construct, not a promise allocated here — so what script receives is
/// the receiver's own promise (spec 27.5.4.1 step 13).
pub const SLOT_AGGREGATE: i32 = 1;
/// `promiseCapability.[[Reject]]` — whatever `C`'s own executor passed in,
/// because 27.5.1.5 does not build the pair. Read back by the engine to build
/// each element's `[onFulfilled, onRejected]` pair, and by the interpreter for
/// `IfAbruptRejectPromise` (the `[[Resolve]]` call that threw rejects through
/// it).
pub const SLOT_REJECT: i32 = 2;
/// The results array (`KIND_ALL` only): one slot per element pulled so far.
/// The aggregate fulfills with the results array when the count reaches zero.
pub const SLOT_RESULTS: i32 = 3;
/// `KIND_ALL` only: how many elements are still unsettled, counting one extra
/// for "the iterator has not been exhausted yet" (see `combinator::begin`).
/// The aggregate fulfills with the results array when the count reaches zero.
/// `KIND_RACE` leaves it at the exhaustion token forever — the race settles
/// on its first element and its spec loop returns the capability promise
/// unchanged.
pub const SLOT_REMAINING: i32 = 4;
/// The number of populated slots.
pub const STATE_SLOTS: usize = 5;

/// What `combinator_step` hands back to the interpreter, told apart by the
/// length of the carrier array it returns (both are heap arrays, so the shape
/// is the whole of the protocol).
pub mod carrier {
    /// A one-element carrier: there is no `then` to invoke, so the interpreter
    /// settles the element itself through
    /// [`super::registry::NativeRegistry::combinator_element_settled`] with
    /// element 0 as the value. This is the spec's non-callable-`then` path
    /// (27.5.4.1 step 6-l-i / 27.5.4.5.1 step 8-d).
    pub const SETTLE_NOW: usize = 1;
    /// A two-element carrier: the `[onFulfilled, onRejected]` pair to hand to
    /// `then`, verbatim as 27.5.4.1 step 6-m-i / 27.5.4.5.1 step 8-e build it.
    pub const PAIR: usize = 2;
}

/// The record an `onFulfilledElement` function object carries in its
/// `elements` (the same slot a bound function uses for its bound arguments),
/// so the interpreter can reach the element it belongs to when a reaction job
/// calls the handler. One per element; the function object is rooted, and
/// `elements` is traced, so the whole record is.
///
/// Index 3 is a one-slot mutable object rather than a bare value because the
/// spec's `alreadyResolved[index]` has to survive between two calls of the
/// same handler — a `then` free to call its resolve function twice must not
/// overwrite the first result.
pub mod element {
    /// The combinator state object ([`super::SLOT_KIND`] and friends).
    pub const STATE: usize = 0;
    /// The element's index, as an Smi.
    pub const INDEX: usize = 1;
    /// `promiseCapability.[[Resolve]]`, the function the last element's
    /// bookkeeping calls with the values array.
    pub const RESOLVE: usize = 2;
    /// The one-slot `alreadyResolved` flag object.
    pub const ALREADY: usize = 3;
    /// The number of slots in the record.
    pub const SLOTS: usize = 4;
}
