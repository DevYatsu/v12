//! The collector: `Trace`/`MarkSink` seam, growth-based trigger policy, and
//! the non-moving mark-sweep [`Heap`] — start with NoGC, graduate to
//! mark-sweep; moving/generational/concurrent collectors come much later,
//! if ever.
//!
//! Marking is iterative over an explicit worklist — prototype chains and deep
//! nested structures must never cost native stack. Sweeping is **lazy**: a collection only publishes liveness and counts dead slots; the dead
//! slots themselves are released into per-space free lists in budgeted steps
//! charged against later allocations. Debug builds panic on dead-slot access
//! regardless of sweep progress — liveness is published eagerly at mark end
//! (see crate docs).

use crate::handle::{Handle, HeapSpace, Space};
use crate::object::{IntegrityLevel, JsObject, SizeEstimate, V12BigInt, V12Symbol};
use crate::prop_key::PropKey;
use crate::shape::{Attrs, Descriptor, Shape, ShapeHandle, Transitions, ValidityCellId};
use crate::string::{self, CONCAT_EAGER_FLATTEN_MAX_UNITS, Seed, StrStorage, V12Str};
use crate::value::JsValue;

/// Participation in marking. Implemented by values, handles, heap objects,
/// and standard containers of these; embedders implement it for wrappers that
/// hide handles so custom graphs stay collectable.
///
/// Seam note: this trait plus [`MarkSink`] is the interface a future
/// alternative backend would re-implement behind.
pub trait Trace {
    /// Report every referenced slot to the marker.
    fn trace(&self, sink: &mut MarkSink<'_>);
}

impl<T: Trace> Trace for [T] {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        for item in self {
            item.trace(sink);
        }
    }
}

impl<T: Trace> Trace for Vec<T> {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        self.as_slice().trace(sink);
    }
}

impl<T: Trace> Trace for Option<T> {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        if let Some(item) = self {
            item.trace(sink);
        }
    }
}

impl<T: HeapSpace> Trace for Handle<T> {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        match T::SPACE {
            Space::Objects => sink.mark_object(Handle::new(self.index())),
            Space::Strings => sink.mark_string(Handle::new(self.index())),
            Space::Symbols => sink.mark_symbol(Handle::new(self.index())),
            Space::Bigints => sink.mark_bigint(Handle::new(self.index())),
            Space::Shapes => sink.mark_shape(Handle::new(self.index())),
        }
    }
}

/// Marker front-end handed to [`Trace::trace`]. Deduplication and the worklist
/// live behind it; reporting an already-marked slot is cheap and idempotent.
pub struct MarkSink<'a> {
    collector: &'a mut Collector,
}

impl MarkSink<'_> {
    /// Mark the referenced object as live.
    pub fn mark_object(&mut self, h: Handle<JsObject>) {
        self.collector.mark(Space::Objects, h.index());
    }

    /// Mark the referenced string as live.
    pub fn mark_string(&mut self, h: Handle<V12Str>) {
        self.collector.mark(Space::Strings, h.index());
    }

    /// Mark the referenced symbol as live.
    pub fn mark_symbol(&mut self, h: Handle<V12Symbol>) {
        self.collector.mark(Space::Symbols, h.index());
    }

    /// Mark the referenced BigInt as live.
    pub fn mark_bigint(&mut self, h: Handle<V12BigInt>) {
        self.collector.mark(Space::Bigints, h.index());
    }

    /// Mark the referenced shape as live.
    pub fn mark_shape(&mut self, h: ShapeHandle) {
        self.collector.mark(Space::Shapes, h.index());
    }

    pub(crate) fn mark_slot(&mut self, space: Space, index: u32) {
        self.collector.mark(space, index);
    }
}

/// Mark bits plus the explicit worklist. Iterative by construction.
struct Collector {
    // Indexed by `Space as usize`; array order must match the enum.
    marked: [Vec<bool>; 5],
    work: Vec<(Space, u32)>,
}

impl Collector {
    fn new(slot_counts: [usize; 5]) -> Self {
        Self {
            marked: slot_counts.map(|n| vec![false; n]),
            work: Vec::new(),
        }
    }

    fn mark(&mut self, space: Space, index: u32) {
        let i = index as usize;
        let bits = &mut self.marked[space as usize];
        // Out-of-range handles can only come from forged values; ignore rather
        // than panic inside the collector.
        if i < bits.len() && !bits[i] {
            bits[i] = true;
            self.work.push((space, index));
        }
    }

    fn mark_value(&mut self, v: JsValue) {
        if let Some((space, index)) = v.as_slot() {
            self.mark(space, index);
        }
    }
}

fn trace_referents(heap: &Heap, space: Space, index: u32, sink: &mut MarkSink<'_>) {
    match space {
        Space::Objects => heap.objects[index as usize].trace(sink),
        Space::Strings => heap.strings[index as usize].trace(sink),
        Space::Symbols => heap.symbols[index as usize].trace(sink),
        Space::Bigints => heap.bigints[index as usize].trace(sink),
        Space::Shapes => heap.shapes[index as usize].trace(sink),
    }
}

fn live_bytes_of<T: SizeEstimate>(slots: &[T], marked: &[bool]) -> usize {
    let mut total = 0usize;
    for (slot, is_marked) in slots.iter().zip(marked) {
        if *is_marked {
            total += slot.approx_size();
        }
    }
    total
}

/// Upper bound on slots examined per lazy-sweep step, charged against the
/// allocation that triggered the work ("sweep budget charged against
/// allocations").
const SWEEP_BUDGET_SLOTS: usize = 256;

/// Collection trigger policy, selected at [`Heap::new`].
///
/// * [`GcPolicy::NoGC`] — never collects automatically (bring-up mode);
///   explicit [`Heap::force_collect`] still works.
/// * [`GcPolicy::Growth`] — collect when bytes allocated since the
///   last mark reach the live-bytes estimate after the last mark (2× heap
///   growth), never below `floor_bytes` (default 1 MiB).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcPolicy {
    NoGC,
    Growth {
        /// Lower bound on the trigger threshold, in bytes.
        floor_bytes: usize,
    },
}

impl Default for GcPolicy {
    fn default() -> Self {
        GcPolicy::Growth {
            floor_bytes: DEFAULT_GC_FLOOR_BYTES,
        }
    }
}

const DEFAULT_GC_FLOOR_BYTES: usize = 1 << 20;

/// Registered GC roots: every value here is live at collection start.
#[derive(Clone, Debug, Default)]
pub struct RootSet(pub Vec<JsValue>);

