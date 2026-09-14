//! The stub cache ([`StubCache`]): a fixed-size, open-addressed
//! memoization of shape-chain property lookups, keyed by the pair
//! `(shape, PropKey)` and answering with the property's slot number.
//!
//! ## Design
//!
//! Resolving a property walks a shape chain descriptor by descriptor;
//! inline caches instead ask "for this shape and key, which slot?" and
//! memoize the answer. The table has no chaining and no resizing: each
//! lookup probes at most two addresses — the home slot from the hash's low
//! half, then one secondary address derived from the high half (forced odd,
//! so on a power-of-two table it is always a nonzero, per-key offset rather
//! than an alias of home).
//!
//! Insertion writes the home slot when it is free or already ours, falls
//! back to the secondary, and otherwise evicts whatever occupies home.
//! Eviction is deliberately dumb (no LRU bookkeeping): a miss re-record is
//! exactly how hot sites refresh themselves, so the cache self-heals and
//! any metadata would cost more than the occasional lost stub.
//!
//! ## Proto entries (validity-guarded chain stubs)
//!
//! Plain stubs only serve own-data layouts. [`StubCache::lookup_proto`] /
//! [`StubCache::record_proto`] extend the same table to prototype-chain
//! hits: the entry additionally names the chain `holder` (and its shape)
//! plus the chain links observed at record time. A hit requires every
//! verify to hold — cache generation, proto generation, receiver link,
//! second link and intermediate shape (depth-2), holder shape — each one
//! integer compare, so steady-state proto reads stay O(1) and
//! invalidation is purely comparative, never a scan.
//!
//! Soundness notes (why the verifies suffice):
//!
//! * Only `Data` hits are recorded. Accessor semantics cannot be served
//!   from a slot, so accessors always take the slow path.
//! * Layout growth is append-only with stable slots, and duplicate keys
//!   keep first-match semantics (see `indexed`), so adds on the holder
//!   never move a recorded location — they only mint it a new shape,
//!   which the holder-shape verify turns into a miss + re-record.
//! * Shadowing adds on an intermediate mint *it* a new shape, caught by
//!   the intermediate-shape verify; link rewiring is caught by the link
//!   verifies no matter which call site performed it.
//! * Walks deeper than two links are never recorded (rare long chains
//!   keep the slow path).
//! * A live receiver keeps its whole chain traced, so a recorded holder
//!   cannot be reclaimed out from under a usable entry; the table itself
//!   is generation-cleared at every collection, closing shape-slot reuse.
//!
//! ## Tier-2 recipe
//!
//! A JIT guard site reuses this mechanism without touching stubs: emit the
//! `(shape, key)` probe via [`crate::Heap::stub_lookup_proto`] (hit =
//! holder + slot under the same verifies the interpreter uses), and on a
//! miss walk once plus [`crate::Heap::stub_record_proto`]. A
//! `ValidityCell`-flavored guard over the same assumption is
//! `(holder.validity_cell, serial)` checked with [`crate::Heap::guard_holds`]
//! — the cell a `[[SetPrototypeOf]]`/integrity path bumps.
//!
//! ## Clearing
//!
//! [`StubCache::clear`] is O(1). Every entry carries the generation stamp
//! it was written under; clearing only advances the generation counter, so
//! stale entries read as absent on their next probe and are overwritten in
//! place as the cache refills. Nothing is swept, because nothing needs to be.

use crate::object::JsObject;
use crate::prop_key::PropKey;
use crate::shape::ShapeHandle;
use crate::Handle;

/// Fixed slot count. A power of two so addressing is a mask and the forced-odd
/// secondary step stays coprime with the table size; sized at a few dozen KiB
/// so the working set of monomorphic/polymorphic sites fits without thrashing
/// while keeping worst-case memory bounded no matter how many sites exist.
pub const STUB_CACHE_CAPACITY: usize = 1024;

/// Address mask for indexing: `STUB_CACHE_CAPACITY` is a power of two, so
/// `hash & CAPACITY_MASK` covers every slot exactly once.
const CAPACITY_MASK: usize = STUB_CACHE_CAPACITY - 1;

/// Stamp marking a never-written slot. Live generations start above it, so
/// an all-zero-initialized table reads as empty without a fill pass.
const EMPTY_STAMP: u64 = 0;

/// First live generation; must exceed [`EMPTY_STAMP`] (see its comment).
const FIRST_GENERATION: u64 = EMPTY_STAMP + 1;

