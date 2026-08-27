//! Shapes ([`Shape`]): the shared, immutable description of an object's
//! property layout, organized in a transition tree.
//!
//! ## Design
//!
//! Objects with the same "shape" — same properties added in the same order,
//! same prototype relationship — share one [`Shape`] node instead of each
//! carrying their own layout metadata. Adding a property walks an edge of the
//! **transition tree**: [`Heap::add_property`] returns the parent's existing
//! child for that key when one exists (converging layouts share shapes), and
//! otherwise creates a fresh child whose descriptor list is the parent's list
//! plus one entry. Shapes are never mutated after publication and branches
//! are never pruned by user action — two objects adding different second
//! properties simply fork the tree ("branch freely on divergent adds").
//!
//! ## Reachability and reclamation
//!
//! A shape traces exactly two things: its **parent link** and the **keys
//! named by its descriptors** (property keys reference string/symbol slots).
//! Transition edges are deliberately *not* traced. Retention therefore flows
//! from live objects upward through parent links, which has two properties
//! the engine relies on:
//!
//! * A subtree that no live object reaches is garbage and gets reclaimed even
//!   though older siblings' transition tables still name it — those stale
//!   entries are pruned at mark end (`Heap::prune_shape_transitions`), so a
//!   cached transition hit can never surface a dead handle.
//! * Anchors that must outlive any single object (the pinned empty-object
//!   root shape at shape-slot 0; embedder-pinned speculative trees) are held
//!   strongly by [`Heap::add_shape_root`].
//!
//! The allocation contract from the crate docs applies doubly here: a shape
//! returned by `add_property` must be published onto a live object (or
//! anchored) before the next allocation, or a collection will reclaim it.
//!
//! ## Storage tiers
//!
//! Both per-shape tables start inline and spill once they grow past a small
//! cap, because most real objects have few own properties and few sibling
//! branches:
//!
//! * [`Transitions`] keeps up to eight edges in a stack array scanned
//!   linearly, upgrading to a hash map beyond that.
//! * [`Descriptors`] keeps up to eight entries directly on the shape; larger
//!   layouts move to one exact-size boxed slice out-of-line (shapes are
//!   immutable, so amortized-growth vectors buy nothing past construction).
//!
//! [`Heap::add_property`]: crate::Heap::add_property
//! [`Heap::add_shape_root`]: crate::Heap::add_shape_root
//! [`Heap::prune_shape_transitions`]: crate::Heap::prune_shape_transitions

use crate::gc::{MarkSink, Trace};
use crate::handle::{Handle, HeapSpace, Space};
use crate::prop_key::PropKey;
use crate::string::V12Str;

use std::boxed::Box;
use std::vec::Vec;

/// A typed index into the heap's shape space.
pub type ShapeHandle = Handle<Shape>;

/// Property attributes: writable, enumerable, configurable (ES property
/// flags, one bit each). The default for ordinary assignment turns all three
/// on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Attrs(u8);

impl Attrs {
    pub const WRITABLE: u8 = 0b001;
    pub const ENUMERABLE: u8 = 0b010;
    pub const CONFIGURABLE: u8 = 0b100;

    /// All three attributes set: what plain assignment produces.
    pub const DEFAULT: Attrs = Attrs(Self::WRITABLE | Self::ENUMERABLE | Self::CONFIGURABLE);

    /// From explicit flags.
    pub const fn new(writable: bool, enumerable: bool, configurable: bool) -> Attrs {
        let mut bits = 0;
        if writable {
            bits |= Self::WRITABLE;
        }
        if enumerable {
            bits |= Self::ENUMERABLE;
        }
        if configurable {
            bits |= Self::CONFIGURABLE;
        }
        Attrs(bits)
    }

    pub const fn writable(self) -> bool {
        self.0 & Self::WRITABLE != 0
    }

    pub const fn enumerable(self) -> bool {
        self.0 & Self::ENUMERABLE != 0
    }

    pub const fn configurable(self) -> bool {
        self.0 & Self::CONFIGURABLE != 0
    }

    /// Raw flag bits.
    pub const fn bits(self) -> u8 {
        self.0
    }
}

