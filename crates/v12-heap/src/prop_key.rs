//! Unified property key ([`PropKey`]): a single tagged `u32` naming either a
//! string-keyed or a symbol-keyed property.
//!
//! Why one word instead of an enum of two payloads: property keys sit inside
//! hot per-property structures (transition tables, descriptors, inline
//! caches). A compact `Copy` key keeps those structures dense and lets every
//! consumer treat "the key" as one hashable, comparable value without
//! matching on an outer enum first. The tag is the high bit, leaving 31 bits
//! of payload — the full width of a heap handle index.
//!
//! Key identity is **reference identity**, not textual equality: two heap
//! strings holding the same characters occupy different slots and therefore
//! produce different keys. Canonicalizing equal texts onto one shared slot is
//! interning's job (`Heap::intern_string`), after which reference identity and
//! textual equality coincide.

use crate::gc::{MarkSink, Trace};
use crate::handle::Handle;
use crate::object::V12Symbol;
use crate::string::V12Str;

/// High bit of the key word: set = symbol handle, clear = string handle.
const SYMBOL_FLAG: u32 = 1 << 31;

/// Payload mask: the 31 low bits carrying the handle index.
const PAYLOAD_MASK: u32 = SYMBOL_FLAG - 1;

/// A property key: string handle or symbol handle, packed into one `u32`.
///
/// The bit layout is private; build keys with [`Self::from_string`],
/// [`Self::from_symbol`], or [`Self::from_parts`], and take them apart with
/// [`Self::as_u32`] / [`Self::parts`] (roundtrip-stable).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PropKey(u32);

impl PropKey {
    /// Key for a string-named property.
    pub fn from_string(h: Handle<V12Str>) -> Self {
        Self(h.index())
    }

    /// Key for a symbol-named property.
    pub fn from_symbol(h: Handle<V12Symbol>) -> Self {
        Self(SYMBOL_FLAG | h.index())
    }

    /// Key from a raw tag/payload split, inverse of [`Self::parts`]. The
    /// payload must fit in 31 bits — always true for genuine heap handles.
    ///
    /// ## Panics
    /// Panics when `payload` exceeds 31 bits; that can only come from corrupt
    /// input, never from a real handle, and silently truncating would forge a
    /// wrong-but-valid key.
    pub fn from_parts(is_symbol: bool, payload: u32) -> Self {
        assert!(
            payload & !PAYLOAD_MASK == 0,
            "property-key payload {payload:#x} exceeds 31 bits"
        );
        Self(if is_symbol {
            SYMBOL_FLAG | payload
        } else {
            payload
        })
    }

    /// Raw key word. Stable across runs and processes: the encoding is pure
    /// arithmetic over handle indices, with no random seeding.
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// `(is_symbol, payload)` split, inverse of [`Self::parts`].
    pub const fn parts(self) -> (bool, u32) {
        (self.0 & SYMBOL_FLAG != 0, self.0 & PAYLOAD_MASK)
    }

    /// True when this key names a symbol.
    pub const fn is_symbol(self) -> bool {
        self.0 & SYMBOL_FLAG != 0
    }

    /// True when this key names a string.
    pub const fn is_string(self) -> bool {
        self.0 & SYMBOL_FLAG == 0
    }

    /// The referenced string handle, or `None` for symbol keys.
    pub fn string(self) -> Option<Handle<V12Str>> {
        if self.is_string() {
            Some(Handle::new(self.0))
        } else {
            None
        }
    }

    /// The referenced symbol handle, or `None` for string keys.
    pub fn symbol(self) -> Option<Handle<V12Symbol>> {
        if self.is_symbol() {
            Some(Handle::new(self.0 & PAYLOAD_MASK))
        } else {
            None
        }
    }

    /// Cheap key mixing for hash tables and cache probes.
    ///
    /// A fixed integer avalanche finalizer (multiply/xorshift rounds): every
    /// input bit influences every output bit, so low-bit table indices do not
    /// correlate with key patterns (consecutive string slots land in
    /// different buckets). Pure and unscaled — no random seed — so a given
    /// key hashes identically in every process, which is what makes persisted
    /// caches and reproducible tests possible. Collisions remain legal; only
    /// equality ([`PartialEq`] on the raw word) decides identity.
    pub fn hash(self) -> u32 {
        let mut z = self.0 ^ (self.0 >> 16);
        z = z.wrapping_mul(0x7FEF_352D);
        z ^= z >> 15;
        z = z.wrapping_mul(0x846C_A68B);
        z ^ (z >> 16)
    }
}