/// A verified stub-cache hit: which object's storage answers the read.
/// Own entries resolve to the CURRENT receiver (same shape means same
/// layout, but values are per-instance — reading the recorded holder
/// would serve a stale instance); proto entries resolve to the recorded
/// holder (the chain verifies pin its identity).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtoHit {
    /// Object whose storage answers: the current receiver for own
    /// entries, the recorded holder for proto entries.
    pub target: Handle<JsObject>,
    /// Slot on `target`.
    pub slot: u32,
    /// Recorded holder shape, for the caller's final verify on proto
    /// entries (own entries skip it: their layout comes from the
    /// already-matched receiver shape).
    pub holder_shape: ShapeHandle,
    /// True for own entries (no chain was consulted, none verified).
    pub is_own: bool,
}

/// One probe address: the recorded stub plus the generation it was written
/// under. `stamp == EMPTY_STAMP` or a stamp below the current generation
/// both mean "logically absent".
#[derive(Clone, Debug)]
struct Slot {
    stamp: u64,
    shape: ShapeHandle,
    key: PropKey,
    /// Property slot the stub resolves to (the cached answer): the slot on
    /// `holder` (the receiver itself for own entries).
    slot: u32,
    /// Object whose storage answers the read: the receiver for own
    /// entries, the prototype-chain owner for proto entries. A live
    /// receiver keeps its chain traced, so a usable entry's holder is
    /// always live.
    holder: Handle<JsObject>,
    /// `holder`'s shape at record time; the hit requires it unchanged.
    holder_shape: ShapeHandle,
    /// `receiver.prototype` at record time (`None` for own entries, whose
    /// lookup never consults the chain). The hit requires the link
    /// unchanged, catching every `[[SetPrototypeOf]]` uniformly.
    link0: Option<Handle<JsObject>>,
    /// `link0.prototype` at record time (`None` for own entries). Second
    /// link of the two-link verify for depth-2 entries.
    link1: Option<Handle<JsObject>>,
    /// Shape of `link0` at record time (meaningless for own entries). The
    /// hit requires it unchanged, catching shadowing adds on the
    /// intermediate object.
    mid_shape: ShapeHandle,
    /// `Heap::proto_generation` at record time; the hit requires equality
    /// (defense-in-depth invalidation for the ordinary `[[SetPrototypeOf]]`
    /// path and integrity raises, which both bump it).
    proto_gen: u64,
}

