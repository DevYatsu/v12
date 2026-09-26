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
/// The aggregate promise the combinator settles.
pub const SLOT_AGGREGATE: i32 = 1;
/// The capability reject function. The engine hands it back so the
/// interpreter can reject the aggregate without a second engine round trip.
pub const SLOT_REJECT: i32 = 2;
/// The results array (`KIND_ALL` only): one slot per element pulled so far.
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