/// One own-property record: which key, which value slot, which attributes.
///
/// Slot numbers are dense per shape: a shape with `num_own == n` addresses
/// slots `0..n`, and extending a shape assigns slot `num_own`. Where those
/// slots physically live (in-object vs overflow storage) is the object's
/// decision, not the shape's.
///
/// `Descriptor` has two forms:
///
/// * `Data` — an ordinary data property with a value slot and attributes.
/// * `Accessor` — a getter/setter pair, each an optional heap-string handle
///   whose text is the JS source of the accessor function body for `v1`.
///   Accessor descriptors occupy a slot index in `num_own` for layout stability
///   but hold `hole` in the object's `properties` storage; the getter/setter
///   handles are traced via the shape.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Descriptor {
    Data {
        key: PropKey,
        slot: u32,
        attrs: Attrs,
    },
    Accessor {
        key: PropKey,
        getter: Option<Handle<V12Str>>,
        setter: Option<Handle<V12Str>>,
        attrs: Attrs,
    },
}

impl Descriptor {
    /// Property key.
    #[must_use]
    pub fn key(self) -> PropKey {
        match self {
            Self::Data { key, .. } | Self::Accessor { key, .. } => key,
        }
    }

    /// Attributes.
    #[must_use]
    pub fn attrs(self) -> Attrs {
        match self {
            Self::Data { attrs, .. } | Self::Accessor { attrs, .. } => attrs,
        }
    }

    /// Slot index for data descriptors; `None` for accessors.
    #[must_use]
    pub fn slot(self) -> Option<u32> {
        match self {
            Self::Data { slot, .. } => Some(slot),
            Self::Accessor { .. } => None,
        }
    }

    /// `true` for data descriptors.
    #[must_use]
    pub fn is_data(self) -> bool {
        matches!(self, Self::Data { .. })
    }

    /// `true` for accessor descriptors.
    #[must_use]
    pub fn is_accessor(self) -> bool {
        matches!(self, Self::Accessor { .. })
    }

    /// Getter handle for accessors.
    #[must_use]
    pub fn getter(self) -> Option<Handle<V12Str>> {
        match self {
            Self::Accessor { getter, .. } => getter,
            Self::Data { .. } => None,
        }
    }

    /// Setter handle for accessors.
    #[must_use]
    pub fn setter(self) -> Option<Handle<V12Str>> {
        match self {
            Self::Accessor { setter, .. } => setter,
            Self::Data { .. } => None,
        }
    }
}

/// Maximum number of transition edges kept inline on a shape.
pub const TRANSITIONS_INLINE_CAP: usize = 8;

/// Child shapes reachable by adding one property. Inline stack array below
/// the cap, hash map above (see module docs for why the split exists).
#[derive(Clone, Debug)]
pub enum Transitions {
    Inline {
        entries: [Option<(PropKey, ShapeHandle)>; TRANSITIONS_INLINE_CAP],
        len: u8,
    },
    Map(Box<hashbrown::HashMap<PropKey, ShapeHandle>>),
}

impl Default for Transitions {
    fn default() -> Self {
        Transitions::Inline {
            entries: [None; TRANSITIONS_INLINE_CAP],
            len: 0,
        }
    }
}

impl Transitions {
    /// Number of recorded edges.
    pub fn len(&self) -> usize {
        match self {
            Transitions::Inline { len, .. } => *len as usize,
            Transitions::Map(map) => map.len(),
        }
    }

    /// True when no edges are recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Child for `key`, if a transition on that key exists.
    pub fn get(&self, key: PropKey) -> Option<ShapeHandle> {
        match self {
            Transitions::Inline { entries, .. } => entries
                .iter()
                .flatten()
                .find(|(k, _)| *k == key)
                .map(|&(_, h)| h),
            Transitions::Map(map) => map.get(&key).copied(),
        }
    }

