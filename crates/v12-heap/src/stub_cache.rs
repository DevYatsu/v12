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
//! ## Clearing
//!
//! [`StubCache::clear`] is O(1). Every entry carries the generation stamp
//! it was written under; clearing only advances the generation counter, so
//! stale entries read as absent on their next probe and are overwritten in
//! place as the cache refills. Nothing is swept, because nothing needs to be.

use crate::prop_key::PropKey;
use crate::shape::ShapeHandle;

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

/// One probe address: the recorded stub plus the generation it was written
/// under. `stamp == EMPTY_STAMP` or a stamp below the current generation
/// both mean "logically absent".
#[derive(Clone, Debug)]
struct Slot {
    stamp: u64,
    shape: ShapeHandle,
    key: PropKey,
    /// Property slot the stub resolves to (the cached answer).
    slot: u32,
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
        let home = hash as usize & CAPACITY_MASK;
        let order = [home, (home + probe_step(hash)) & CAPACITY_MASK];
        let written = Slot {
            stamp: self.generation,
            shape,
            key,
            slot,
        };
        for &idx in &order {
            let target = &mut self.slots[idx];
            if target.stamp != self.generation || (target.shape == shape && target.key == key) {
                *target = written;
                return;
            }
        }
        // Both candidates alive with other keys: overwrite home. The displaced
        // site re-records on its next miss; see the module docs for why no
        // smarter victim selection pays for itself.
        self.slots[home] = written;
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
    fn clear_bumps_generation_and_invalidates_everything() {
        let mut cache = StubCache::new();
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
}