impl Trace for PropKey {
    fn trace(&self, _sink: &mut MarkSink<'_>) {
        // PropKey is a plain u32 with no GC handles; nothing to trace.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_pair(is_symbol: bool, payload: u32) -> PropKey {
        PropKey::from_parts(is_symbol, payload)
    }

    #[test]
    fn roundtrip_both_arms() {
        for &(is_symbol, payload) in &[
            (false, 0),
            (false, 1),
            (false, u32::MAX >> 1),
            (true, 0),
            (true, 42),
            (true, u32::MAX >> 1),
        ] {
            let k = key_pair(is_symbol, payload);
            assert_eq!(k.as_u32() & SYMBOL_FLAG != 0, is_symbol);
            assert_eq!(k.parts(), (is_symbol, payload));
            // Rebuilding from the split reproduces the identical word.
            let (sym, payload) = k.parts();
            assert_eq!(PropKey::from_parts(sym, payload), k);
        }
    }

    #[test]
    fn typed_constructors_agree_with_from_parts() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let s = heap.alloc(crate::V12Str::latin1(b"name".to_vec()));
        let y = heap.alloc(crate::V12Symbol);

        let ks = PropKey::from_string(s);
        let ky = PropKey::from_symbol(y);
        assert_eq!(ks, PropKey::from_parts(false, s.index()));
        assert_eq!(ky, PropKey::from_parts(true, y.index()));
        assert_eq!(ks.string(), Some(s));
        assert_eq!(ks.symbol(), None);
        assert_eq!(ky.symbol(), Some(y));
        assert_eq!(ky.string(), None);
        assert!(ks.is_string() && !ks.is_symbol());
        assert!(ky.is_symbol() && !ky.is_string());
    }

    #[test]
    fn distinct_keys_never_collide_on_identity() {
        // Same payload, opposite arms: different keys.
        assert_ne!(key_pair(false, 7), key_pair(true, 7));
        // Different payloads within one arm: different keys.
        assert_ne!(key_pair(false, 7), key_pair(false, 8));
        assert_ne!(key_pair(true, 7), key_pair(true, 8));
        // Equality is exact on the raw word.
        assert_eq!(key_pair(true, 9), key_pair(true, 9));
    }

    #[test]
    fn hash_is_stable_and_disperses() {
        let samples: Vec<PropKey> = (0..64u32)
            .map(|i| key_pair(i % 2 == 0, i * 0x9E37 % 1000))
            .collect();

        // Stability: repeated calls and copies agree exactly (pure function
        // of the key word, no per-process seed).
        for &k in &samples {
            assert_eq!(k.hash(), k.hash());
            assert_eq!(k.hash(), key_pair(k.parts().0, k.parts().1).hash());
        }

        // Dispersion: these 64 structured keys must not collapse onto a
        // handful of values. Exact distinct-count pinning would overfit the
        // mixer; a wide floor catches a broken (constant/linear) hash.
        let hashes: std::collections::HashSet<u32> = samples.iter().map(|k| k.hash()).collect();
        assert!(
            hashes.len() >= 48,
            "poor dispersion: {} distinct hashes",
            hashes.len()
        );

        // Adjacent string slots must land in different low-bit buckets — the
        // whole point of avalanching sequential handle indices.
        let consecutive: std::collections::HashSet<u32> = (0..16u32)
            .map(|i| key_pair(false, i).hash() & 0xF)
            .collect();
        assert!(
            consecutive.len() >= 8,
            "low bits cluster for consecutive keys"
        );
    }

    #[test]
    #[should_panic(expected = "exceeds 31 bits")]
    fn oversized_payload_panics_instead_of_aliasing() {
        // A payload with the tag bit set would silently re-tag the key.
        PropKey::from_parts(false, 0xFFFF_FFFF);
    }
}