/// Wiring that makes allocation/access generic over the four spaces without
/// unsafe casts or per-type method explosions.
///
/// Sealed: only the four heap-space payload types implement this, so the
/// public [`Heap`] generics cannot be extended from outside the crate.
pub trait SpaceOps: SizeEstimate + Default + sealed::Sealed + Sized {
    const SPACE: Space;
    fn slots(heap: &Heap) -> &Vec<Self>;
    fn slots_mut(heap: &mut Heap) -> &mut Vec<Self>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::JsObject {}
    impl Sealed for crate::string::V12Str {}
    impl Sealed for super::V12Symbol {}
    impl Sealed for super::V12BigInt {}
    impl Sealed for super::Shape {}
}

macro_rules! impl_space_ops {
    ($($ty:ty => $field:ident),* $(,)?) => {$(
        impl SpaceOps for $ty {
            const SPACE: Space = <$ty as HeapSpace>::SPACE;
            fn slots(heap: &Heap) -> &Vec<Self> {
                &heap.$field
            }
            fn slots_mut(heap: &mut Heap) -> &mut Vec<Self> {
                &mut heap.$field
            }
        }
    )*};
}

impl_space_ops!(JsObject => objects, V12Str => strings, V12Symbol => symbols, V12BigInt => bigints, Shape => shapes);

/// The single-mutator heap: five slot-vector spaces with free lists, a root
/// set, and the mark-sweep driver. Intentionally `!Send + !Sync`.
#[derive(Debug)]
pub struct Heap {
    objects: Vec<JsObject>,
    strings: Vec<V12Str>,
    symbols: Vec<V12Symbol>,
    bigints: Vec<V12BigInt>,
    shapes: Vec<Shape>,
    // Indexed by `Space as usize`.
    free: [Vec<u32>; 5],
    alive: [Vec<bool>; 5],
    /// Per-slot "payload released, index sits in the free list" flag.
    /// Invariant: `released[s][i] == free[s].contains(&i)`. Lets the sweeper
    /// skip already-freed slots when a later collection keeps the free list
    /// instead of clearing it.
    released: [Vec<bool>; 5],
    /// Lazy-sweep cursor per space: next slot index to examine.
    sweep_cursor: [usize; 5],
    /// Dead slots found by the last mark but not yet moved to a free list.
    pending_dead: [usize; 5],

    roots: RootSet,

    /// Shapes pinned for their whole lifetime: shape-slot 0's empty-object
    /// root plus any anchors registered via [`Heap::add_shape_root`]. Marked
    /// at every collection start; transition-tree branches hanging off them
    /// survive only while live objects reach them through parent links.
    shape_roots: Vec<ShapeHandle>,
    /// Handles of the pinned empty-object shape (always `shape_roots[0]`).
    root_shape: ShapeHandle,

    /// Serials of validity cells, indexed by `ValidityCellId.0 - 1`; id 0 is
    /// the shared null cell and never appears here. A cell's serial changes
    /// exactly when the assumption it watches (prototype identity, integrity
    /// level) stops holding, so a guard recorded as `(cell, serial)` fails
    /// precisely when its assumption was invalidated. Plain metadata: no
    /// heap handles inside, so collections ignore it entirely.
    validity_cells: Vec<u32>,

    /// Canonical string instances by content hash. Every handle in the
    /// table is a GC root (marked at each collection start), so interned
    /// strings outlive the transient duplicates they deduplicate. Buckets
    /// hold hash-colliding candidates; textual equality decides hits.
    interned_strings: hashbrown::HashMap<u32, Vec<Handle<V12Str>>>,

    policy: GcPolicy,
    allocated_since_gc: usize,
    live_after_gc: usize,
    collections: u64,

    gc_stress_every: Option<u32>,
    stress_ticks: u32,

    _single_thread: core::marker::PhantomData<*mut ()>,
}

impl Default for Heap {
    fn default() -> Self {
        Heap::new(GcPolicy::default())
    }
}

