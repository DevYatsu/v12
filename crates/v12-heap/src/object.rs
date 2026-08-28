//! Minimal heap-object types: `JsObject`, `V12Symbol`, `V12BigInt`. Strings
//! live in their own module ([`crate::string`]); real BigInt semantics over
//! malachite arrive with the built-ins.

use crate::gc::{MarkSink, Trace};
use crate::handle::{Handle, HeapSpace, Space};
use crate::shape::ValidityCellId;

/// Default `JsObject` kind: the ordinary object. Further kinds are assigned
/// as they become needed (function, array, Proxy, …).
pub const KIND_ORDINARY: u8 = 0;

/// Object kind for user functions created by `Closure`.
pub const KIND_FUNCTION: u8 = 1;

/// Object kind for array literals.
pub const KIND_ARRAY: u8 = 2;

/// Object kind for arguments exotic objects (ES `ArgumentsExoticObject`).
///
/// Layout: `properties` holds named properties (`length`, `callee`),
/// `elements` holds indexed arguments, and `arguments_mapped` tracks the
/// exotic parameter alias (see `JsObject::arguments_mapped`).
pub const KIND_ARGUMENTS: u8 = 3;

/// Generator object. Interpreter contract (v12-interp/src/lib.rs:3094):
/// properties[0]=fn_idx (Smi), [1]=next_index (Smi), [2]=done (0.0/1.0);
/// elements=yield values (LoadInt eager path) or empty when real suspension used;
/// prototype=captured Env. DO NOT reorder without updating interp.
pub const KIND_GENERATOR: u8 = 4;

/// ES integrity levels ([`JsObject`]): how far an object has been locked
/// down. Transitions are monotone — sealing then freezing is legal, nothing
/// un-seals — so [`Heap::set_integrity_level`] only ever raises flags.
///
/// [`Heap::set_integrity_level`]: crate::Heap::set_integrity_level
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityLevel {
    /// Non-extensible; existing properties may remain configurable.
    Sealed,
    /// Sealed, and every own property additionally non-writable.
    Frozen,
}

/// A heap object: a kind/flags header plus dense property and element
/// vectors and a prototype link. Shapes describe the layout; this carries
/// the storage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JsObject {
    /// Object kind; [`KIND_ORDINARY`] by default.
    pub kind: u8,
    /// Kind-specific flag bits (see the `FLAG_*` constants).
    pub flags: u8,
    /// Named-property slots (shape-backed layout arrives with the object
    /// model work).
    pub properties: Vec<crate::JsValue>,
    /// Property keys (names) parallel to `properties` vector. `None` for
    /// empty slots; `Some(PropKey)` for populated slots. Used by native
    /// handlers that need to enumerate property names without interpreter's
    /// shape cache.
    pub property_keys: Vec<Option<crate::PropKey>>,
    /// Integer-indexed element slots.
    pub elements: Vec<crate::JsValue>,
    /// Prototype link; traced as a strong reference. Guards over this chain
    /// watch a validity-cell serial, not this field alone.
    pub prototype: Option<Handle<JsObject>>,
    /// The validity cell watching assumptions about this object (prototype
    /// identity, attribute stability), assigned lazily on first use. The
    /// registry holding serials lives on the heap.
    pub validity_cell: ValidityCellId,
    /// Arguments exotic mapping: `Some(map)` where `map[i]` is `Some(slot)`
    /// when indexed property `i` is aliased to the `slot`-th parameter slot,
    /// `None` for mapped holes and `None` for unmapped (strict) arguments.
    ///
    /// Only meaningful when `kind == KIND_ARGUMENTS`.
    pub arguments_mapped: Option<Box<[Option<u32>]>>,
}

impl JsObject {
    /// An ordinary object with empty storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// `[[Extensible]] == false`. Implied by both integrity transitions.
    pub const FLAG_NOT_EXTENSIBLE: u8 = 0b0000_0001;
    /// Every own property non-configurable: sealed or stricter.
    pub const FLAG_SEALED: u8 = 0b0000_0010;
    /// Every own property also non-writable: frozen.
    pub const FLAG_FROZEN: u8 = 0b0000_0100;

