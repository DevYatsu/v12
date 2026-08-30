//! The frozen `JsValue(u64)` machine word (boxed payloads are heap handles —
//! value representation and GC are inseparable). Bit-level details, the tag
//! table, and the layout rationale live in the crate documentation.

use crate::gc::{MarkSink, Trace};
use crate::handle::{Handle, Space};
use crate::object::{JsObject, V12BigInt, V12Symbol};
use crate::string::V12Str;

/// Mask identifying the NaN-boxed space: bits 63..51 all set.
///
/// A `JsValue` is a raw `f64` iff `(bits & BOX_MASK) != BOX_MASK`. The only
/// IEEE-754 doubles sharing this bit prefix are negative NaNs whose mantissa
/// bit 51 is set; [`JsValue::from_f64`] canonicalizes those to
/// [`QUIET_NAN_BITS`], so every other double (including `-Infinity`, `-0.0`,
/// and all positive NaN payloads) round-trips bit-exactly.
pub const BOX_MASK: u64 = 0xFFF8_0000_0000_0000;

/// IEEE-754 quiet NaN; the canonicalization target for doubles whose bit
/// pattern collides with [`BOX_MASK`].
pub const QUIET_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

const TAG_SHIFT: u32 = 44;
const TAG_MASK: u64 = 0xF_u64 << TAG_SHIFT;
/// Bits 50..48 sit between the box marker (which pins bit 51 to 1) and the
/// tag nibble; canonical boxed values carry zeros here.
const SPARE_MASK: u64 = 0x7_u64 << 48;
const REF_PAYLOAD_MASK: u64 = 0xFFFF_FFFF;
const SMI_PAYLOAD_MASK: u64 = 0x7FFF_FFFF;
/// Bits 43..0: everything under the tag nibble.
const LOWER_BITS_MASK: u64 = (1_u64 << TAG_SHIFT) - 1;

pub(crate) const TAG_SMI: u64 = 0;
pub(crate) const TAG_OBJECT: u64 = 1;
pub(crate) const TAG_STRING: u64 = 2;
pub(crate) const TAG_SYMBOL: u64 = 3;
pub(crate) const TAG_BIGINT: u64 = 4;
pub(crate) const TAG_UNDEFINED: u64 = 5;
pub(crate) const TAG_NULL: u64 = 6;
pub(crate) const TAG_FALSE: u64 = 7;
pub(crate) const TAG_TRUE: u64 = 8;
pub(crate) const TAG_HOLE: u64 = 9;
pub(crate) const TAG_EMPTY: u64 = 10;

/// One JavaScript value in a single `u64`. See the crate docs for the frozen
/// bit layout. `PartialEq`/`Eq`/`Hash` are **bitwise**: `+0.0 != -0.0` and a
/// NaN equals only itself bit-for-bit. This is identity semantics for engines,
/// not JS abstract equality.
///
/// The field is public so embedders can forge values; doing so forfeits the
/// canonical-form guarantee: [`JsValue::is_canonical`] answers `false` and
/// every type predicate ([`Self::is_object`] and friends) refuses the word.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct JsValue(pub u64);

impl JsValue {
    /// Smi range lower bound: −2³⁰ (31-bit signed payload).
    pub const SMI_MIN: i32 = -(1 << 30);
    /// Smi range upper bound: 2³⁰ − 1.
    pub const SMI_MAX: i32 = (1 << 30) - 1;

    const fn boxed(tag: u64, payload: u64) -> u64 {
        BOX_MASK | (tag << TAG_SHIFT) | payload
    }

    /// Raw bit pattern.
    pub const fn bits(self) -> u64 {
        self.0
    }

    const fn is_boxed(self) -> bool {
        self.0 & BOX_MASK == BOX_MASK
    }

    /// Tag nibble at bits 47..44. Meaningful only when [`Self::is_boxed`].
    const fn tag(self) -> u64 {
        (self.0 & TAG_MASK) >> TAG_SHIFT
    }

    const fn boxed_with(tag: u64) -> Self {
        Self(Self::boxed(tag, 0))
    }

    // ------------------------------------------------------------------
    // Constructors
    // ------------------------------------------------------------------

    /// Boxes a double. Doubles landing inside [`BOX_MASK`] (negative NaNs with
    /// mantissa bit 51 set) are canonicalized to the quiet NaN; every other
    /// value, including `-Infinity` and `-0.0`, round-trips bit-exactly.
    pub fn from_f64(f: f64) -> JsValue {
        let bits = f.to_bits();
        if bits & BOX_MASK == BOX_MASK {
            JsValue(QUIET_NAN_BITS)
        } else {
            JsValue(bits)
        }
    }

