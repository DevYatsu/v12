//! # v12-heap — values, handles, and the GC heap
//!
//! Foundation crate: v12 owns a handle-based heap. Values reference heap
//! objects by 32-bit index, never by raw pointer, so roots are enumerable
//! without machine-stack scanning in interpreted frames, and a future moving
//! collector would fix up handles rather than pointers. The trait seam that
//! keeps an `mmtk-core` swap open is the [`Trace`]/[`MarkSink`] pair.
//!
//! The crate provides:
//!
//! * values ([`JsValue`], frozen NaN-boxed layout below) and typed
//!   [`Handle`]s;
//! * the mark-sweep [`Heap`] with growth-triggered, stress-testable,
//!   lazy-sweep collection;
//! * shapes with transition trees and descriptors, plus validity cells for
//!   guarded assumptions (prototype identity, integrity levels);
//! * the elements-kind lattice for integer-indexed storage;
//! * the [`StubCache`] memoizing shape+key → slot lookups;
//! * strings as flat Latin-1/UTF-16 leaves with lazy Cons/Sliced composites,
//!   cached content hashing, and canonical-instance interning.
//!
//! Weak collections and ephemerons are not built yet.
//!
//! ## Frozen `JsValue(u64)` bit layout
//!
//! A `JsValue` is one of:
//!
//! * a **raw `f64`**: any bit pattern where `(bits & BOX_MASK) != BOX_MASK`;
//!   decoded with `f64::from_bits`.
//! * a **boxed value**: `(bits & BOX_MASK) == BOX_MASK`, i.e. bits 63..51 all
//!   set (the NaN-boxed space).
//!
//! Boxed layout:
//!
//! ```text
//! bits 63..51 : 1...1 (box marker; part of BOX_MASK)
//! bits 50..48 : spare — MUST be zero (canonical form)
//! bits 47..44 : tag nibble
//! bits 43..0  : payload (all unused bits MUST be zero — canonical form)
//!
//! tag  meaning    payload
//! 0    Smi        i31 stored in bits 30..0 (bits 43..31 zero)
//! 1    ObjectRef  handle index u32 in bits 31..0 (bits 43..32 zero)
//! 2    StringRef  handle index u32 in bits 31..0 (bits 43..32 zero)
//! 3    SymbolRef  handle index u32 in bits 31..0 (bits 43..32 zero)
//! 4    BigIntRef  handle index u32 in bits 31..0 (bits 43..32 zero)
//! 5    undefined  none (bits 43..0 zero)
//! 6    null       none
//! 7    false      none
//! 8    true       none
//! 9    hole       none (internal absent-element marker; never visible to JS)
//! 10   empty      none
//! 11..15          reserved; any value carrying them is non-canonical
//! ```
//!
//! Constructors produce canonical bits; [`JsValue::is_canonical`] asserts the
//! invariant and is exercised by tests (spare-bits-zero assertions).
//!
//! ### Why the tag sits at bits 47..44
//!
//! A tempting alternative puts the tag nibble directly under the box marker at
//! bits 51..48 — but the box predicate pins bit 51 to `1`, so tag values
//! `0..7` would fail their own box test and decode as garbage doubles. Hosting
//! the tag at bits 47..44 keeps the single-mask box predicate intact. Two
//! consequences, both tested:
//!
//! * `-Infinity` (`0xFFF0_0000_0000_0000`, mantissa bit 51 clear) stays a raw
//!   double. (A narrower box mask stopping at bit 52 would make it collide
//!   with the boxed space and become unrepresentable.)
//! * Negative NaNs whose mantissa bit 51 is set are canonicalized by
//!   [`JsValue::from_f64`] to the IEEE quiet NaN ([`QUIET_NAN_BITS`]); NaN
//!   payloads are unobservable through `JsValue` (TypedArray byte views bypass
//!   `JsValue` and keep payloads), consistent with ES NaN semantics.
//!
//! ## Handles and liveness
//!
//! A [`Handle`] is a typed `u32` index into one of four slot-storage spaces
//! (`JsObject`, `V12Str`, `V12Symbol`, `V12BigInt`). The tag inside a boxed
//! `JsValue` names the space, so downcasts need no header check.
//!
//! Stale-handle behavior:
//!
//! * **Debug builds** (`cfg!(debug_assertions)`): [`Heap::get`] and
//!   [`Heap::get_mut`] consult a per-space alive bitmap and **panic** on a
//!   dead-slot access (an object reclaimed by a previous collection).
//! * **Release builds**: no liveness check; a dead slot reads back whatever
//!   the lazy sweeper has not yet cleared (eventually the empty default).
//!   This is quiet corruption — run tests in debug.
//! * Known limitation: a freed index that has been *reused* by a
//!   later allocation aliases the new object; neither build detects that. The
//!   allocation contract below is the defense.
//!
//! Allocation contract: a freshly returned handle must be registered in
//! [`Heap::roots`] (or made reachable from an existing root) **before the next
//! `Heap::alloc` call**. Collection runs only inside `alloc` and inside the
//! explicit `force_collect`/`collect_if_needed` entry points, so the window
//! between `alloc` and rooting is safe.
//!
//! ## Garbage collection
//!
//! Non-moving mark-sweep over slot vectors with per-space free lists.
//! Marking is **iterative** (explicit worklist, never recursion) because
//! prototype chains and nested structures get deep. Sweeping is **lazy**:
//! the mark phase only publishes liveness, and dead slots return to their
//! space's free list in budgeted steps charged against later allocations.
//! The trigger is the heap-growth policy: collect when bytes allocated
//! since the last mark reach the live-bytes estimate at the last mark (2×
//! growth), with a floor of 1 MiB by default ([`GcPolicy::Growth`]);
//! [`GcPolicy::NoGC`] disables automatic collection for bring-up. A
//! `--gc-stress`-style cadence ([`Heap::gc_stress`]) forces collection every
//! *n* allocations (`n = 1` valid).
//!
//! Single-mutator design: [`Heap`] is intentionally `!Send + !Sync`.

#![forbid(unsafe_code)]

mod gc;
mod handle;
mod object;
mod prop_key;
mod shape;
mod string;
mod stub_cache;
mod value;

pub use gc::{GcPolicy, Heap, MarkSink, RootSet, Trace};
pub use handle::{Handle, HeapSpace, Space};
pub use object::{
    IntegrityLevel, JsObject, KIND_ARGUMENTS, KIND_ARRAY, KIND_FUNCTION, KIND_ORDINARY, V12BigInt,
    V12Symbol,
};
pub use prop_key::PropKey;
pub use shape::{
    Attrs, DESCRIPTORS_INLINE_CAP, Descriptor, Descriptors, Shape, ShapeHandle,
    TRANSITIONS_INLINE_CAP, Transitions, ValidityCellId,
};
pub use string::{CONCAT_EAGER_FLATTEN_MAX_UNITS, StrStorage, V12Str};
pub use stub_cache::{STUB_CACHE_CAPACITY, StubCache};
pub use value::{BOX_MASK, JsValue, QUIET_NAN_BITS};