    /// True once sealed (or frozen): no property may be removed or
    /// reconfigured.
    pub fn is_sealed(&self) -> bool {
        self.flags & Self::FLAG_SEALED != 0
    }

    /// True only when frozen: sealed plus every own property non-writable.
    pub fn is_frozen(&self) -> bool {
        self.flags & Self::FLAG_FROZEN != 0
    }

    /// An ordinary object with the given properties and their keys.
    pub fn ordinary(properties: Vec<crate::JsValue>, property_keys: Vec<Option<crate::PropKey>>) -> Self {
        Self {
            kind: KIND_ORDINARY,
            properties,
            property_keys,
            ..Self::default()
        }
    }

    /// A function object: `elements[0]` holds the function index (Smi),
    /// `prototype` is the closure environment.
    pub fn function(func_index: u32, env: Option<Handle<JsObject>>) -> Self {
        Self {
            kind: KIND_FUNCTION,
            elements: vec![crate::JsValue::from_f64(f64::from(func_index))],
            prototype: env,
            ..Self::default()
        }
    }

    /// An array object with the given elements and length property.
    pub fn array(elements: Vec<crate::JsValue>) -> Self {
        let len = elements.len();
        Self {
            kind: KIND_ARRAY,
            properties: vec![crate::JsValue::from_f64(f64::from(len as u32))],
            property_keys: vec![Some(crate::PropKey::from_parts(false, 0))], // length property key (index 0 placeholder; caller should intern "length" when heap is available)
            elements,
            ..Self::default()
        }
    }

    /// An arguments exotic object with the given properties and elements.
    pub fn arguments(properties: Vec<crate::JsValue>, elements: Vec<crate::JsValue>, mapped: Option<Box<[Option<u32>]>>) -> Self {
        let prop_len = properties.len();
        Self {
            kind: KIND_ARGUMENTS,
            properties,
            property_keys: vec![None; prop_len],
            elements,
            arguments_mapped: mapped,
            ..Self::default()
        }
    }

    /// A generator object (suspended frame).
    pub fn generator() -> Self {
        Self { kind: KIND_GENERATOR, ..Default::default() }
    }

    /// A Promise object (ordinary kind with Promise-specific fields set later).
    pub fn promise() -> Self {
        Self {
            kind: KIND_ORDINARY,
            ..Self::default()
        }
    }


}
/// A heap symbol. Identity *is* the handle for now; descriptions,
/// well-known singletons, and `#private` names come with interning work.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct V12Symbol;

/// A heap BigInt placeholder: sign + little-endian base-256 magnitude. The
/// malachite-backed implementation lands with built-ins; this carries the
/// space and allocation accounting until then.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct V12BigInt {
    /// `true` when negative. Zero is canonically positive (`sign == false`).
    pub sign: bool,
    /// Magnitude, little-endian base 256, no trailing zero bytes for zero.
    pub magnitude_le: Vec<u8>,
}

impl HeapSpace for JsObject {
    const SPACE: Space = Space::Objects;
}
impl HeapSpace for V12Symbol {
    const SPACE: Space = Space::Symbols;
}
impl HeapSpace for V12BigInt {
    const SPACE: Space = Space::Bigints;
}

/// Rough retained-bytes estimate used by the GC growth trigger and live-byte
/// accounting. Deliberately approximate: capacities count, exact allocator
/// overhead does not. Implemented by the four heap-space payload types;
/// not meant to be implemented elsewhere.
pub trait SizeEstimate {
    fn approx_size(&self) -> usize;
}

const VALUE_BYTES: usize = core::mem::size_of::<crate::JsValue>();