    /// Boxes an `i32` as a Smi. Returns `None` outside the i31 range
    /// `[−2³⁰, 2³⁰−1]`; callers promote such values to heap numbers.
    pub fn from_i32_smi(v: i32) -> Option<JsValue> {
        if (Self::SMI_MIN..=Self::SMI_MAX).contains(&v) {
            // Two's-complement low 31 bits == sign-extended i31 payload.
            Some(JsValue(Self::boxed(
                TAG_SMI,
                (v as u32 as u64) & SMI_PAYLOAD_MASK,
            )))
        } else {
            None
        }
    }

    /// The `undefined` singleton.
    pub const fn undefined() -> JsValue {
        Self::boxed_with(TAG_UNDEFINED)
    }

    /// The `null` singleton.
    pub const fn null() -> JsValue {
        Self::boxed_with(TAG_NULL)
    }

    /// The boolean `true`.
    pub const fn true_() -> JsValue {
        Self::boxed_with(TAG_TRUE)
    }

    /// The boolean `false`.
    pub const fn false_() -> JsValue {
        Self::boxed_with(TAG_FALSE)
    }

    /// The internal absent-element marker; never observable from conforming
    /// JavaScript.
    pub const fn hole() -> JsValue {
        Self::boxed_with(TAG_HOLE)
    }

    /// The internal empty-slot marker.
    pub const fn empty() -> JsValue {
        Self::boxed_with(TAG_EMPTY)
    }

    /// Boxes an object handle (tag names the space: no header check needed).
    pub fn object(h: Handle<JsObject>) -> JsValue {
        JsValue(Self::boxed(TAG_OBJECT, h.index() as u64))
    }

    /// Boxes a string handle.
    pub fn string(h: Handle<V12Str>) -> JsValue {
        JsValue(Self::boxed(TAG_STRING, h.index() as u64))
    }

    /// Boxes a symbol handle.
    pub fn symbol(h: Handle<V12Symbol>) -> JsValue {
        JsValue(Self::boxed(TAG_SYMBOL, h.index() as u64))
    }

    /// Boxes a BigInt handle.
    pub fn bigint(h: Handle<V12BigInt>) -> JsValue {
        JsValue(Self::boxed(TAG_BIGINT, h.index() as u64))
    }

    // ------------------------------------------------------------------
    // Predicates
    // ------------------------------------------------------------------

    /// Tag test gated on canonical form: forged words carrying a valid tag
    /// nibble but dirty spare/payload bits (or a reserved tag) never match,
    /// so every type predicate below doubles as a well-formedness proof.
    /// The box check stays explicit — [`Self::is_canonical`] alone is also
    /// `true` for raw doubles.
    ///
    /// In release builds the canonical-form validation is compiled out (the
    /// interpreter's values are canonical by construction); the predicate
    /// becomes the single tag check. Debug builds keep the full validation.
    #[inline]
    const fn has_tag(self, tag: u64) -> bool {
        let tag_matches = self.tag() == tag;
        if cfg!(debug_assertions) {
            self.is_boxed() && self.is_canonical() && tag_matches
        } else {
            self.is_boxed() && tag_matches
        }
    }

    /// Raw (unboxed) double.
    #[inline]
    pub const fn is_f64(self) -> bool {
        !self.is_boxed()
    }

    /// Smi.
    #[inline]
    pub const fn is_smi(self) -> bool {
        self.has_tag(TAG_SMI)
    }

    /// Heap reference of any space.
    #[inline]
    pub const fn is_ref(self) -> bool {
        let tag_matches = matches!(
            self.tag(),
            TAG_OBJECT | TAG_STRING | TAG_SYMBOL | TAG_BIGINT
        );
        if cfg!(debug_assertions) {
            self.is_boxed() && self.is_canonical() && tag_matches
        } else {
            self.is_boxed() && tag_matches
        }
    }

    #[inline]
    pub const fn is_object(self) -> bool {
        self.has_tag(TAG_OBJECT)
    }

    #[inline]
    pub const fn is_string(self) -> bool {
        self.has_tag(TAG_STRING)
    }

    #[inline]
    pub const fn is_symbol(self) -> bool {
        self.has_tag(TAG_SYMBOL)
    }