    /// Records `key -> child`, replacing any previous edge on `key`. Upgrades
    /// the inline array to a hash map when the cap is exceeded; the upgrade
    /// is one-way (shapes are immutable once published, so nothing ever
    /// shrinks back).
    pub fn insert(&mut self, key: PropKey, child: ShapeHandle) {
        if self.get(key).is_some() {
            // Replacement never changes the count, so neither tier needs its
            // append path.
            if let Transitions::Inline { entries, .. } = self {
                for (k, h) in entries.iter_mut().flatten() {
                    if *k == key {
                        *h = child;
                        return;
                    }
                }
            }
            if let Transitions::Map(map) = self {
                map.insert(key, child);
            }
            return;
        }
        match self {
            Transitions::Inline { entries, len } => {
                if (*len as usize) < TRANSITIONS_INLINE_CAP {
                    entries[*len as usize] = Some((key, child));
                    *len += 1;
                    return;
                }
                let mut map = hashbrown::HashMap::with_capacity(TRANSITIONS_INLINE_CAP + 1);
                for (k, h) in entries.iter_mut().flatten() {
                    map.insert(*k, *h);
                }
                map.insert(key, child);
                *self = Transitions::Map(Box::new(map));
            }
            Transitions::Map(map) => {
                map.insert(key, child);
            }
        }
    }

    /// Drops every edge whose target does not satisfy `keep`. Used by the
    /// collector to prune entries naming reclaimed child shapes.
    pub fn retain(&mut self, keep: impl Fn(PropKey, ShapeHandle) -> bool) {
        match self {
            Transitions::Inline { entries, len } => {
                let n = *len as usize;
                let mut kept = 0usize;
                // Index loop rather than iter_mut: compaction writes back
                // into the same array being read.
                for i in 0..n {
                    if let Some((k, h)) = entries[i]
                        && keep(k, h)
                    {
                        entries[kept] = Some((k, h));
                        kept += 1;
                    }
                }
                for entry in entries.iter_mut().skip(kept) {
                    *entry = None;
                }
                *len = kept as u8;
            }
            Transitions::Map(map) => {
                map.retain(|k, h| keep(*k, *h));
            }
        }
    }

    /// Iterates `(key, child)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (PropKey, ShapeHandle)> + '_ {
        match self {
            Transitions::Inline { entries, len } => {
                let items: Vec<(PropKey, ShapeHandle)> = entries
                    .iter()
                    .take(*len as usize)
                    .flatten()
                    .copied()
                    .collect();
                Box::new(items.into_iter()) as Box<dyn Iterator<Item = _>>
            }
            Transitions::Map(map) => {
                Box::new(map.iter().map(|(k, h)| (*k, *h))) as Box<dyn Iterator<Item = _>>
            }
        }
    }

    /// Rough heap bytes retained beyond the struct itself (size accounting
    /// for the GC growth trigger).
    pub(crate) fn retained_bytes(&self) -> usize {
        const ENTRY: usize = core::mem::size_of::<(PropKey, ShapeHandle)>();
        match self {
            Transitions::Inline { .. } => TRANSITIONS_INLINE_CAP * (ENTRY + 1),
            Transitions::Map(map) => map.capacity() * (ENTRY + 1),
        }
    }
}

/// Maximum number of descriptors kept directly on a shape.
pub const DESCRIPTORS_INLINE_CAP: usize = 8;

/// Own-property records of one shape. Inline vector up to the cap, then a
/// single exact-size boxed slice (module docs explain the tiering).
#[derive(Clone, Debug)]
pub enum Descriptors {
    Inline(Vec<Descriptor>),
    OutOfLine(Box<[Descriptor]>),
}

impl Default for Descriptors {
    fn default() -> Self {
        Descriptors::Inline(Vec::new())
    }
}

impl Descriptors {
    /// Appends a descriptor, spilling out-of-line past the cap. Spill copies
    /// the whole layout once into an exact-size boxed slice; further growth
    /// re-copies (shapes are built incrementally but read for their entire
    /// lifetime, so construction cost is amortized away).
    pub fn push(&mut self, descriptor: Descriptor) {
        match self {
            Descriptors::Inline(vec) => {
                if vec.len() < DESCRIPTORS_INLINE_CAP {
                    vec.push(descriptor);
                    return;
                }
                let mut flat = Vec::with_capacity(DESCRIPTORS_INLINE_CAP + 1);
                flat.append(vec);
                flat.push(descriptor);
                *self = Descriptors::OutOfLine(flat.into_boxed_slice());
            }
            Descriptors::OutOfLine(slice) => {
                let mut flat = Vec::with_capacity(slice.len() + 1);
                flat.extend_from_slice(slice);
                flat.push(descriptor);
                *self = Descriptors::OutOfLine(flat.into_boxed_slice());
            }
        }
    }