/// Pure, seedless mix of a cache key (splitmix64-style finalizer), mirroring
/// the element-key mixer elsewhere in this crate. Purity matters twice over:
/// a given `(shape, key)` lands identically in every process, which keeps
/// probe patterns — and therefore eviction-order tests — reproducible.
fn stub_hash(shape: ShapeHandle, key: PropKey) -> u64 {
    let mut z = (u64::from(shape.index()) << 32) | u64::from(key.as_u32());
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Secondary probe offset from the hash's high half, forced odd: on the
/// power-of-two table an even step could fold onto home's parity class and
/// waste the second probe, while an odd step guarantees a distinct address.
fn probe_step(hash: u64) -> usize {
    ((hash >> 32) as usize | 1) & CAPACITY_MASK
}

/// Fixed-size open-addressed memoization of `(shape, key) → slot`. Not GC
/// infrastructure: it holds handles but participates in no tracing — stale
/// entries are harmless by construction, since a lookup only trusts an entry
/// whose full key matches what the caller is currently asking about.
///
/// Single-mutator like the rest of the heap: intentionally `!Send + !Sync`
/// through its use inside [`crate::Heap`]-driven compilation paths.
#[derive(Clone, Debug)]
pub struct StubCache {
    slots: [Slot; STUB_CACHE_CAPACITY],
    generation: u64,
}

impl Default for StubCache {
    fn default() -> Self {
        // One vacant template cloned across the table. `Handle::new` and
        // `PropKey::from_parts` are not const-callable, so the array cannot
        // be filled by repeating a `const`. A vacant slot's stamp can never
        // equal a live generation, so its placeholder handle/key are never
        // compared against real keys.
        let vacant = Slot {
            stamp: EMPTY_STAMP,
            shape: ShapeHandle::new(u32::MAX),
            key: PropKey::from_parts(false, 0),
            slot: 0,
            holder: Handle::new(u32::MAX),
            holder_shape: ShapeHandle::new(u32::MAX),
            link0: None,
            link1: None,
            mid_shape: ShapeHandle::new(u32::MAX),
            proto_gen: 0,
        };
        StubCache {
            slots: std::array::from_fn(|_| vacant.clone()),
            generation: FIRST_GENERATION,
        }
    }
}

impl StubCache {
    /// An empty cache at generation 1.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cached slot for `(shape, key)`, or `None` on a miss. At most two
    /// probes; entries from earlier generations read as absent without ever
    /// being touched.
    pub fn lookup(&self, shape: ShapeHandle, key: PropKey) -> Option<u32> {
        let hash = stub_hash(shape, key);
        let home = hash as usize & CAPACITY_MASK;

        // Home first. A current-generation entry that isn't ours means our
        // insert collided here and may have gone secondary; a stale-or-empty
        // home means our insert would have taken home, so probing further
        // can only miss.
        if let Some(slot) = self.matching_slot(home, shape, key) {
            return Some(slot);
        }
        if self.slots[home].stamp == self.generation {
            let alt = (home + probe_step(hash)) & CAPACITY_MASK;
            return self.matching_slot(alt, shape, key);
        }
        None
    }

    /// The slot at `idx` when it names exactly `(shape, key)` in the current
    /// generation.
    fn matching_slot(&self, idx: usize, shape: ShapeHandle, key: PropKey) -> Option<u32> {
        let entry = &self.slots[idx];
        if entry.stamp == self.generation && entry.shape == shape && entry.key == key {
            Some(entry.slot)
        } else {
            None
        }
    }

    /// Records `slot` as the answer for `(shape, key)`, replacing any stub
    /// previously recorded for the same pair. Free-or-stale addresses are
    /// taken before live foreign ones; when both candidate addresses hold
    /// live entries for other keys, home is evicted.
    pub fn record(&mut self, shape: ShapeHandle, key: PropKey, slot: u32) {
        let hash = stub_hash(shape, key);
        let written = Slot {
            stamp: self.generation,
            shape,
            key,
            slot,
            holder: Handle::new(u32::MAX),
            holder_shape: shape,
            link0: None,
            link1: None,
            mid_shape: ShapeHandle::new(u32::MAX),
            proto_gen: 0,
        };
        self.place(hash, written);
    }

    /// Records a chain stub: a `Data` hit for receiver `(shape, key)` lives
    /// at `holder_slot` on `holder` (whose shape is `holder_shape`).
    /// `link0`/`link1`/`mid_shape` are the chain links and intermediate
    /// shape the caller observed while walking (`None` links mark an own
    /// entry: `holder` is the receiver and no chain is consulted).
    /// `proto_gen` is the heap's current proto generation. Callers pass
    /// only `Data` hits — accessor semantics cannot be served from a slot.
    /// Placement follows the same dumb-eviction policy as [`Self::record`].
    #[allow(clippy::too_many_arguments)]
    pub fn record_proto(
        &mut self,
        shape: ShapeHandle,
        key: PropKey,
        holder: Handle<JsObject>,
        holder_shape: ShapeHandle,
        holder_slot: u32,
        link0: Option<Handle<JsObject>>,
        link1: Option<Handle<JsObject>>,
        mid_shape: ShapeHandle,
        proto_gen: u64,
    ) {
        let hash = stub_hash(shape, key);
        let written = Slot {
            stamp: self.generation,
            shape,
            key,
            slot: holder_slot,
            holder,
            holder_shape,
            link0,
            link1,
            mid_shape,
            proto_gen,
        };
        self.place(hash, written);
    }

    /// Shared placement: home when free, stale, or already ours, else the
    /// secondary, else evict home. A miss re-record is how hot sites
    /// refresh themselves (see module docs).
    fn place(&mut self, hash: u64, written: Slot) {
        let home = hash as usize & CAPACITY_MASK;
        let order = [home, (home + probe_step(hash)) & CAPACITY_MASK];
        for &idx in &order {
            let target = &mut self.slots[idx];
            if target.stamp != self.generation
                || (target.shape == written.shape && target.key == written.key)
            {
                *target = written;
                return;
            }
        }
        // Both candidates alive with other keys: overwrite home. The displaced
        // site re-records on its next miss; see the module docs for why no
        // smarter victim selection pays for itself.
        self.slots[home] = written;
    }

    /// Proto-capable lookup: the cached `(holder, holder_slot,
    /// holder_shape_at_record)` for a receiver of `shape` reading `key`,
    /// or `None` on any mismatch. At most two probes; entries from earlier
    /// generations read as absent.
    ///
    /// The caller supplies the CURRENTLY OBSERVED chain state — the heap
    /// proto generation, the receiver's prototype link, that link's
    /// prototype link, and the intermediate shape — and the hit requires
    /// every recorded verify to still hold (see module docs for why the
    /// set suffices). Each verify is one integer compare: steady-state
    /// proto reads stay O(1), invalidation is purely comparative.
    ///
    /// The caller finishes the job by checking the returned
    /// `holder_shape` against the live holder's shape
    /// (see `Heap::stub_lookup_proto`): the cache holds no heap reference
    /// to re-read it from, and that final compare closes shadowing adds,
    /// reconfigurations, and duplicate-key forks on the holder itself.
    /// The hit resolves its read target (see [`ProtoHit`]): the current
    /// receiver for own entries, the recorded holder for proto entries.
    #[allow(clippy::too_many_arguments)]
    pub fn lookup_proto(
        &self,
        receiver: Handle<JsObject>,
        shape: ShapeHandle,
        key: PropKey,
        proto_gen: u64,
        link0: Option<Handle<JsObject>>,
        link1: Option<Handle<JsObject>>,
        mid_shape: ShapeHandle,
    ) -> Option<ProtoHit> {
        let hash = stub_hash(shape, key);
        let home = hash as usize & CAPACITY_MASK;

        // Home first (same two-probe discipline as `lookup`: a
        // current-generation entry that isn't ours means our insert
        // collided here and may have gone secondary; a stale-or-empty home
        // means our insert would have taken home).
        if let Some(hit) =
            self.matching_proto(home, receiver, shape, key, proto_gen, link0, link1, mid_shape)
        {
            return Some(hit);
        }
        if self.slots[home].stamp == self.generation {
            let alt = (home + probe_step(hash)) & CAPACITY_MASK;
            return self.matching_proto(alt, receiver, shape, key, proto_gen, link0, link1, mid_shape);
        }
        None
    }

    /// The hit at `idx` when it names exactly `(shape, key)` in the
    /// current generation with every chain verify still holding.
    #[allow(clippy::too_many_arguments)]
    fn matching_proto(
        &self,
        idx: usize,
        receiver: Handle<JsObject>,
        shape: ShapeHandle,
        key: PropKey,
        proto_gen: u64,
        link0: Option<Handle<JsObject>>,
        link1: Option<Handle<JsObject>>,
        mid_shape: ShapeHandle,
    ) -> Option<ProtoHit> {
        let entry = &self.slots[idx];
        if entry.stamp != self.generation || entry.shape != shape || entry.key != key {
            return None;
        }
        if entry.proto_gen != proto_gen {
            return None;
        }
        let is_own = entry.link0.is_none();
        if let Some(recorded0) = entry.link0 {
            // Proto entry: the receiver link, the second link, and the
            // intermediate shape must all still match. (Depth-1 entries
            // record `link1` too; rewiring past the holder only costs an
            // extra miss, never a wrong hit.)
            if link0 != Some(recorded0) || entry.link1 != link1 || entry.mid_shape != mid_shape
            {
                return None;
            }
        }
        Some(ProtoHit {
            // Own layout is shared, values are not: serve the current
            // receiver. Proto chains pin the recorded holder's identity
            // via the verifies above: serve it.
            target: if is_own { receiver } else { entry.holder },
            slot: entry.slot,
            holder_shape: entry.holder_shape,
            is_own,
        })
    }

    /// Invalidates every recorded stub in O(1) by advancing the generation:
    /// all existing stamps fall out of currency and read as absent.
    pub fn clear(&mut self) {
        self.generation += 1;
    }

    /// Current generation counter; strictly increases across `clear` calls,
    /// unchanged by lookups and records.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a key from a raw payload (string-arm); payloads only need to
    /// be distinct for these tests, not reference real strings.
    fn key(payload: u32) -> PropKey {
        PropKey::from_parts(false, payload)
    }

    #[test]
    fn hit_after_insert_and_miss_on_unknown() {
        let mut cache = StubCache::new();
        let shape = ShapeHandle::new(7);
        assert_eq!(cache.lookup(shape, key(1)), None, "empty cache misses");

        cache.record(shape, key(1), 3);
        assert_eq!(cache.lookup(shape, key(1)), Some(3));

        // Unknown key on the same shape…
        assert_eq!(cache.lookup(shape, key(2)), None);
        // …same key on a different shape: identity is the full pair.
        assert_eq!(cache.lookup(ShapeHandle::new(8), key(1)), None);

        // Re-recording the same pair overwrites in place.
        cache.record(shape, key(1), 9);
        assert_eq!(cache.lookup(shape, key(1)), Some(9));
    }

    /// Finds three distinct key payloads sharing one full probe signature —
    /// home address *and* secondary step — under `shape`. Matching both is
    /// what forces the third insertion onto the eviction path deterministically
    /// (same home alone would let it take a free secondary elsewhere).
    fn colliding_trio(shape: ShapeHandle) -> [PropKey; 3] {
        let mut by_signature: std::collections::HashMap<(usize, usize), Vec<u32>> =
            std::collections::HashMap::new();
        let mut payload = 0u32;
        loop {
            let k = key(payload);
            let h = stub_hash(shape, k);
            let bucket = by_signature
                .entry((h as usize & CAPACITY_MASK, probe_step(h)))
                .or_default();
            bucket.push(payload);
            if bucket.len() == 3 {
                return [key(bucket[0]), key(bucket[1]), key(bucket[2])];
            }
            payload += 1;
        }
    }

    #[test]
    fn secondary_probe_serves_collided_pairs() {
        let mut cache = StubCache::new();
        let shape = ShapeHandle::new(1);
        let [ka, kb, kc] = colliding_trio(shape);

        // First pair fills home, then secondary.
        cache.record(shape, ka, 100);
        cache.record(shape, kb, 101);
        assert_eq!(cache.lookup(shape, ka), Some(100));
        assert_eq!(cache.lookup(shape, kb), Some(101));

        // Third insert faces both addresses live with foreign keys and
        // evicts the home occupant.
        cache.record(shape, kc, 102);
        assert_eq!(cache.lookup(shape, kc), Some(102));
        assert_eq!(
            cache.lookup(shape, kb),
            Some(101),
            "secondary occupant survives a home eviction"
        );
        assert_eq!(cache.lookup(shape, ka), None, "home occupant was evicted");
    }

    #[test]
    fn sustained_overload_bounds_residency_and_favors_recent_entries() {
        let mut cache = StubCache::new();
        let total = STUB_CACHE_CAPACITY * 4;
        for i in 0..total as u32 {
            cache.record(ShapeHandle::new(i % 16), key(i), i);
        }

        // Fixed size means fixed residency: whatever the insert history, no
        // more than CAPACITY distinct stubs can ever resolve.
        let hits_of = |range: std::ops::Range<usize>| {
            range
                .filter(|&i| {
                    cache
                        .lookup(ShapeHandle::new((i % 16) as u32), key(i as u32))
                        .is_some()
                })
                .count()
        };
        assert!(hits_of(0..total) <= STUB_CACHE_CAPACITY);

        // Survivors skew toward recent inserts: the final quarter went in
        // last and faces only intra-wave collisions, while the first
        // quarter was ground down by three subsequent waves. Exact counts
        // would pin the mixer; a wide margin catches a scheme that never
        // evicts (hits would spread evenly) or one that evicts everything.
        let early = hits_of(0..total / 4);
        let late = hits_of(3 * total / 4..total);
        assert!(
            late > early.saturating_mul(2),
            "recent stubs should dominate: early={early}, late={late}"
        );
    }

    #[test]
    fn clear_bumps_generation_and_invalidates_everything() {        let mut cache = StubCache::new();
        let gen0 = cache.generation();
        cache.record(ShapeHandle::new(3), key(5), 11);
        assert_eq!(cache.lookup(ShapeHandle::new(3), key(5)), Some(11));

        cache.clear();
        assert_eq!(cache.generation(), gen0 + 1, "clear advances the counter");
        assert_eq!(cache.lookup(ShapeHandle::new(3), key(5)), None);

        // Recording again works immediately: stale entries are overwritten,
        // not swept, so nothing blocks refilling.
        cache.record(ShapeHandle::new(3), key(5), 12);
        assert_eq!(cache.lookup(ShapeHandle::new(3), key(5)), Some(12));

        // Repeated clears keep advancing; lookups stay clean misses.
        cache.clear();
        cache.clear();
        assert_eq!(cache.generation(), gen0 + 3);
        assert_eq!(cache.lookup(ShapeHandle::new(3), key(5)), None);
    }

    /// Object handles for proto-entry tests (identity only; no heap).
    fn obj(index: u32) -> Handle<JsObject> {
        Handle::new(index)
    }

    fn shape(index: u32) -> ShapeHandle {
        ShapeHandle::new(index)
    }

    /// Expected hit under test: same shape the pure-value API cannot
    /// verify itself (no heap), so tests spell it out.
    fn hit(target: Handle<JsObject>, slot: u32, holder_shape: ShapeHandle, is_own: bool) -> ProtoHit {
        ProtoHit {
            target,
            slot,
            holder_shape,
            is_own,
        }
    }

    #[test]
    fn proto_own_entry_resolves_current_receiver() {
        let mut cache = StubCache::new();
        // Own entry: holder is the recording receiver, no links recorded.
        cache.record_proto(shape(1), key(1), obj(10), shape(1), 4, None, None, shape(99), 7);
        // Hits whether or not a receiver currently links anywhere — and
        // always resolves to the CURRENT receiver (per-instance values).
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, None, None, shape(99)),
            Some(hit(obj(30), 4, shape(1), true))
        );
        assert_eq!(
            cache.lookup_proto(
                obj(31),
                shape(1),
                key(1),
                7,
                Some(obj(20)),
                Some(obj(21)),
                shape(22)
            ),
            Some(hit(obj(31), 4, shape(1), true))
        );
        // Generation still gates.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 8, None, None, shape(99)),
            None
        );
        // Wrong shape/key still miss.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(2), key(1), 7, None, None, shape(99)),
            None
        );
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(2), 7, None, None, shape(99)),
            None
        );
    }

    #[test]
    fn proto_depth1_entry_verifies_link_and_shapes() {
        let mut cache = StubCache::new();
        // Receiver shape 1 → holder obj(10) at slot 4; chain link obj(10).
        cache.record_proto(
            shape(1),
            key(1),
            obj(10),
            shape(11),
            4,
            Some(obj(10)),
            Some(obj(12)),
            shape(11),
            7,
        );
        // Exact replay hits (any receiver sharing the shape+chain serves
        // the recorded holder).
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(10)), Some(obj(12)), shape(11)),
            Some(hit(obj(10), 4, shape(11), false))
        );
        // Rewired receiver link misses.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(13)), Some(obj(12)), shape(11)),
            None
        );
        // Nulled receiver link misses.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, None, None, shape(11)),
            None
        );
        // Rewiring past the holder only costs a miss, never a wrong hit.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(10)), Some(obj(14)), shape(11)),
            None
        );
        // Changed intermediate shape misses.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(10)), Some(obj(12)), shape(15)),
            None
        );
        // Bumped proto generation misses.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 9, Some(obj(10)), Some(obj(12)), shape(11)),
            None
        );
    }

    #[test]
    fn proto_depth2_entry_verifies_both_links() {
        let mut cache = StubCache::new();
        // Receiver shape 1 → P0 obj(10) → holder obj(11) at slot 6.
        cache.record_proto(
            shape(1),
            key(1),
            obj(11),
            shape(12),
            6,
            Some(obj(10)),
            Some(obj(11)),
            shape(13),
            7,
        );
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(10)), Some(obj(11)), shape(13)),
            Some(hit(obj(11), 6, shape(12), false))
        );
        // Either link rewired, or the intermediate reshaped, misses.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(19)), Some(obj(11)), shape(13)),
            None
        );
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(10)), Some(obj(19)), shape(13)),
            None
        );
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(10)), Some(obj(11)), shape(19)),
            None
        );
    }

    #[test]
    fn proto_rerecord_same_pair_overwrites() {
        let mut cache = StubCache::new();
        cache.record_proto(shape(1), key(1), obj(10), shape(11), 4, Some(obj(10)), None, shape(11), 7);
        cache.record_proto(shape(1), key(1), obj(20), shape(21), 9, Some(obj(20)), None, shape(21), 7);
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(20)), None, shape(21)),
            Some(hit(obj(20), 9, shape(21), false))
        );
        // The old holder's links no longer resolve the pair.
        assert_eq!(
            cache.lookup_proto(obj(30), shape(1), key(1), 7, Some(obj(10)), None, shape(11)),
            None
        );
    }
}