    #[inline]
    pub const fn is_bigint(self) -> bool {
        self.has_tag(TAG_BIGINT)
    }

    #[inline]
    pub const fn is_undefined(self) -> bool {
        self.has_tag(TAG_UNDEFINED)
    }

    #[inline]
    pub const fn is_null(self) -> bool {
        self.has_tag(TAG_NULL)
    }

    #[inline]
    pub const fn is_boolean(self) -> bool {
        let tag_matches = matches!(self.tag(), TAG_TRUE | TAG_FALSE);
        if cfg!(debug_assertions) {
            self.is_boxed() && self.is_canonical() && tag_matches
        } else {
            self.is_boxed() && tag_matches
        }
    }

    #[inline]
    pub const fn is_true(self) -> bool {
        self.has_tag(TAG_TRUE)
    }

    #[inline]
    pub const fn is_false(self) -> bool {
        self.has_tag(TAG_FALSE)
    }

    /// Internal absent-element marker.
    #[inline]
    pub const fn is_hole(self) -> bool {
        self.has_tag(TAG_HOLE)
    }

    /// Internal empty-slot marker.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.has_tag(TAG_EMPTY)
    }

    /// True when all unused payload bits are zero (canonical form). Raw
    /// doubles use every bit and are always canonical; tags 11..15 are
    /// reserved and therefore non-canonical.
    pub const fn is_canonical(self) -> bool {
        if !self.is_boxed() {
            return true;
        }
        if self.0 & SPARE_MASK != 0 {
            return false;
        }
        let payload_mask = match self.tag() {
            TAG_SMI => SMI_PAYLOAD_MASK,
            TAG_OBJECT | TAG_STRING | TAG_SYMBOL | TAG_BIGINT => REF_PAYLOAD_MASK,
            TAG_UNDEFINED..=TAG_EMPTY => 0,
            _ => return false,
        };
        self.0 & LOWER_BITS_MASK & !payload_mask == 0
    }

    // ------------------------------------------------------------------
    // Extractors
    // ------------------------------------------------------------------

    /// Raw double, bit-exact (`Some(-0.0)` stays `-0.0`; NaN payloads are
    /// whatever was stored, already canonicalized at construction).
    #[inline]
    pub fn as_f64(self) -> Option<f64> {
        if self.is_boxed() {
            None
        } else {
            Some(f64::from_bits(self.0))
        }
    }

    /// Smi payload with sign extension from bit 30.
    #[inline]
    pub fn as_smi(self) -> Option<i32> {
        if !self.is_smi() {
            return None;
        }
        let p = (self.0 & SMI_PAYLOAD_MASK) as u32;
        // Reinterpret the low 31 bits as two's complement i31.
        Some(((p << 1) as i32) >> 1)
    }

    #[inline]
    pub fn as_object(self) -> Option<Handle<JsObject>> {
        self.ref_handle(TAG_OBJECT).map(Handle::new)
    }

    #[inline]
    pub fn as_string(self) -> Option<Handle<V12Str>> {
        self.ref_handle(TAG_STRING).map(Handle::new)
    }

    #[inline]
    pub fn as_symbol(self) -> Option<Handle<V12Symbol>> {
        self.ref_handle(TAG_SYMBOL).map(Handle::new)
    }

    #[inline]
    pub fn as_bigint(self) -> Option<Handle<V12BigInt>> {
        self.ref_handle(TAG_BIGINT).map(Handle::new)
    }

    #[inline]
    pub fn as_bool(self) -> Option<bool> {
        if self.is_true() {
            Some(true)
        } else if self.is_false() {
            Some(false)
        } else {
            None
        }
    }

    fn ref_handle(self, expected_tag: u64) -> Option<u32> {
        if self.is_boxed() && self.tag() == expected_tag {
            Some((self.0 & REF_PAYLOAD_MASK) as u32)
        } else {
            None
        }
    }

    /// Space + slot index if this value boxes a heap reference.
    pub(crate) fn as_slot(self) -> Option<(Space, u32)> {
        let idx = (self.0 & REF_PAYLOAD_MASK) as u32;
        match self.tag() {
            TAG_OBJECT if self.is_boxed() => Some((Space::Objects, idx)),
            TAG_STRING if self.is_boxed() => Some((Space::Strings, idx)),
            TAG_SYMBOL if self.is_boxed() => Some((Space::Symbols, idx)),
            TAG_BIGINT if self.is_boxed() => Some((Space::Bigints, idx)),
            _ => None,
        }
    }
}