    /// Number of own properties described.
    pub fn len(&self) -> usize {
        match self {
            Descriptors::Inline(vec) => vec.len(),
            Descriptors::OutOfLine(slice) => slice.len(),
        }
    }

    /// True when the shape describes no own properties.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The descriptor for `key` within this shape alone (no parent walk; see
    /// [`Shape::find_descriptor`] for chain-aware lookup).
    pub fn find(&self, key: PropKey) -> Option<&Descriptor> {
        self.as_slice().iter().find(|d| d.key() == key)
    }

    /// All descriptors, cheapest slice view.
    pub fn as_slice(&self) -> &[Descriptor] {
        match self {
            Descriptors::Inline(vec) => vec,
            Descriptors::OutOfLine(slice) => slice,
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let unit = core::mem::size_of::<Descriptor>();
        match self {
            Descriptors::Inline(vec) => vec.capacity() * unit,
            Descriptors::OutOfLine(slice) => slice.len() * unit,
        }
    }
}

/// Handle to a validity cell: a version stamp watching one prototype
/// relationship. `NONE` marks "no guarded assumption". The registry that
/// assigns ids and serials lives on the heap (`Heap::new_validity_cell`,
/// `Heap::bump_validity`); a guard is simply the pair (cell, serial seen when
/// the assumption was recorded).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ValidityCellId(pub u32);

impl ValidityCellId {
    /// The null cell: never registered, never bumped, always fails a guard
    /// check — assumptions with no cell are assumptions worth nothing.
    pub const NONE: ValidityCellId = ValidityCellId(0);

    /// The registry index this id denotes (the id *is* the index).
    pub fn cell_index(self) -> usize {
        self.0 as usize
    }
}

/// The null cell, so objects and shapes can derive `Default` with "no guard
/// recorded" semantics.
impl Default for ValidityCellId {
    fn default() -> Self {
        Self::NONE
    }
}

/// A shape node. Immutable once published; extension goes through
/// [`Heap::add_property`], which either returns a cached transition child or
/// creates one.
///
/// Field notes:
///
/// * `parent` — the shape this one extends by exactly one property. Traced.
/// * `transitions` — edges to children, keyed by the added property's key.
///   Deliberately *not* traced (module docs explain why).
/// * `descriptors` — own properties of THIS shape only; ancestors' records
///   live on the ancestors.
/// * `proto_cell` — validity cell watching the prototype chain objects with
///   this shape rely on; inherited down each branch.
/// * `num_own` — total own-property count including all ancestors; also the
///   slot number assigned to the next added property.
///
/// [`Heap::add_property`]: crate::Heap::add_property
#[derive(Clone, Debug)]
pub struct Shape {
    pub parent: Option<ShapeHandle>,
    pub transitions: Transitions,
    pub descriptors: Descriptors,
    pub proto_cell: ValidityCellId,
    pub num_own: u32,
}

impl Default for Shape {
    fn default() -> Self {
        Shape {
            parent: None,
            transitions: Transitions::default(),
            descriptors: Descriptors::default(),
            proto_cell: ValidityCellId::NONE,
            num_own: 0,
        }
    }
}

impl Shape {
    /// The empty-object shape: no parent, no properties, no transitions.
    /// The heap pins one instance at shape-slot 0 ([`crate::Heap::root_shape`])
    /// and every fresh object starts there.
    pub fn root() -> Shape {
        Shape::default()
    }

    /// Chain-aware property lookup: scans this shape's descriptors, then
    /// walks parent links until found or the root is passed. Iterative by
    /// construction — transition chains get as deep as object literals are
    /// long, and native recursion over them would cost stack.
    pub fn find_descriptor<'a>(
        &'a self,
        store: &'a [Shape],
        key: PropKey,
    ) -> Option<&'a Descriptor> {
        let mut current = self;
        loop {
            if let Some(descriptor) = current.descriptors.find(key) {
                return Some(descriptor);
            }
            let parent = current.parent?;
            current = store.get(parent.index() as usize)?;
        }
    }
}