impl Heap {
    /// Creates an empty heap with the given collection policy.
    pub fn new(policy: GcPolicy) -> Self {
        Self {
            objects: Vec::new(),
            strings: Vec::new(),
            symbols: Vec::new(),
            bigints: Vec::new(),
            // Shape-slot 0 is pinned: the canonical empty-object shape every
            // fresh object starts from. Registered as a permanent shape root
            // below, it can never be collected or handed out again.
            shapes: vec![Shape::root()],
            free: [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            // Shape-slot 0 is born live (pinned root); every other space
            // starts with no slots and therefore no flags.
            alive: [Vec::new(), Vec::new(), Vec::new(), Vec::new(), vec![true]],
            released: [Vec::new(), Vec::new(), Vec::new(), Vec::new(), vec![false]],
            sweep_cursor: [0; 5],
            pending_dead: [0; 5],
            roots: RootSet(Vec::new()),
            shape_roots: vec![Handle::new(0)],
            root_shape: Handle::new(0),
            validity_cells: Vec::new(),
            interned_strings: hashbrown::HashMap::new(),
            policy,
            allocated_since_gc: 0,
            live_after_gc: 0,
            collections: 0,
            gc_stress_every: None,
            stress_ticks: 0,
            _single_thread: core::marker::PhantomData,
        }
    }

    /// The pinned empty-object shape at shape-slot 0: parentless,
    /// property-less, permanently rooted. Every fresh object starts here;
    /// all transition trees descend from it (or from other anchored roots).
    pub fn root_shape(&self) -> ShapeHandle {
        self.root_shape
    }

    // ------------------------------------------------------------------
    // Allocation and access
    // ------------------------------------------------------------------

    /// Moves `value` into the space of its type and returns its handle.
    ///
    /// May collect first (policy trigger and/or stress cadence), so obey the
    /// crate-level contract: root the returned handle before the next `alloc`
    /// call. Freed indices return through the space's free list; when that is
    /// empty, the allocator sweeps the space forward in budgeted steps
    /// ([`SWEEP_BUDGET_SLOTS`]) until a dead slot surfaces or the space is
    /// fully swept, re-checking the free list after each step, and only then
    /// extends the slot vector. A slot freed by the last collection is
    /// therefore reused lowest-index-first relative to the sweep cursor.
    pub fn alloc<T: SpaceOps>(&mut self, value: T) -> Handle<T> {
        self.collect_if_needed();
        self.stress_collect_if_due();

        let size = value.approx_size();
        let s = T::SPACE as usize;
        let index: u32 = loop {
            if let Some(i) = self.free[s].pop() {
                self.released[s][i as usize] = false;
                T::slots_mut(self)[i as usize] = value;
                break i;
            }
            // A step may have released dead slots into the free list, so
            // always retry the pop before concluding the space is exhausted.
            if !self.sweep_step::<T>() && self.free[s].is_empty() {
                let slots = T::slots_mut(self);
                slots.push(value);
                let new_index = slots.len() - 1;
                self.alive[s].push(false);
                self.released[s].push(false);
                break new_index as u32;
            }
        };
        self.alive[s][index as usize] = true;

        self.allocated_since_gc += size;
        Handle::new(index)
    }

    /// Object content by handle.
    ///
    /// Debug builds panic when the slot is dead (collected since the handle
    /// was taken); see "Handles and liveness" in the crate docs for release
    /// behavior and the reuse caveat.
    pub fn get<T: SpaceOps>(&self, h: Handle<T>) -> &T {
        let i = h.index() as usize;
        if cfg!(debug_assertions) && !self.alive[T::SPACE as usize][i] {
            panic!(
                "stale {} handle: index {} refers to a dead slot",
                T::SPACE.name(),
                i
            );
        }
        &T::slots(self)[i]
    }

    /// Mutable object content by handle; same liveness rules as [`Heap::get`].
    pub fn get_mut<T: SpaceOps>(&mut self, h: Handle<T>) -> &mut T {
        let i = h.index() as usize;
        if cfg!(debug_assertions) && !self.alive[T::SPACE as usize][i] {
            panic!(
                "stale {} handle: index {} refers to a dead slot",
                T::SPACE.name(),
                i
            );
        }
        &mut T::slots_mut(self)[i]
    }

    // ------------------------------------------------------------------
    // Roots
    // ------------------------------------------------------------------

    /// Current root values.
    pub fn roots(&self) -> &[JsValue] {
        &self.roots.0
    }

    /// Direct access to the root set (push/pop/clear at will).
    pub fn roots_mut(&mut self) -> &mut RootSet {
        &mut self.roots
    }

    /// Registers one root value.
    pub fn add_root(&mut self, value: JsValue) {
        self.roots.0.push(value);
    }

    // ------------------------------------------------------------------
    // Shapes
    // ------------------------------------------------------------------

    /// Pins a shape for its whole lifetime, independent of any object.
    ///
    /// Use this to anchor a transition-tree base that must outlive every
    /// individual object that will ever descend from it. The empty-object
    /// root shape is pinned automatically; speculative trees built before
    /// any object adopts them need explicit anchoring to survive collections
    /// in between.
    pub fn add_shape_root(&mut self, shape: ShapeHandle) {
        self.shape_roots.push(shape);
    }

    /// Extends `parent` by one property, returning the resulting shape.
    ///
    /// If a transition on `key` already exists, its child returns unchanged —
    /// objects that add the same properties in the same order converge on one
    /// shape regardless of how many times the edge is walked. Otherwise a new
    /// child shape is created (parent's descriptors plus this property at
    /// slot `parent.num_own`, prototype cell inherited) and the edge is
    /// recorded.
    ///
    /// Allocation contract: publish the returned handle onto a live object or
    /// anchor it with [`Heap::add_shape_root`] before the next allocation,
    /// or a collection may reclaim it — transition edges are not traced.
    pub fn add_property(&mut self, parent: ShapeHandle, key: PropKey, attrs: Attrs) -> ShapeHandle {
        if let Some(existing) = self.get(parent).transitions.get(key) {
            return existing;
        }
        let (slot, proto_cell, mut descriptors) = {
            let parent_shape = self.get(parent);
            (
                parent_shape.num_own,
                parent_shape.proto_cell,
                parent_shape.descriptors.clone(),
            )
        };
        descriptors.push(Descriptor::Data { key, slot, attrs });
        let child_handle = self.alloc(Shape {
            parent: Some(parent),
            transitions: Transitions::default(),
            descriptors,
            proto_cell,
            num_own: slot + 1,
        });
        self.get_mut(parent).transitions.insert(key, child_handle);
        child_handle
    }

    /// Defines an accessor property (getter/setter) on `parent`.
    ///
    /// Like [`Self::add_property`], but creates an [`Descriptor::Accessor`]
    /// instead of a data descriptor. Accessor descriptors occupy a slot index
    /// for `num_own` stability but store `hole` in the object's `properties`.
    pub fn define_accessor(
        &mut self,
        parent: ShapeHandle,
        key: PropKey,
        getter: Option<crate::Handle<crate::V12Str>>,
        setter: Option<crate::Handle<crate::V12Str>>,
        attrs: Attrs,
    ) -> ShapeHandle {
        if let Some(existing) = self.get(parent).transitions.get(key) {
            return existing;
        }
        let (slot, proto_cell, mut descriptors) = {
            let parent_shape = self.get(parent);
            (
                parent_shape.num_own,
                parent_shape.proto_cell,
                parent_shape.descriptors.clone(),
            )
        };
        descriptors.push(Descriptor::Accessor {
            key,
            getter,
            setter,
            attrs,
        });
        let child_handle = self.alloc(Shape {
            parent: Some(parent),
            transitions: Transitions::default(),
            descriptors,
            proto_cell,
            num_own: slot + 1,
        });
        self.get_mut(parent).transitions.insert(key, child_handle);
        child_handle
    }

    /// Chain-aware descriptor lookup starting at `start`: checks each shape's
    /// own records along the parent links until `key` surfaces. `None` means
    /// no shape on the chain names the key.
    pub fn lookup_property(&self, start: ShapeHandle, key: PropKey) -> Option<&Descriptor> {
        self.get(start).find_descriptor(&self.shapes, key)
    }

    // ------------------------------------------------------------------
    // Validity cells
    // ------------------------------------------------------------------

    /// Allocates a fresh validity cell. Its serial starts at zero and
    /// changes only through [`Heap::bump_validity`]; a guard is the pair
    /// (cell, serial seen when the assumption was recorded) and holds exactly
    /// while [`Heap::guard_holds`] says the recorded serial still matches.
    pub fn new_validity_cell(&mut self) -> ValidityCellId {
        let id = self.validity_cells.len() + 1;
        self.validity_cells.push(0);
        ValidityCellId(id as u32)
    }

    /// The cell watching assumptions about `obj`, creating it on first use.
    /// Lazy because most objects never sit under an inline cache; the id is
    /// cached on the object itself, so repeated queries are stable.
    pub fn validity_cell_of(&mut self, obj: Handle<JsObject>) -> ValidityCellId {
        let existing = self.get(obj).validity_cell;
        if existing != ValidityCellId::NONE {
            return existing;
        }
        let cell = self.new_validity_cell();
        self.get_mut(obj).validity_cell = cell;
        cell
    }

    /// Current serial of `cell`; `None` for the null cell — a guard over
    /// [`ValidityCellId::NONE`] can never hold.
    pub fn validity_serial(&self, cell: ValidityCellId) -> Option<u32> {
        if cell == ValidityCellId::NONE {
            return None;
        }
        self.validity_cells.get((cell.0 - 1) as usize).copied()
    }

    /// True when a guard that recorded `seen` against `cell` still holds.
    pub fn guard_holds(&self, cell: ValidityCellId, seen: u32) -> bool {
        self.validity_serial(cell) == Some(seen)
    }

    /// Invalidates every guard recorded against `cell` before this call by
    /// advancing its serial. Guards compare their recorded serial against
    /// the current one, so any difference — not just an increment — fails
    /// them; the arithmetic wraps rather than panicking so a cell bumped
    /// 2³² times simply starts a fresh era instead of aborting.
    pub fn bump_validity(&mut self, cell: ValidityCellId) {
        if let Some(serial) = self.validity_serial(cell) {
            self.validity_cells[(cell.0 - 1) as usize] = serial.wrapping_add(1);
        }
    }

    /// Raises `obj` to `level` (ES `SetIntegrityLevel`) and records the
    /// transition by bumping the object's validity cell: inline caches that
    /// assumed attribute stability re-verify on their next guarded use.
    /// Flags only ever accumulate ([`IntegrityLevel`] is monotone).
    pub fn set_integrity_level(&mut self, obj: Handle<JsObject>, level: IntegrityLevel) {
        let bits = match level {
            IntegrityLevel::Sealed => JsObject::FLAG_NOT_EXTENSIBLE | JsObject::FLAG_SEALED,
            IntegrityLevel::Frozen => {
                JsObject::FLAG_NOT_EXTENSIBLE | JsObject::FLAG_SEALED | JsObject::FLAG_FROZEN
            }
        };
        self.get_mut(obj).flags |= bits;
        let cell = self.validity_cell_of(obj);
        self.bump_validity(cell);
    }

    // ------------------------------------------------------------------
    // Strings
    //
    // Composite-string machinery (walking, materialization, hashing) lives
    // in the string module; these methods are its heap-facing surface.
    // ------------------------------------------------------------------

    /// Roots `values` as string references across `build`, then drops them.
    ///
    /// Composite nodes carry child handles but are unreachable until their
    /// own allocation returns, so a collection inside that allocation would
    /// otherwise reclaim the children mid-build. Pushes and truncation
    /// balance; the single-mutator design means nothing observes the
    /// transient roots.
    fn with_pinned_string_roots<R>(
        &mut self,
        values: &[Handle<V12Str>],
        build: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let base = self.roots.0.len();
        for &value in values {
            self.add_root(JsValue::string(value));
        }
        let result = build(self);
        self.roots.0.truncate(base);
        result
    }

    /// Concatenates two strings. The result is O(1): a [`StrStorage::Cons`]
    /// over both operands — unless the combined length is at most
    /// [`CONCAT_EAGER_FLATTEN_MAX_UNITS`], where node overhead rivals the
    /// payload and the text is materialized immediately (see the string
    /// module docs for the economics).
    pub fn concat(&mut self, left: Handle<V12Str>, right: Handle<V12Str>) -> Handle<V12Str> {
        let total = self.get(left).len() + self.get(right).len();
        debug_assert!(
            total <= u32::MAX as usize,
            "combined string exceeds u32 length"
        );
        let len = total as u32;

        let handle = self.with_pinned_string_roots(&[left, right], |heap| {
            heap.alloc(V12Str {
                storage: StrStorage::Cons { left, right, len },
                hash: None,
            })
        });
        if total <= CONCAT_EAGER_FLATTEN_MAX_UNITS {
            self.flatten(handle);
        }
        handle
    }

    /// Views `[start_utf16, start_utf16 + len)` of `parent` without copying.
    /// Offsets are UTF-16 code units regardless of the parent's encoding.
    /// Returns `None` when the window leaves the parent's bounds — checked
    /// before any allocation.
    pub fn slice_string(
        &mut self,
        parent: Handle<V12Str>,
        start_utf16: u32,
        len: u32,
    ) -> Option<Handle<V12Str>> {
        let end = start_utf16.checked_add(len)?;
        if end > self.get(parent).len() as u32 {
            return None;
        }
        Some(self.with_pinned_string_roots(&[parent], |heap| {
            heap.alloc(V12Str {
                storage: StrStorage::Sliced {
                    parent,
                    start_utf16,
                    len,
                },
                hash: None,
            })
        }))
    }

    /// Materializes `handle` in place: replaces any Cons/Sliced tree with an
    /// equivalent flat Latin-1 or UTF-16 store, encoding chosen by the
    /// widening rule (Latin-1 only if every leaf is Latin-1). A cached
    /// content hash survives — the text did not change. A no-op on flat
    /// strings, and allocation-free throughout.
    pub fn flatten(&mut self, handle: Handle<V12Str>) {
        if self.get(handle).is_flat() {
            return;
        }
        let flat = string::materialize(self, Seed::Resident(handle));
        let cached = self.get(handle).hash;
        let slot = self.get_mut(handle);
        slot.storage = flat;
        slot.hash = cached;
    }

    /// Content hash of the string, computed once and cached on the node:
    /// FNV-1a over the UTF-16 view, identical for equal texts across
    /// encodings. Subsequent calls return the cached value.
    pub fn string_hash(&mut self, handle: Handle<V12Str>) -> u32 {
        if let Some(hash) = self.get(handle).hash {
            return hash;
        }
        let hash = string::content_hash(self, Seed::Resident(handle));
        self.get_mut(handle).hash = Some(hash);
        hash
    }

    /// Textual equality across any pair of representations: flat pairs
    /// compare directly (widening one side when encodings differ), anything
    /// composite is materialized first. Pure computation — no allocation of
    /// collectable storage.
    pub fn strings_equal(&self, a: Handle<V12Str>, b: Handle<V12Str>) -> bool {
        if a == b {
            return true;
        }
        if self.get(a).len() != self.get(b).len() {
            return false;
        }
        let (sa, sb) = (self.get(a), self.get(b));
        if sa.is_flat() && sb.is_flat() {
            return string::flat_units_equal(&sa.storage, &sb.storage);
        }
        let fa = string::materialize(self, Seed::Resident(a));
        let fb = string::materialize(self, Seed::Resident(b));
        string::flat_units_equal(&fa, &fb)
    }

    /// Deduplicates by content: equal texts map to one canonical instance —
    /// flat, with its hash precomputed. The canonical table roots every
    /// member, so interned strings survive all collections.
    ///
    /// The input may be composite; its tree is read into flat units *before*
    /// the allocation (which may collect), while the caller still holds the
    /// children live per the crate-wide allocation contract.
    pub fn intern_string(&mut self, text: V12Str) -> Handle<V12Str> {
        let flat = string::materialize(self, Seed::Owned(&text));
        let hash = string::hash_flat(&flat);

        if let Some(candidates) = self.interned_strings.get(&hash) {
            for &existing in candidates {
                // Canonical instances are flat, so a normalized flat compare
                // decides exactly.
                if string::flat_units_equal(&flat, &self.get(existing).storage) {
                    return existing;
                }
            }
        }
        let handle = self.alloc(V12Str {
            storage: flat,
            hash: Some(hash),
        });
        self.interned_strings.entry(hash).or_default().push(handle);
        handle
    }

    // ------------------------------------------------------------------
    // Collection
    // ------------------------------------------------------------------

    /// Collects when the policy says so. No-op under [`GcPolicy::NoGC`] and
    /// whenever the allocated-since-last-mark estimate is below the threshold
    /// (see [`GcPolicy::Growth`] for the 2×-growth/floor formula).
    pub fn collect_if_needed(&mut self) {
        if matches!(self.policy, GcPolicy::NoGC) {
            return;
        }
        if self.allocated_since_gc >= self.growth_threshold() {
            self.force_collect();
        }
    }

    /// Forces a full mark-sweep regardless of policy. The `--expose-gc`
    /// analog for tests and embedders.
    ///
    /// The sweep itself is lazy: this marks, publishes liveness (so debug
    /// stale-handle detection is immediate), counts dead slots, and returns;
    /// the dead slots drain into the free lists through [`Heap::sweep_step`]
    /// as later allocations need them.
    pub fn force_collect(&mut self) {
        // Complete the previous cycle's lazy sweep first so marking never
        // races partially-drained free-list bookkeeping.
        self.finish_sweep();

        let counts = [
            self.objects.len(),
            self.strings.len(),
            self.symbols.len(),
            self.bigints.len(),
            self.shapes.len(),
        ];
        let mut collector = Collector::new(counts);

        for &root in &self.roots.0 {
            collector.mark_value(root);
        }
        for &shape_root in &self.shape_roots {
            collector.mark(Space::Shapes, shape_root.index());
        }
        // Canonical string instances are permanent roots: the interning
        // table promises deduplication for the heap's whole lifetime, so a
        // canonical instance can never be reclaimed while the table stands.
        for bucket in self.interned_strings.values() {
            for &handle in bucket {
                collector.mark(Space::Strings, handle.index());
            }
        }

        while let Some((space, index)) = collector.work.pop() {
            let mut sink = MarkSink {
                collector: &mut collector,
            };
            trace_referents(self, space, index, &mut sink);
        }

        // Transition edges are untraced by design, so a live shape's table
        // may still name children nothing else keeps alive. Prune those
        // entries now — before any slot is released — so a cached transition
        // hit can never surface a dead handle.
        self.prune_shape_transitions(&collector.marked[Space::Shapes as usize]);

        let live = live_bytes_of(&self.objects, &collector.marked[Space::Objects as usize])
            + live_bytes_of(&self.strings, &collector.marked[Space::Strings as usize])
            + live_bytes_of(&self.symbols, &collector.marked[Space::Symbols as usize])
            + live_bytes_of(&self.bigints, &collector.marked[Space::Bigints as usize])
            + live_bytes_of(&self.shapes, &collector.marked[Space::Shapes as usize]);

        self.publish_mark::<JsObject>(&collector.marked[Space::Objects as usize]);
        self.publish_mark::<V12Str>(&collector.marked[Space::Strings as usize]);
        self.publish_mark::<V12Symbol>(&collector.marked[Space::Symbols as usize]);
        self.publish_mark::<V12BigInt>(&collector.marked[Space::Bigints as usize]);
        self.publish_mark::<Shape>(&collector.marked[Space::Shapes as usize]);

        self.allocated_since_gc = 0;
        self.live_after_gc = live;
        self.collections += 1;
    }

    /// Removes transition edges of every *live* shape whose target died in
    /// this collection. Runs between mark end and publish; `marked` is the
    /// shape space's mark bitmap.
    fn prune_shape_transitions(&mut self, marked: &[bool]) {
        for (i, shape) in self.shapes.iter_mut().enumerate() {
            if !marked[i] {
                continue; // dead shapes are about to be released wholesale
            }
            shape
                .transitions
                .retain(|_, child| marked[child.index() as usize]);
        }
    }

    /// Publishes one space's mark result: liveness becomes visible
    /// immediately (debug stale-handle detection), the free list survives —
    /// its entries are already-released reusable slots, which a fresh mark
    /// must not re-count or re-default — and only dead slots *not* yet in
    /// the free list are handed to the lazy sweeper.
    fn publish_mark<T: SpaceOps>(&mut self, marked: &[bool]) {
        let s = T::SPACE as usize;
        debug_assert_eq!(marked.len(), T::slots(self).len());
        // A root still naming a previously freed slot resurrects it: evict it
        // from the free list so the allocator can never hand it out twice.
        self.free[s].retain(|&i| !marked[i as usize]);
        for (flag, &m) in self.released[s].iter_mut().zip(marked) {
            if m {
                *flag = false;
            }
        }
        self.alive[s] = marked.to_vec();
        self.pending_dead[s] = marked
            .iter()
            .enumerate()
            .filter(|&(i, &m)| !m && !self.released[s][i])
            .count();
        self.sweep_cursor[s] = 0;
    }

    /// Completes any outstanding lazy sweep across all spaces.
    fn finish_sweep(&mut self) {
        while self.sweep_step::<JsObject>() {}
        while self.sweep_step::<V12Str>() {}
        while self.sweep_step::<V12Symbol>() {}
        while self.sweep_step::<V12BigInt>() {}
        while self.sweep_step::<Shape>() {}
    }

    /// Examines up to [`SWEEP_BUDGET_SLOTS`] slots of this space, releasing
    /// dead ones into the free list. Stops early once a slot is released:
    /// the allocation that charged this step needs only one, and the
    /// remaining dead slots stay pending for later allocations. Slots
    /// already in the free list (`released`) are skipped. Returns `true`
    /// when unswept slots may remain; `false` once the space is fully swept
    /// (the caller may then extend the slot vector).
    fn sweep_step<T: SpaceOps>(&mut self) -> bool {
        let s = T::SPACE as usize;
        let slot_count = T::slots(self).len();
        if self.pending_dead[s] == 0 {
            self.sweep_cursor[s] = slot_count;
            return false;
        }
        let end = slot_count.min(self.sweep_cursor[s] + SWEEP_BUDGET_SLOTS);
        while self.sweep_cursor[s] < end {
            let i = self.sweep_cursor[s];
            self.sweep_cursor[s] += 1;
            if !self.alive[s][i] && !self.released[s][i] {
                // Take-and-replace releases child storage now and guarantees
                // a release-build read of a swept slot sees an empty default.
                T::slots_mut(self)[i] = T::default();
                self.released[s][i] = true;
                self.pending_dead[s] -= 1;
                self.free[s].push(i as u32);
                break;
            }
        }
        self.sweep_cursor[s] < slot_count
    }

    /// Bytes allocated since the last completed mark; the growth trigger's
    /// input.
    pub fn allocated_since_last_gc(&self) -> usize {
        self.allocated_since_gc
    }

    /// Current trigger threshold in bytes (`max(live_after_last_gc, floor)`;
    /// `usize::MAX` under [`GcPolicy::NoGC`]).
    pub fn growth_threshold(&self) -> usize {
        match self.policy {
            GcPolicy::NoGC => usize::MAX,
            GcPolicy::Growth { floor_bytes } => self.live_after_gc.max(floor_bytes),
        }
    }

    /// Completed collection count.
    pub fn collections(&self) -> u64 {
        self.collections
    }

    /// Live-bytes estimate recomputed from current liveness
    /// (approximate; see the crate-private size estimator).
    pub fn live_bytes_estimate(&self) -> usize {
        live_bytes_of(&self.objects, &self.alive[Space::Objects as usize])
            + live_bytes_of(&self.strings, &self.alive[Space::Strings as usize])
            + live_bytes_of(&self.symbols, &self.alive[Space::Symbols as usize])
            + live_bytes_of(&self.bigints, &self.alive[Space::Bigints as usize])
            + live_bytes_of(&self.shapes, &self.alive[Space::Shapes as usize])
    }

    /// Number of occupied (non-freed) slots in a space. Dead slots found by
    /// the last mark count as freed even before the lazy sweeper reaches them.
    pub fn live_count<T: SpaceOps>(&self) -> usize {
        let s = T::SPACE as usize;
        T::slots(self)
            .len()
            .saturating_sub(self.free[s].len())
            .saturating_sub(self.pending_dead[s])
    }

    /// Total slot count (live + freed) in a space; slots are never shrunk.
    pub fn slot_count<T: SpaceOps>(&self) -> usize {
        T::slots(self).len()
    }

    /// The configured policy.
    pub fn policy(&self) -> GcPolicy {
        self.policy
    }

    /// Enables/disables stress mode: collect every `every` allocations
    /// (`Some(1)` collects before every allocation). Overrides the growth
    /// policy while active; values below 1 are clamped to 1.
    pub fn gc_stress(&mut self, every: Option<u32>) {
        self.gc_stress_every = every.map(|n| n.max(1));
        self.stress_ticks = 0;
    }

    /// Current stress cadence, if enabled.
    pub fn gc_stress_cadence(&self) -> Option<u32> {
        self.gc_stress_every
    }

    fn stress_collect_if_due(&mut self) {
        if let Some(every) = self.gc_stress_every {
            self.stress_ticks += 1;
            if self.stress_ticks >= every {
                self.stress_ticks = 0;
                self.force_collect();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KIND_ORDINARY, StrStorage};

    const fn ordinary_with(props: Vec<JsValue>, proto: Option<Handle<JsObject>>) -> JsObject {
        JsObject {
            kind: KIND_ORDINARY,
            flags: 0,
            properties: props,
            elements: Vec::new(),
            prototype: proto,
            validity_cell: ValidityCellId::NONE,
            arguments_mapped: None,
        }
    }

    #[test]
    fn alloc_get_roundtrip_and_mutation() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let h = heap.alloc(JsObject {
            properties: vec![JsValue::from_i32_smi(7).unwrap()],
            ..JsObject::default()
        });
        assert_eq!(Handle::<JsObject>::space(), Space::Objects);
        assert_eq!(heap.get(h).properties[0].as_smi(), Some(7));
        heap.get_mut(h).flags |= 1;
        assert_eq!(heap.get(h).flags, 1);
        assert_eq!(heap.live_count::<JsObject>(), 1);
        assert_eq!(heap.slot_count::<JsObject>(), 1);
    }

    #[test]
    fn cycle_is_reclaimed_and_slot_reused() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let a = heap.alloc(JsObject::default());
        let b = heap.alloc(JsObject::default());
        heap.get_mut(a).prototype = Some(b);
        heap.get_mut(b).prototype = Some(a);
        heap.get_mut(a)
            .properties
            .push(JsValue::from_i32_smi(42).unwrap());

        // Cycle survives while rooted…
        heap.add_root(JsValue::object(a));
        heap.force_collect();
        assert_eq!(heap.live_count::<JsObject>(), 2);
        assert_eq!(heap.get(a).properties[0].as_smi(), Some(42));

        // …and is fully reclaimed once unrooted, despite the cycle.
        heap.roots_mut().0.clear();
        heap.force_collect();
        assert_eq!(heap.live_count::<JsObject>(), 0);

        // Freed slots return through the free list lowest-index-first: the
        // allocator sweeps forward from slot 0, so `a` resurfaces before `b`.
        let fresh = heap.alloc(JsObject::default());
        assert_eq!(fresh.index(), a.index());
        assert!(heap.get(fresh).properties.is_empty());
    }

    #[test]
    fn all_four_spaces_trace_and_sweep() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let s = heap.alloc(V12Str::latin1(b"keep".to_vec()));
        let y = heap.alloc(V12Symbol);
        let b = heap.alloc(V12BigInt {
            sign: false,
            magnitude_le: vec![1, 2, 3],
        });
        let o = heap.alloc(ordinary_with(
            vec![JsValue::string(s), JsValue::symbol(y), JsValue::bigint(b)],
            None,
        ));
        heap.add_root(JsValue::object(o));

        // Garbage in every space dies together.
        let _garbage_str = heap.alloc(V12Str::utf16(vec![0]));
        let _garbage_obj = heap.alloc(JsObject::default());
        heap.force_collect();

        assert_eq!(heap.live_count::<JsObject>(), 1);
        assert_eq!(heap.live_count::<V12Str>(), 1);
        assert_eq!(heap.live_count::<V12Symbol>(), 1);
        assert_eq!(heap.live_count::<V12BigInt>(), 1);
        assert!(matches!(heap.get(s).storage, StrStorage::Latin1(_)));
        assert_eq!(heap.get(b).magnitude_le, vec![1, 2, 3]);
    }