impl Trace for JsValue {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        if let Some((space, index)) = self.as_slot() {
            sink.mark_slot(space, index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_singletons_have_exact_bits() {
        assert_eq!(JsValue::undefined().bits(), 0xFFF8_5000_0000_0000);
        assert_eq!(JsValue::null().bits(), 0xFFF8_6000_0000_0000);
        assert_eq!(JsValue::false_().bits(), 0xFFF8_7000_0000_0000);
        assert_eq!(JsValue::true_().bits(), 0xFFF8_8000_0000_0000);
        assert_eq!(JsValue::hole().bits(), 0xFFF8_9000_0000_0000);
        assert_eq!(JsValue::empty().bits(), 0xFFF8_A000_0000_0000);
        // Smi(0): tag 0, no payload — exactly the box mask itself.
        assert_eq!(JsValue::from_i32_smi(0).unwrap().bits(), BOX_MASK);
    }

    #[test]
    fn specials_are_canonical_and_predicated() {
        for v in [
            JsValue::undefined(),
            JsValue::null(),
            JsValue::true_(),
            JsValue::false_(),
            JsValue::hole(),
            JsValue::empty(),
        ] {
            assert!(v.is_canonical(), "{v:?}");
            assert!(!v.is_f64());
            assert!(!v.is_ref());
            assert!(!v.is_smi());
        }
        assert!(JsValue::undefined().is_undefined());
        assert!(JsValue::null().is_null());
        assert!(JsValue::true_().is_true() && JsValue::true_().is_boolean());
        assert!(JsValue::false_().is_false() && JsValue::false_().is_boolean());
        assert!(JsValue::hole().is_hole());
        assert!(JsValue::empty().is_empty());
        assert_eq!(JsValue::true_().as_bool(), Some(true));
        assert_eq!(JsValue::false_().as_bool(), Some(false));
        assert_eq!(JsValue::null().as_bool(), None);
    }

    #[test]
    fn f64_roundtrip_is_bit_exact() {
        // Everything here must pass through untouched, including -Inf and -0.0.
        let samples = [
            0.0f64,
            -0.0,
            1.5,
            -1.5,
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            1e308,
            -1e-308,
            5e-324,                                // smallest subnormal
            f64::from_bits(0x7FF8_DEAD_BEEF_0000), // positive NaN, payload kept
            f64::from_bits(0xFFF7_FFFF_FFFF_FFFF), // negative NaN, mantissa bit 51 clear
        ];
        for &x in &samples {
            let v = JsValue::from_f64(x);
            assert!(
                v.is_f64(),
                "bits {:#018x} misrouted to the box",
                x.to_bits()
            );
            assert!(v.is_canonical());
            assert_eq!(v.as_f64().map(|y| y.to_bits()), Some(x.to_bits()));
        }
        // Spot-check the two most collision-prone patterns literally.
        assert_eq!(JsValue::from_f64(-0.0).bits(), 0x8000_0000_0000_0000);
        assert_eq!(
            JsValue::from_f64(f64::NEG_INFINITY).bits(),
            0xFFF0_0000_0000_0000
        );
    }

    #[test]
    fn colliding_negative_nans_are_canonicalized() {
        // Negative NaN with mantissa bit 51 set: inside BOX_MASK.
        let hostile = f64::from_bits(0xFFFF_1234_5678_9ABC);
        assert!(hostile.to_bits() & BOX_MASK == BOX_MASK);
        let v = JsValue::from_f64(hostile);
        assert_eq!(v.bits(), QUIET_NAN_BITS);
        // Compare bits, not values: `NaN == NaN` is false by IEEE semantics.
        assert_eq!(v.as_f64().map(|y| y.to_bits()), Some(QUIET_NAN_BITS));
    }

    #[test]
    fn smi_i31_boundaries() {
        assert!(JsValue::from_i32_smi(JsValue::SMI_MAX).is_some()); // 2^30 - 1
        assert!(JsValue::from_i32_smi(JsValue::SMI_MIN).is_some()); // -2^30
        assert!(JsValue::from_i32_smi(JsValue::SMI_MAX + 1).is_none()); // 2^30
        assert!(JsValue::from_i32_smi(JsValue::SMI_MIN - 1).is_none()); // -2^30 - 1
    }

    #[test]
    fn smi_sign_extension_and_payload_canonicality() {
        for v in [0, 1, -1, 42, -42, JsValue::SMI_MAX, JsValue::SMI_MIN] {
            let boxed = JsValue::from_i32_smi(v).unwrap();
            assert!(boxed.is_smi());
            assert!(boxed.is_canonical());
            assert_eq!(boxed.as_smi(), Some(v));
            // Payload is the sign-extended i31 in bits 30..0.
            assert_eq!(
                boxed.bits() & SMI_PAYLOAD_MASK,
                (v as u32 as u64) & SMI_PAYLOAD_MASK
            );
            assert_eq!(
                boxed.bits() & LOWER_BITS_MASK & !SMI_PAYLOAD_MASK,
                0,
                "spare bits not zero for {v}"
            );
        }
        assert_eq!(
            JsValue::from_i32_smi(-1).unwrap().bits() & SMI_PAYLOAD_MASK,
            0x7FFF_FFFF
        );
    }

    #[test]
    fn ref_values_are_canonical_across_full_handle_range() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let o = heap.alloc(JsObject::default());
        let s = heap.alloc(V12Str::latin1(vec![b'x']));
        let y = heap.alloc(V12Symbol);
        let b = heap.alloc(V12BigInt::default());
        for v in [
            JsValue::object(o),
            JsValue::string(s),
            JsValue::symbol(y),
            JsValue::bigint(b),
        ] {
            assert!(v.is_ref());
            assert!(v.is_canonical());
            assert_eq!(
                v.bits() & LOWER_BITS_MASK & !REF_PAYLOAD_MASK,
                0,
                "spare bits not zero"
            );
            assert_eq!(v.bits() & REF_PAYLOAD_MASK, v.as_slot().unwrap().1 as u64);
        }
        assert_eq!(JsValue::object(o).as_object(), Some(o));
        assert_eq!(JsValue::string(s).as_string(), Some(s));
        assert_eq!(JsValue::symbol(y).as_symbol(), Some(y));
        assert_eq!(JsValue::bigint(b).as_bigint(), Some(b));
        // Cross-space extractors refuse.
        assert_eq!(JsValue::object(o).as_string(), None);

        // Max handle index still encodes canonically (payload bits 31..0 full).
        let max_ref = JsValue(BOX_MASK | (TAG_OBJECT << TAG_SHIFT) | u64::from(u32::MAX));
        assert!(max_ref.is_canonical());
        assert_eq!(max_ref.as_object().map(Handle::index), Some(u32::MAX));
    }