impl HeapSpace for Shape {
    const SPACE: Space = Space::Shapes;
}

// Tracing: keep the parent alive and keep descriptor keys' string/symbol
// slots alive plus accessor getter/setter string handles.
// Transition targets are intentionally skipped — retention must
// be anchored by live objects or explicit roots so unreachable branches stay
// collectable (see module docs).
impl Trace for Shape {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        self.parent.trace(sink);
        for descriptor in self.descriptors.as_slice() {
            match descriptor.key().parts() {
                (false, index) => sink.mark_string(Handle::new(index)),
                (true, index) => sink.mark_symbol(Handle::new(index)),
            }
            if let Descriptor::Accessor { getter, setter, .. } = descriptor {
                if let Some(g) = getter {
                    sink.mark_string(*g);
                }
                if let Some(s) = setter {
                    sink.mark_string(*s);
                }
            }
        }
    }
}

impl crate::object::SizeEstimate for Shape {
    fn approx_size(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.transitions.retained_bytes()
            + self.descriptors.retained_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GcPolicy, Heap, V12Str};

    /// Allocates a string and roots it immediately (allocation contract),
    /// returning it as a property key.
    fn keyed(heap: &mut Heap, name: &[u8]) -> PropKey {
        let h = heap.alloc(V12Str::latin1(name.to_vec()));
        heap.add_root(crate::JsValue::string(h));
        PropKey::from_string(h)
    }

    #[test]
    fn chain_adds_build_descriptors_and_slots() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let base = heap.root_shape();
        assert_eq!(heap.get(base).num_own, 0);
        assert!(heap.get(base).descriptors.is_empty());

        let ka = keyed(&mut heap, b"a");
        let kb = keyed(&mut heap, b"b");
        let sa = heap.add_property(base, ka, Attrs::DEFAULT);
        let sab = heap.add_property(sa, kb, Attrs::new(false, true, true));

        // Each extension adds exactly one descriptor with a dense slot.
        assert_eq!(heap.get(sa).num_own, 1);
        assert_eq!(heap.get(sab).num_own, 2);
        let da = heap.get(sa).descriptors.find(ka).copied();
        assert_eq!(
            da,
            Some(Descriptor::Data {
                key: ka,
                slot: 0,
                attrs: Attrs::DEFAULT
            })
        );
        let db = heap.get(sab).descriptors.find(kb).copied();
        assert_eq!(
            db,
            Some(Descriptor::Data {
                key: kb,
                slot: 1,
                attrs: Attrs::new(false, true, true)
            })
        );