    #[test]
    fn growth_trigger_fires_near_expected_allocation_count() {
        // Floor small enough to trip quickly; default object ≈ 16 bytes.
        let mut heap = Heap::new(GcPolicy::Growth { floor_bytes: 128 });
        assert_eq!(heap.growth_threshold(), 128);

        // One rooted survivor must ride out automatic collections.
        let survivor = heap.alloc(ordinary_with(vec![JsValue::from_i32_smi(1).unwrap()], None));
        heap.add_root(JsValue::object(survivor));

        let mut allocs = 0;
        while heap.collections() == 0 {
            heap.alloc(JsObject::default());
            allocs += 1;
            assert!(allocs < 100_000, "growth trigger never fired");
        }
        // Trigger fired only after the accumulated bytes crossed the floor…
        assert!(allocs >= 2);
        // …reset the counter…
        assert!(heap.allocated_since_last_gc() <= heap.growth_threshold());
        // …and the rooted survivor kept its data.
        assert_eq!(heap.get(survivor).properties[0].as_smi(), Some(1));
        assert_eq!(heap.collections(), 1);

        // Collection ran *before* the allocation that crossed the threshold
        // committed, so that newest unrooted object is still present…
        assert_eq!(heap.live_count::<JsObject>(), 2);
        // …and one more cycle reduces the heap to the survivor alone.
        heap.force_collect();
        assert_eq!(heap.collections(), 2);
        assert_eq!(heap.live_count::<JsObject>(), 1);
    }