    #[test]
    fn forged_non_canonical_values_are_detected() {
        // Object tag (1) with stray mid-bit at bit 40.
        let stray = JsValue(BOX_MASK | (1 << TAG_SHIFT) | (1u64 << 40) | 3);
        assert!(!stray.is_canonical());
        // Reserved tag 15.
        let reserved = JsValue(BOX_MASK | (15 << TAG_SHIFT));
        assert!(!reserved.is_canonical());
        // Special with nonzero payload.
        let dirty_special = JsValue(BOX_MASK | (5 << TAG_SHIFT) | 1);
        assert!(!dirty_special.is_canonical());
        // Nonzero spare bits between box marker and tag nibble.
        let dirty_spare = JsValue(BOX_MASK | (1u64 << 49) | (1 << TAG_SHIFT));
        assert!(!dirty_spare.is_canonical());
        // Smi with bit 31 set: above the i31 payload, below the tag.
        let wide_smi = JsValue(BOX_MASK | (1u64 << 31) | 5);
        assert!(wide_smi.tag() == TAG_SMI);
        assert!(!wide_smi.is_canonical());
        // Predicates never fire on reserved/dirty tags. The forge-detection
        // gate in `has_tag`/`is_ref` is compiled out in release builds (the
        // interpreter's values are canonical by construction), so this
        // assertion is debug-only.
        #[cfg(debug_assertions)]
        assert!(!stray.is_object() && !stray.is_smi() && !stray.is_ref());
    }

    #[test]
    fn cross_type_extraction_returns_none() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let h = heap.alloc(JsObject::default());
        let v = JsValue::object(h);
        assert_eq!(v.as_f64(), None);
        assert_eq!(v.as_smi(), None);
        assert_eq!(v.as_bool(), None);
        let d = JsValue::from_f64(1.0);
        assert_eq!(d.as_smi(), None);
        assert_eq!(d.as_object(), None);
    }
}