        // Parent links thread the chain; attributes ride on descriptors.
        assert_eq!(heap.get(sab).parent, Some(sa));
        assert_eq!(heap.get(sa).parent, Some(base));
        assert!(!db.unwrap().attrs().writable());
    }

    #[test]
    fn lookup_walks_parent_chain() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let kx = keyed(&mut heap, b"x");
        let ky = keyed(&mut heap, b"y");
        let missing = keyed(&mut heap, b"nope");

        let s0 = heap.root_shape();
        let sx = heap.add_property(s0, kx, Attrs::DEFAULT);
        let sxy = heap.add_property(sx, ky, Attrs::DEFAULT);

        // Own key resolves locally…
        let d = heap.lookup_property(sxy, ky).expect("own key must resolve");
        assert_eq!((d.key(), d.slot()), (ky, Some(1)));
        // …ancestor keys resolve through parent links…
        let d = heap
            .lookup_property(sxy, kx)
            .expect("inherited key must resolve");
        assert_eq!((d.key(), d.slot()), (kx, Some(0)));
        // …and unknown keys resolve nowhere.
        assert_eq!(heap.lookup_property(sxy, missing), None);

        // Same lookups work from mid-chain shapes.
        assert!(heap.lookup_property(sx, ky).is_none());
        assert_eq!(heap.lookup_property(sx, kx).and_then(|d| d.slot()), Some(0));
    }

    #[test]
    fn divergence_creates_branch_not_mutation() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let kx = keyed(&mut heap, b"x");
        let ky = keyed(&mut heap, b"y");
        let kz = keyed(&mut heap, b"z");

        let s0 = heap.root_shape();
        let sx = heap.add_property(s0, kx, Attrs::DEFAULT); // branch 1
        let sxy = heap.add_property(sx, ky, Attrs::DEFAULT);
        let sz = heap.add_property(s0, kz, Attrs::DEFAULT); // branch 2 diverges

        // The base gained a second transition edge but its own layout is
        // untouched: one property (x) at slot 0, nothing more.
        assert_eq!(heap.get(s0).descriptors.len(), 0);
        assert_eq!(heap.get(s0).num_own, 0);
        assert_eq!(heap.get(s0).transitions.len(), 2);

        // Branch shapes are distinct nodes with their own descriptor sets.
        assert_ne!(sx, sz);
        assert_eq!(heap.get(sz).descriptors.as_slice().len(), 1);
        assert_eq!(
            heap.get(sz).descriptors.find(kz).and_then(|d| d.slot()),
            Some(0)
        );
        assert_eq!(heap.get(sxy).descriptors.len(), 2);

        // Walking the same edges converges back onto the existing shapes —
        // no new nodes for repeated adds along an established path.
        assert_eq!(heap.add_property(s0, kx, Attrs::DEFAULT), sx);
        assert_eq!(heap.add_property(sx, ky, Attrs::DEFAULT), sxy);
        assert_eq!(heap.get(s0).transitions.len(), 2);
    }

    #[test]
    fn transitions_upgrade_to_map_past_inline_cap() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let s0 = heap.root_shape();
        let keys: Vec<PropKey> = (0..(TRANSITIONS_INLINE_CAP as u8 + 3))
            .map(|i| PropKey::from_parts(false, i as u32 * 7 + 100))
            .collect();

        let mut children = Vec::new();
        for &k in &keys {
            children.push(heap.add_property(s0, k, Attrs::DEFAULT));
        }
        // Past eight edges the table spilled into a hash map…
        assert!(matches!(heap.get(s0).transitions, Transitions::Map(_)));
        // …with every edge still resolvable, including the early ones that
        // migrated during the upgrade.
        for (&k, &child) in keys.iter().zip(&children) {
            assert_eq!(heap.get(s0).transitions.get(k), Some(child));
        }
        assert_eq!(heap.get(s0).transitions.len(), TRANSITIONS_INLINE_CAP + 3);
    }

    #[test]
    fn descriptors_spill_out_of_line_past_cap() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let mut shape = heap.root_shape();
        let keys: Vec<PropKey> = (0..(DESCRIPTORS_INLINE_CAP as u32 + 4))
            .map(|i| PropKey::from_parts(false, i * 13))
            .collect();

        for &k in &keys[..DESCRIPTORS_INLINE_CAP] {
            shape = heap.add_property(shape, k, Attrs::DEFAULT);
            assert!(matches!(
                heap.get(shape).descriptors,
                Descriptors::Inline(_)
            ));
        }
        // The ninth property spills the frozen layout out-of-line…
        shape = heap.add_property(shape, keys[DESCRIPTORS_INLINE_CAP], Attrs::DEFAULT);
        assert!(matches!(
            heap.get(shape).descriptors,
            Descriptors::OutOfLine(_)
        ));

        // …and further adds keep working off the boxed slice.
        for &k in &keys[DESCRIPTORS_INLINE_CAP + 1..] {
            shape = heap.add_property(shape, k, Attrs::DEFAULT);
            assert!(matches!(
                heap.get(shape).descriptors,
                Descriptors::OutOfLine(_)
            ));
        }
        assert_eq!(heap.get(shape).num_own, DESCRIPTORS_INLINE_CAP as u32 + 4);
        for (slot, &k) in keys.iter().enumerate() {
            assert_eq!(
                heap.get(shape).descriptors.find(k).and_then(|d| d.slot()),
                Some(slot as u32)
            );
            assert_eq!(
                heap.lookup_property(shape, k).and_then(|d| d.slot()),
                Some(slot as u32)
            );
        }
    }

    #[test]
    fn gc_stress_reclaims_dead_subtree_keeps_live_branch() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        heap.gc_stress(Some(1));

        let kx = keyed(&mut heap, b"x");
        let ky = keyed(&mut heap, b"y");
        let kz = keyed(&mut heap, b"z");

        // Live branch: x -> y. Each new shape is anchored before the next
        // allocation (transition edges are untraced, so unanchored shapes do
        // not survive collections).
        let base = heap.root_shape(); // pinned at slot 0 by the heap itself
        let sx = heap.add_property(base, kx, Attrs::DEFAULT);
        heap.add_shape_root(sx);
        let sxy = heap.add_property(sx, ky, Attrs::DEFAULT);
        heap.add_shape_root(sxy);

        // Dead branch: z. Linked from the base's transition table only.
        let sz = heap.add_property(base, kz, Attrs::DEFAULT);
        assert_eq!(heap.get(base).transitions.get(kz), Some(sz));

        // Stress cadence has been collecting throughout; one explicit cycle
        // settles the final state.
        heap.force_collect();

        // Only the anchored chain survives; the dead branch was reclaimed…
        assert_eq!(
            heap.live_count::<Shape>(),
            3,
            "expected root + two live-branch shapes"
        );
        // …and its stale transition entry was pruned from the live base.
        assert_eq!(heap.get(base).transitions.get(kz), None);
        assert_eq!(heap.get(base).transitions.get(kx), Some(sx));
        assert_eq!(heap.get(sx).transitions.get(ky), Some(sxy));

        // Live branch data is intact after all the churn.
        assert_eq!(
            heap.lookup_property(sxy, kx).and_then(|d| d.slot()),
            Some(0)
        );
        assert_eq!(
            heap.lookup_property(sxy, ky).and_then(|d| d.slot()),
            Some(1)
        );

        // A fresh add on the same key creates a NEW branch (the old edge is
        // gone). The reclaimed slot may legitimately be reused for it; what
        // matters is that the edge names a live shape with fresh contents.
        let sz2 = heap.add_property(base, kz, Attrs::DEFAULT);
        heap.add_shape_root(sz2);
        assert_eq!(heap.get(base).transitions.get(kz), Some(sz2));
        assert_eq!(heap.get(sz2).num_own, 1);
        assert_eq!(heap.get(sz2).parent, Some(base));
    }

    #[test]
    fn accessor_descriptor_is_distinct_from_data() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let k = keyed(&mut heap, b"acc");
        let getter = heap.intern_string(V12Str::latin1(b"42".to_vec()));
        heap.add_root(crate::JsValue::string(getter));
        let base = heap.root_shape();
        let s_acc = heap.define_accessor(base, k, Some(getter), None, Attrs::DEFAULT);
        heap.add_shape_root(s_acc);
        let desc = heap
            .lookup_property(s_acc, k)
            .expect("accessor must be found");
        assert!(desc.is_accessor());
        assert!(!desc.is_data());
        assert_eq!(desc.getter(), Some(getter));
        assert_eq!(desc.setter(), None);
        assert!(desc.slot().is_none());
        // Data descriptor for a different key remains distinct
        let k2 = keyed(&mut heap, b"acc2");
        let s_data = heap.add_property(base, k2, Attrs::DEFAULT);
        let desc_data = heap.lookup_property(s_data, k2).unwrap();
        assert!(desc_data.is_data());
    }

    #[test]
    fn accessor_getter_and_setter_roundtrip() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let k = keyed(&mut heap, b"x");
        let getter = heap.intern_string(V12Str::latin1(b"123".to_vec()));
        let setter = heap.intern_string(V12Str::latin1(b"setter_body".to_vec()));
        heap.add_root(crate::JsValue::string(getter));
        heap.add_root(crate::JsValue::string(setter));
        let base = heap.root_shape();
        let s = heap.define_accessor(base, k, Some(getter), Some(setter), Attrs::DEFAULT);
        heap.add_shape_root(s);
        let d = heap.lookup_property(s, k).unwrap();
        assert_eq!(d.getter(), Some(getter));
        assert_eq!(d.setter(), Some(setter));
        // Ensure GC keeps getter/setter strings alive via shape trace
        heap.force_collect();
        assert_eq!(heap.lookup_property(s, k).unwrap().getter(), Some(getter));
    }
}