    #[test]
    fn no_gc_never_collects_automatically_but_force_works() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        for _ in 0..10_000 {
            heap.alloc(JsObject::default());
            heap.collect_if_needed();
        }
        assert_eq!(heap.collections(), 0);
        assert_eq!(heap.growth_threshold(), usize::MAX);
        heap.force_collect();
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.live_count::<JsObject>(), 0);
    }

    #[test]
    fn gc_stress_one_churn_keeps_live_data() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        heap.gc_stress(Some(1));
        assert_eq!(heap.gc_stress_cadence(), Some(1));
        // Cadence 0 is meaningless and clamps to 1.
        heap.gc_stress(Some(0));
        assert_eq!(heap.gc_stress_cadence(), Some(1));
        heap.gc_stress(Some(1));

        let parent = heap.alloc(JsObject::default());
        heap.add_root(JsValue::object(parent));

        for i in 0..500u32 {
            // Contract: link each new handle into rooted storage BEFORE the
            // next allocation, or stress(1) will have collected it.
            let o = heap.alloc(JsObject::default());
            heap.get_mut(parent).elements.push(JsValue::object(o));
            let s = heap.alloc(V12Str::latin1(vec![b'a' + (i % 26) as u8]));
            heap.get_mut(o).properties.push(JsValue::string(s));
            assert_eq!(heap.get(parent).elements.len(), (i + 1) as usize);
        }

        // Two allocations per iteration ⇒ ~two collections per iteration.
        assert!(heap.collections() >= 500, "stress cadence did not fire");
        assert_eq!(heap.live_count::<JsObject>(), 501);
        assert_eq!(heap.live_count::<V12Str>(), 500);

        // Drop everything; churn leftovers were already gone long ago.
        heap.roots_mut().0.clear();
        heap.force_collect();
        assert_eq!(heap.live_count::<JsObject>(), 0);
        assert_eq!(heap.live_count::<V12Str>(), 0);
    }

    #[test]
    fn lazy_sweep_returns_dead_slots_through_the_free_list() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let a = heap.alloc(JsObject::default());
        let b = heap.alloc(JsObject::default());
        let c = heap.alloc(JsObject::default());
        heap.add_root(JsValue::object(c));

        heap.force_collect();

        // Marking published liveness eagerly, but nothing has been swept yet:
        // `c` counts live, `a`/`b` are dead-but-pending, free list untouched.
        assert_eq!(heap.live_count::<JsObject>(), 1);
        assert!(heap.free[Space::Objects as usize].is_empty());
        assert_eq!(heap.pending_dead[Space::Objects as usize], 2);
        assert!(!heap.alive[Space::Objects as usize][a.index() as usize]);

        // First allocation sweeps just far enough to surface one dead slot.
        let d = heap.alloc(JsObject {
            properties: vec![JsValue::from_i32_smi(1).unwrap()],
            ..JsObject::default()
        });
        assert_eq!(d.index(), a.index()); // lowest dead slot first
        assert_eq!(heap.free[Space::Objects as usize].len(), 0); // popped again
        assert_eq!(heap.pending_dead[Space::Objects as usize], 1);

        let e = heap.alloc(JsObject::default());
        assert_eq!(e.index(), b.index());
        assert_eq!(heap.pending_dead[Space::Objects as usize], 0);

        // A later cycle reclaims the previously rooted object too.
        heap.roots_mut().0.clear();
        heap.force_collect();
        assert_eq!(heap.pending_dead[Space::Objects as usize], 3);
        let f = heap.alloc(JsObject::default());
        assert_eq!(f.index(), a.index()); // sweep restarts from slot 0
    }

    #[test]
    fn pending_sweep_completes_at_next_collection_without_allocations() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        // The pinned root shape is the only thing live at start; capture its
        // byte contribution as the floor for later estimates.
        let pinned = heap.live_bytes_estimate();
        assert!(pinned > 0);
        for _ in 0..8 {
            heap.alloc(JsObject::default());
        }
        heap.force_collect();
        assert_eq!(heap.pending_dead[Space::Objects as usize], 8);
        assert!(heap.free[Space::Objects as usize].is_empty());

        // The next collection first drains the pending sweep into the free
        // list, then marks everything dead again — no allocation in between.
        heap.force_collect();
        assert_eq!(heap.free[Space::Objects as usize].len(), 8);
        assert_eq!(heap.live_count::<JsObject>(), 0);
        assert_eq!(heap.live_bytes_estimate(), pinned);

        // Free-list indices stay unique: allocate all eight back.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            let h = heap.alloc(JsObject::default());
            assert!(seen.insert(h.index()));
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn iterative_marking_survives_deep_prototype_chain() {
        // Native recursion over 100k links would blow the stack; this proves
        // the explicit worklist.
        let count = 100_000u32;
        let mut heap = Heap::new(GcPolicy::NoGC);
        let mut prev: Option<Handle<JsObject>> = None;
        for _ in 0..count {
            let o = heap.alloc(JsObject {
                prototype: prev,
                ..JsObject::default()
            });
            prev = Some(o); // NoGC: safe to hold across the next alloc
        }
        let head = prev.unwrap();
        heap.add_root(JsValue::object(head));
        heap.force_collect();
        assert_eq!(heap.live_count::<JsObject>(), count as usize);

        heap.roots_mut().0.clear();
        heap.force_collect();
        assert_eq!(heap.live_count::<JsObject>(), 0);
    }

    #[test]
    fn forged_out_of_range_handle_in_root_is_ignored_not_fatal() {
        use crate::BOX_MASK;
        let mut heap = Heap::new(GcPolicy::NoGC);
        // Reserved-tag-free but wildly out-of-range object ref.
        let forged = JsValue(BOX_MASK | (1 << 47) | u64::from(u32::MAX));
        heap.add_root(forged);
        heap.force_collect(); // must not panic
        assert_eq!(heap.collections(), 1);
    }

    #[test]
    fn live_bytes_estimate_tracks_liveness() {
        let mut heap = Heap::new(GcPolicy::default());
        // Pinned root shape: the permanent live-bytes floor.
        let pinned = heap.live_bytes_estimate();
        assert!(pinned > 0);
        let h = heap.alloc(JsObject::default());
        heap.add_root(JsValue::object(h));
        let live = heap.live_bytes_estimate();
        assert!(live > pinned);
        heap.roots_mut().0.clear();
        heap.force_collect();
        assert_eq!(heap.live_bytes_estimate(), pinned);
        assert_eq!(heap.collections(), 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stale object handle")]
    fn stale_object_handle_panics_on_get_in_debug() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let h = heap.alloc(JsObject::default());
        heap.force_collect(); // h is unreachable: swept
        let _ = heap.get(h);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "stale string handle")]
    fn stale_string_handle_panics_on_get_mut_in_debug() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let s = heap.alloc(V12Str::latin1(vec![b'x']));
        heap.force_collect();
        let _ = heap.get_mut(s);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn live_handles_do_not_panic_after_collections() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let h = heap.alloc(JsObject::default());
        heap.add_root(JsValue::object(h));
        for _ in 0..10 {
            heap.force_collect();
        }
        assert_eq!(heap.get(h).kind, KIND_ORDINARY); // still alive, no panic
    }

    #[test]
    fn validity_cells_bump_visibly_and_independently() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let a = heap.new_validity_cell();
        let b = heap.new_validity_cell();
        assert_ne!(a, b, "cells must be distinct ids");
        assert_eq!(heap.validity_serial(a), Some(0));
        assert_eq!(heap.validity_serial(b), Some(0));

        // A guard recorded now fails exactly when a bump lands.
        let seen = heap.validity_serial(a).expect("real cell has a serial");
        assert!(heap.guard_holds(a, seen));
        heap.bump_validity(a);
        assert!(!heap.guard_holds(a, seen));
        assert_eq!(heap.validity_serial(a), Some(1));

        // Sibling cells are untouched by a's bump.
        assert_eq!(heap.validity_serial(b), Some(0));
    }

    #[test]
    fn null_cell_has_no_serial_and_ignores_bumps() {
        let heap = Heap::new(GcPolicy::NoGC);
        assert_eq!(heap.validity_serial(ValidityCellId::NONE), None);
        // Guards over the null cell can never hold: no cell, no assumption.
        assert!(!heap.guard_holds(ValidityCellId::NONE, 0));
    }

    #[test]
    fn validity_cell_of_is_lazy_and_stable_per_object() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let o = heap.alloc(JsObject::default());
        assert_eq!(heap.get(o).validity_cell, ValidityCellId::NONE);

        let first = heap.validity_cell_of(o);
        assert_ne!(first, ValidityCellId::NONE);
        assert_eq!(heap.validity_cell_of(o), first, "cell id is cached");

        // Distinct objects get distinct cells; serials start clean.
        let p = heap.alloc(JsObject::default());
        let other = heap.validity_cell_of(p);
        assert_ne!(other, first);
        heap.bump_validity(first);
        assert_eq!(heap.validity_serial(other), Some(0));
    }

    #[test]
    fn integrity_transitions_raise_flags_and_bump_the_cell() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let o = heap.alloc(JsObject::default());

        // Sealing records the transition on the object's own cell…
        let cell = heap.validity_cell_of(o);
        let before = heap.validity_serial(cell).expect("cell exists");
        heap.set_integrity_level(o, IntegrityLevel::Sealed);
        let after_seal = heap.validity_serial(cell).expect("cell exists");
        assert_ne!(after_seal, before, "seal must invalidate guards");
        assert!(heap.get(o).is_sealed() && !heap.get(o).is_frozen());

        // …and freezing bumps again, strictly raising the level.
        heap.set_integrity_level(o, IntegrityLevel::Frozen);
        assert!(heap.get(o).is_frozen() && heap.get(o).is_sealed());
        assert_ne!(
            heap.validity_serial(cell),
            Some(after_seal),
            "freeze must invalidate guards afresh"
        );

        // A guard recorded pre-seal is dead; one re-recorded post-freeze holds.
        assert!(!heap.guard_holds(cell, before));
        let fresh = heap.validity_serial(cell).expect("cell exists");
        assert!(heap.guard_holds(cell, fresh));

        // Unrelated objects are neither flagged nor bumped.
        let q = heap.alloc(JsObject::default());
        assert_eq!(heap.get(q).flags, 0);
        assert_eq!(heap.get(q).validity_cell, ValidityCellId::NONE);
    }
}