impl SizeEstimate for JsObject {
    fn approx_size(&self) -> usize {
        // Header (kind, flags, prototype) + capacity of all slot vectors.
        let mapped = self
            .arguments_mapped
            .as_ref()
            .map_or(0, |m| m.len() * core::mem::size_of::<Option<u32>>());
        16 + self.properties.capacity() * VALUE_BYTES
            + self.elements.capacity() * VALUE_BYTES
            + self.property_keys.capacity() * core::mem::size_of::<Option<crate::PropKey>>()
            + mapped
    }
}

impl SizeEstimate for V12Symbol {
    fn approx_size(&self) -> usize {
        1 // opaque unit payload
    }
}

impl SizeEstimate for V12BigInt {
    fn approx_size(&self) -> usize {
        16 + self.magnitude_le.capacity()
    }
}

// Tracing: objects expose their children; the other spaces are leaves this
// wave but participate uniformly so future fields need no collector changes.

impl Trace for JsObject {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        self.properties.trace(sink);
        self.property_keys.trace(sink);
        self.elements.trace(sink);
        self.prototype.trace(sink);
    }
}

impl Trace for V12Symbol {
    fn trace(&self, _sink: &mut MarkSink<'_>) {}
}

impl Trace for V12BigInt {
    fn trace(&self, _sink: &mut MarkSink<'_>) {}

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JsValue;

    #[test]
    fn object_traces_children_through_sink() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let child = heap.alloc(JsObject::default());
        let parent = heap.alloc(JsObject {
            properties: vec![JsValue::object(child)],
            elements: vec![JsValue::object(child)],
            prototype: Some(child),
            ..JsObject::default()
        });
        // Force a collect that only sees the parent: the child must survive
        // via all three child-bearing fields.
        heap.add_root(JsValue::object(parent));
        heap.force_collect();
        assert_eq!(heap.live_count::<JsObject>(), 2);
    }

    #[test]
    fn size_estimates_grow_with_capacity() {
        let mut o = JsObject::default();
        let base = o.approx_size();
        o.properties.reserve(100);
        assert!(o.approx_size() > base);
    }

    #[test]
    fn arguments_exotic_mapped_has_mapping() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let mapped: Box<[Option<u32>]> = vec![Some(0), Some(1), None].into_boxed_slice();
        let obj = heap.alloc(JsObject {
            kind: KIND_ARGUMENTS,
            elements: vec![
                JsValue::from_i32_smi(10).unwrap(),
                JsValue::from_i32_smi(20).unwrap(),
                JsValue::from_i32_smi(30).unwrap(),
            ],
            arguments_mapped: Some(mapped),
            ..JsObject::default()
        });
        heap.add_root(JsValue::object(obj));
        let stored = heap.get(obj);
        assert_eq!(stored.kind, KIND_ARGUMENTS);
        assert!(stored.arguments_mapped.is_some());
        let map = stored.arguments_mapped.as_ref().unwrap();
        assert_eq!(map[0], Some(0));
        assert_eq!(map[1], Some(1));
        assert_eq!(map[2], None);
        // Mapped arguments are exotic: indexed access via elements should work
        assert_eq!(stored.elements[0].as_smi(), Some(10));
    }

    #[test]
    fn arguments_exotic_unmapped_is_strict() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let obj = heap.alloc(JsObject {
            kind: KIND_ARGUMENTS,
            elements: vec![JsValue::from_i32_smi(1).unwrap()],
            arguments_mapped: None,
            ..JsObject::default()
        });
        heap.add_root(JsValue::object(obj));
        assert_eq!(heap.get(obj).arguments_mapped, None);
        // Unmapped (strict) arguments should not alias parameters
        assert_eq!(heap.get(obj).elements[0].as_smi(), Some(1));
    }

    #[test]
    fn generator_object_slots() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let h = heap.alloc(JsObject::generator());
        heap.get_mut(h).properties = vec![JsValue::from_f64(2.0), JsValue::from_f64(0.0), JsValue::from_f64(0.0)];
        assert_eq!(heap.get(h).kind, KIND_GENERATOR);
        assert_eq!(heap.get(h).properties.len(), 3);
    }
}
