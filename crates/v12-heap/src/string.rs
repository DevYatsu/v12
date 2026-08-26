//! Heap strings ([`V12Str`]): flat Latin-1/UTF-16 leaves plus two lazy
//! composite forms, a once-computed content hash, and canonical-instance
//! interning driven by [`Heap::intern_string`].
//!
//! ## Representation
//!
//! * Flat leaves own their code units: [`StrStorage::Latin1`] bytes or
//!   [`StrStorage::Utf16`] units.
//! * [`StrStorage::Cons`] pairs two string handles with the combined length;
//!   concatenation is O(1) and materialization is deferred until someone
//!   reads the text ([`Heap::flatten`]) or the result is small enough that
//!   deferral buys nothing ([`CONCAT_EAGER_FLATTEN_MAX_UNITS`]).
//! * [`StrStorage::Sliced`] views a window of a parent string by UTF-16
//!   offset; slicing copies nothing and bounds-checks once.
//!
//! ## Encoding policy (the mixing rule)
//!
//! Indices are UTF-16 code-unit offsets everywhere — JavaScript string
//! semantics — regardless of how a leaf happens to be stored. When a
//! composite materializes, the result is Latin-1 **only if every leaf it
//! reaches is Latin-1**; any UTF-16 leaf anywhere in the tree widens the
//! whole output to UTF-16. Latin-1 is a strict subset of UTF-16, so widening
//! is lossless, the decision is a single flag accumulated during one walk,
//! and no per-node re-analysis is ever needed. The alternative — per-region
//! encodings inside one string — would triple every consumer's match arms
//! for memory savings only pathological workloads would notice.
//!
//! Small concatenations are exempt from laziness: below
//! [`CONCAT_EAGER_FLATTEN_MAX_UNITS`] the Cons node (two handles plus a
//! length) rivals the payload itself and every later read would pay a tree
//! walk, so `concat` materializes immediately.
//!
//! ## Hashing
//!
//! Content hashes are FNV-1a over the UTF-16 view regardless of leaf
//! encoding, so equal texts hash equally across encodings — the property
//! interning depends on. The hash is computed on first request and cached
//! on the node itself (dying with the slot, so no side table can go
//! stale). All walks run on an explicit task stack: arbitrarily deep cons
//! chains cost no native stack, mirroring the collector's own discipline.
//!
//! ## Interning
//!
//! [`Heap::intern_string`] maps equal texts onto one canonical flat
//! instance with a precomputed hash. The canonical table roots its members
//! strongly, so interned strings survive collections that reclaim transient
//! duplicates.
//!
//! [`Heap::intern_string`]: crate::Heap::intern_string
//! [`Heap::flatten`]: crate::Heap::flatten

use crate::gc::{Heap, MarkSink, Trace};
use crate::handle::{Handle, HeapSpace, Space};
use crate::object::SizeEstimate;

use std::vec::Vec;

/// Concatenations producing at most this many UTF-16 units are materialized
/// immediately rather than kept as ropes. Below the bound the Cons node
/// (two handles plus a length) rivals the payload itself, and small strings
/// dominate real workloads — paying one copy at build time beats charging
/// every later reader a tree walk. Above it, deferral wins: repeated
/// appends stay O(1) and the rope flattens once, on demand.
pub const CONCAT_EAGER_FLATTEN_MAX_UNITS: usize = 128;

/// FNV-1a 32-bit offset basis: standard Fowler–Noll–Vo parameter, giving
/// good dispersion on the short ASCII-heavy texts that dominate property
/// names and source snippets. Pure arithmetic — no addresses, no seeds —
/// so a given text hashes identically in every process.
const FNV_OFFSET_BASIS_32: u32 = 0x811C_9DC5;

/// FNV-1a 32-bit prime (2²⁴ + 2⁸ + 0x93), the multiplier paired with
/// [`FNV_OFFSET_BASIS_32`] in the standard construction.
const FNV_PRIME_32: u32 = 0x0100_0193;

/// String backing: a flat leaf or one of the lazy composite views. See the
/// module docs for the representation and encoding policies.
///
/// Equality ([`PartialEq`]) compares the representation itself — a Latin-1
/// leaf and a UTF-16 leaf holding the same text compare unequal, matching
/// the reference-identity convention used by property keys. Textual equality
/// across representations is [`Heap::strings_equal`]'s job.
///
/// [`Heap::strings_equal`]: crate::Heap::strings_equal
#[derive(Clone, Debug)]
pub enum StrStorage {
    /// 8-bit Latin-1 code units.
    Latin1(Vec<u8>),
    /// 16-bit UTF-16 code units.
    Utf16(Vec<u16>),
    /// `left ++ right`, with the combined length stored so `len` stays O(1).
    /// Both children traced; the tree flattens on demand.
    Cons {
        left: Handle<V12Str>,
        right: Handle<V12Str>,
        /// Total UTF-16 length of both sides combined.
        len: u32,
    },
    /// A `[start_utf16, start_utf16 + len)` window of `parent`, zero-copy.
    /// Offsets are UTF-16 code units (JS index space) even when the parent
    /// stores Latin-1, where unit and byte indices coincide. Traced.
    Sliced {
        parent: Handle<V12Str>,
        start_utf16: u32,
        len: u32,
    },
}

impl Default for StrStorage {
    fn default() -> Self {
        StrStorage::Latin1(Vec::new())
    }
}

impl PartialEq for StrStorage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (StrStorage::Latin1(a), StrStorage::Latin1(b)) => a == b,
            (StrStorage::Utf16(a), StrStorage::Utf16(b)) => a == b,
            (
                StrStorage::Cons {
                    left: l,
                    right: r,
                    len: n,
                },
                StrStorage::Cons {
                    left: l2,
                    right: r2,
                    len: n2,
                },
            ) => (l, r, n) == (l2, r2, n2),
            (
                StrStorage::Sliced {
                    parent: p,
                    start_utf16: s,
                    len: n,
                },
                StrStorage::Sliced {
                    parent: p2,
                    start_utf16: s2,
                    len: n2,
                },
            ) => (p, s, n) == (p2, s2, n2),
            _ => false,
        }
    }
}

impl Eq for StrStorage {}

/// A heap string: one storage variant plus the memoized content hash. The
/// hash is derivation state, not identity, so equality ignores it — a hashed
/// and an unhashed copy of the same storage remain equal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct V12Str {
    /// Backing storage and its encoding.
    pub storage: StrStorage,
    /// Content hash once computed via [`Heap::string_hash`]; `None` until
    /// first requested. Cached on the node so it dies with the slot.
    ///
    /// [`Heap::string_hash`]: crate::Heap::string_hash
    pub hash: Option<u32>,
}

impl V12Str {
    /// A flat Latin-1 string over `bytes`.
    pub fn latin1(bytes: Vec<u8>) -> Self {
        Self {
            storage: StrStorage::Latin1(bytes),
            hash: None,
        }
    }

    /// A flat UTF-16 string over `units`.
    pub fn utf16(units: Vec<u16>) -> Self {
        Self {
            storage: StrStorage::Utf16(units),
            hash: None,
        }
    }

    /// Number of UTF-16 code units. O(1) for every variant: composites
    /// store their length.
    pub fn len(&self) -> usize {
        match &self.storage {
            StrStorage::Latin1(v) => v.len(),
            StrStorage::Utf16(v) => v.len(),
            StrStorage::Cons { len, .. } | StrStorage::Sliced { len, .. } => *len as usize,
        }
    }

    /// True when there are no code units.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the backing store is a Latin-1 *leaf*. Composites have no
    /// single encoding — they may span both — so they answer `false`.
    pub fn is_latin1(&self) -> bool {
        matches!(self.storage, StrStorage::Latin1(_))
    }

    /// True when the text is materialized (no Cons/Sliced indirection).
    pub fn is_flat(&self) -> bool {
        matches!(self.storage, StrStorage::Latin1(_) | StrStorage::Utf16(_))
    }
}

impl HeapSpace for V12Str {
    const SPACE: Space = Space::Strings;
}

// Tracing: composite nodes hold the only strong references to their
// children, so a cons/sliced chain stays alive exactly as long as its head
// is reachable. Flat leaves reference nothing.
impl Trace for V12Str {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        match &self.storage {
            StrStorage::Cons { left, right, .. } => {
                left.trace(sink);
                right.trace(sink);
            }
            StrStorage::Sliced { parent, .. } => parent.trace(sink),
            StrStorage::Latin1(_) | StrStorage::Utf16(_) => {}
        }
    }
}

impl SizeEstimate for V12Str {
    fn approx_size(&self) -> usize {
        core::mem::size_of::<Self>()
            + match &self.storage {
                StrStorage::Latin1(v) => v.capacity(),
                StrStorage::Utf16(v) => v.capacity() * 2,
                // Composites carry handles and a length only; the payload
                // lives in the referenced slots, which account for themselves.
                StrStorage::Cons { .. } | StrStorage::Sliced { .. } => 0,
            }
    }
}

// ----------------------------------------------------------------------
// Tree walking
//
// Everything below resolves composite structures through the heap. All walks
// share one explicit-stack driver so deep chains never recurse.
// ----------------------------------------------------------------------

/// Where a walk starts: a string already resident in the heap, or a value
/// held by the caller whose composite children are resident (the case while
/// building a not-yet-allocated node).
pub(crate) enum Seed<'a> {
    Resident(Handle<V12Str>),
    Owned(&'a V12Str),
}

/// One pending subtree visit: `take` units of `node` starting at `skip`.
/// Ranges always name valid windows because construction validated them and
/// splitting preserves them.
struct WalkTask {
    node: Handle<V12Str>,
    skip: u32,
    take: u32,
}

/// A borrowed run of code units from one flat leaf, already positioned in
/// the walked string's coordinate space.
enum LeafRun<'a> {
    Latin1(&'a [u8]),
    Utf16(&'a [u16]),
}

/// Queues the children of a Cons for `[skip, skip + take)` in visit order.
/// The right-hand piece goes on the stack first so the left pops first —
/// LIFO yields left-to-right emission.
fn queue_cons_range(
    heap: &Heap,
    stack: &mut Vec<WalkTask>,
    left: Handle<V12Str>,
    right: Handle<V12Str>,
    skip: u32,
    take: u32,
) {
    let left_len = heap.get(left).len() as u32;
    let end = skip + take;
    if end > left_len {
        let right_skip = skip.saturating_sub(left_len);
        let right_take = end - left_len.max(skip);
        stack.push(WalkTask {
            node: right,
            skip: right_skip,
            take: right_take,
        });
    }
    let left_end = left_len.min(end);
    if left_end > skip {
        stack.push(WalkTask {
            node: left,
            skip,
            take: left_end - skip,
        });
    }
}

/// Emits every leaf run of `seed` left-to-right through `visit`.
fn walk_leaves<'a>(heap: &'a Heap, seed: Seed<'a>, visit: &mut impl FnMut(LeafRun<'a>)) {
    let mut stack: Vec<WalkTask> = Vec::new();
    match seed {
        Seed::Resident(handle) => {
            let len = heap.get(handle).len() as u32;
            if len > 0 {
                stack.push(WalkTask {
                    node: handle,
                    skip: 0,
                    take: len,
                });
            }
        }
        Seed::Owned(value) => match &value.storage {
            StrStorage::Latin1(bytes) => {
                if !bytes.is_empty() {
                    visit(LeafRun::Latin1(bytes));
                }
            }
            StrStorage::Utf16(units) => {
                if !units.is_empty() {
                    visit(LeafRun::Utf16(units));
                }
            }
            StrStorage::Cons { left, right, .. } => {
                let len = value.len() as u32;
                if len > 0 {
                    queue_cons_range(heap, &mut stack, *left, *right, 0, len);
                }
            }
            StrStorage::Sliced {
                parent,
                start_utf16,
                len,
            } => {
                if *len > 0 {
                    stack.push(WalkTask {
                        node: *parent,
                        skip: *start_utf16,
                        take: *len,
                    });
                }
            }
        },
    }

    while let Some(task) = stack.pop() {
        match &heap.get(task.node).storage {
            StrStorage::Latin1(bytes) => {
                let start = task.skip as usize;
                let end = start + task.take as usize;
                debug_assert!(end <= bytes.len(), "walk range exceeds leaf");
                visit(LeafRun::Latin1(&bytes[start..end]));
            }
            StrStorage::Utf16(units) => {
                let start = task.skip as usize;
                let end = start + task.take as usize;
                debug_assert!(end <= units.len(), "walk range exceeds leaf");
                visit(LeafRun::Utf16(&units[start..end]));
            }
            StrStorage::Cons { left, right, .. } => {
                queue_cons_range(heap, &mut stack, *left, *right, task.skip, task.take);
            }
            StrStorage::Sliced {
                parent,
                start_utf16,
                ..
            } => {
                stack.push(WalkTask {
                    node: *parent,
                    skip: start_utf16 + task.skip,
                    take: task.take,
                });
            }
        }
    }
}

/// Materializes `seed` into a single flat store: Latin-1 iff every leaf is
/// Latin-1, otherwise UTF-16 with Latin-1 leaves widened (module docs give
/// the rationale). Pure computation — allocates nothing collectable.
pub(crate) fn materialize(heap: &Heap, seed: Seed<'_>) -> StrStorage {
    let mut all_latin1 = true;
    let mut latin1: Vec<u8> = Vec::new();
    let mut utf16: Vec<u16> = Vec::new();

    walk_leaves(heap, seed, &mut |run| match run {
        LeafRun::Latin1(bytes) => {
            // Still pure-Latin1: append bytes. After a UTF-16 leaf appeared,
            // widen into the UTF-16 buffer instead.
            if all_latin1 {
                latin1.extend_from_slice(bytes);
            } else {
                utf16.extend(bytes.iter().copied().map(u16::from));
            }
        }
        LeafRun::Utf16(units) => {
            if all_latin1 {
                // First UTF-16 leaf: migrate everything collected so far.
                all_latin1 = false;
                utf16.extend(latin1.drain(..).map(u16::from));
            }
            utf16.extend_from_slice(units);
        }
    });

    if all_latin1 {
        StrStorage::Latin1(latin1)
    } else {
        StrStorage::Utf16(utf16)
    }
}

/// One FNV-1a step over a single code unit. Feeding the 16-bit unit directly
/// (rather than its two bytes) keeps Latin-1 and UTF-16 inputs identical:
/// a Latin-1 byte feeds as its zero-extended unit either way.
fn feed_fnv(state: &mut u32, unit: u16) {
    *state ^= u32::from(unit);
    *state = state.wrapping_mul(FNV_PRIME_32);
}

/// Streaming FNV-1a content hash over the UTF-16 view of `seed`.
pub(crate) fn content_hash(heap: &Heap, seed: Seed<'_>) -> u32 {
    let mut state = FNV_OFFSET_BASIS_32;
    walk_leaves(heap, seed, &mut |run| match run {
        LeafRun::Latin1(bytes) => {
            for &byte in bytes {
                feed_fnv(&mut state, u16::from(byte));
            }
        }
        LeafRun::Utf16(units) => {
            for &unit in units {
                feed_fnv(&mut state, unit);
            }
        }
    });
    state
}

/// Hash of an already-flat store; interning compares canonical instances,
/// which are flat by construction.
pub(crate) fn hash_flat(storage: &StrStorage) -> u32 {
    let mut state = FNV_OFFSET_BASIS_32;
    match storage {
        StrStorage::Latin1(bytes) => {
            for &byte in bytes {
                feed_fnv(&mut state, u16::from(byte));
            }
        }
        StrStorage::Utf16(units) => {
            for &unit in units {
                feed_fnv(&mut state, unit);
            }
        }
        StrStorage::Cons { .. } | StrStorage::Sliced { .. } => {
            panic!("canonical string instances are always flat")
        }
    }
    state
}

/// Encoding-normalized comparison of two flat stores: same-encoding slices
/// compare directly, mixed encodings compare unit-by-unit after widening.
pub(crate) fn flat_units_equal(a: &StrStorage, b: &StrStorage) -> bool {
    match (a, b) {
        (StrStorage::Latin1(x), StrStorage::Latin1(y)) => x == y,
        (StrStorage::Utf16(x), StrStorage::Utf16(y)) => x == y,
        (StrStorage::Latin1(x), StrStorage::Utf16(y))
        | (StrStorage::Utf16(y), StrStorage::Latin1(x)) => {
            x.iter().map(|&byte| u16::from(byte)).eq(y.iter().copied())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GcPolicy, Heap, JsValue};

    /// Allocates a Latin-1 leaf holding `text` (ASCII only in these tests).
    fn latin(heap: &mut Heap, text: &str) -> Handle<V12Str> {
        heap.alloc(V12Str::latin1(text.as_bytes().to_vec()))
    }

    /// Roots a string handle so it survives the next collection.
    fn root(heap: &mut Heap, handle: Handle<V12Str>) {
        heap.add_root(JsValue::string(handle));
    }

    /// Flattened view of a resident string as a byte vector; asserts the
    /// Latin-1 encoding the callers expect.
    fn latin_bytes(heap: &Heap, handle: Handle<V12Str>) -> Vec<u8> {
        match &heap.get(handle).storage {
            StrStorage::Latin1(bytes) => bytes.clone(),
            other => panic!("expected flat Latin-1, got {other:?}"),
        }
    }

    /// Flattened view of a resident string as UTF-16 units.
    fn utf16_units(heap: &Heap, handle: Handle<V12Str>) -> Vec<u16> {
        match &heap.get(handle).storage {
            StrStorage::Utf16(units) => units.clone(),
            other => panic!("expected flat UTF-16, got {other:?}"),
        }
    }

    #[test]
    fn flat_leaves_report_storage_and_length() {
        let s = V12Str::latin1(vec![b'a', b'b']);
        assert_eq!(s.len(), 2);
        assert!(s.is_latin1());
        assert!(s.is_flat());
        let u = V12Str::utf16(vec![0xD83D, 0xDE00]);
        assert_eq!(u.len(), 2);
        assert!(!u.is_latin1());
        assert!(V12Str::default().is_empty());
        assert!(V12Str::default().is_flat());
    }

    #[test]
    fn short_concat_materializes_long_concat_stays_lazy() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let a = latin(&mut heap, "ab");
        let b = latin(&mut heap, "cd");
        let small = heap.concat(a, b);
        // Under CONCAT_EAGER_FLATTEN_MAX_UNITS: flat on arrival…
        assert!(heap.get(small).is_flat());
        assert!(heap.get(small).is_latin1());
        assert_eq!(latin_bytes(&heap, small), b"abcd");

        // …while results past the threshold remain ropes.
        let long_a = latin(&mut heap, &"x".repeat(80));
        let long_b = latin(&mut heap, &"y".repeat(80));
        let big = heap.concat(long_a, long_b);
        assert!(!heap.get(big).is_flat(), "160 units must stay lazy");
        assert_eq!(heap.get(big).len(), 160);
        // Lazy does not mean unreadable: forced flattening preserves text.
        heap.flatten(big);
        let flattened = latin_bytes(&heap, big);
        assert_eq!(flattened.len(), 160);
        assert!(flattened[..80].iter().all(|&b| b == b'x'));
        assert!(flattened[80..].iter().all(|&b| b == b'y'));
    }

    #[test]
    fn flatten_on_flat_strings_is_a_noop() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let s = latin(&mut heap, "stable");
        heap.string_hash(s); // populate the cache too
        heap.flatten(s);
        assert_eq!(latin_bytes(&heap, s), b"stable");
        assert!(heap.get(s).hash.is_some());
    }

    #[test]
    fn nested_cons_flatten_joins_encodings_by_widening() {
        // Mixing policy: ANY UTF-16 leaf widens the whole result. Two
        // Latin-1 leaves around one UTF-16 leaf must produce one UTF-16
        // store containing all three parts in order.
        let mut heap = Heap::new(GcPolicy::NoGC);
        let left_units = "a".repeat(100);
        let left_leaf = heap.alloc(V12Str::latin1(left_units.as_bytes().to_vec()));
        let mid_text = "héllo".repeat(12); // 60 UTF-16 units, above the flat-leaf floor
        let mid_units: Vec<u16> = mid_text.encode_utf16().collect();
        assert_eq!(mid_units.len(), 60);
        let mid_leaf = heap.alloc(V12Str::utf16(mid_units.clone()));
        let right_units = "z".repeat(100);
        let right_leaf = heap.alloc(V12Str::latin1(right_units.as_bytes().to_vec()));

        // Sized past CONCAT_EAGER_FLATTEN_MAX_UNITS so both levels stay
        // genuinely lazy (ropes, not pre-flattened results).
        let inner = heap.concat(left_leaf, mid_leaf);
        assert!(!heap.get(inner).is_flat());
        let outer = heap.concat(inner, right_leaf);
        assert!(!heap.get(outer).is_flat());
        assert_eq!(heap.get(outer).len(), 260);

        heap.flatten(outer);
        assert!(
            !heap.get(outer).is_latin1(),
            "UTF-16 leaf must widen the join"
        );
        let units = utf16_units(&heap, outer);
        let expected: Vec<u16> = left_units
            .encode_utf16()
            .chain(mid_units)
            .chain(right_units.encode_utf16())
            .collect();
        assert_eq!(units, expected);
    }

    #[test]
    fn all_latin1_chains_flatten_to_latin1() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let a = latin(&mut heap, &"1".repeat(90));
        let b = latin(&mut heap, &"2".repeat(90));
        let c = latin(&mut heap, &"3".repeat(90));
        let ab = heap.concat(a, b);
        let abc = heap.concat(ab, c);
        assert!(!heap.get(abc).is_flat());

        heap.flatten(abc);
        assert!(
            heap.get(abc).is_latin1(),
            "no UTF-16 leaf: Latin-1 preserved"
        );
        let bytes = latin_bytes(&heap, abc);
        assert_eq!(bytes.len(), 270);
        assert!(bytes[..90].iter().all(|&b| b == b'1'));
        assert!(bytes[90..180].iter().all(|&b| b == b'2'));
        assert!(bytes[180..].iter().all(|&b| b == b'3'));
    }

    #[test]
    fn slice_bounds_are_checked_before_allocation() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let parent = latin(&mut heap, "abcd");

        assert!(heap.slice_string(parent, 2, 2).is_some()); // exact suffix
        assert!(heap.slice_string(parent, 4, 0).is_some()); // empty tail slice
        assert_eq!(heap.slice_string(parent, 3, 2), None); // end past length
        assert_eq!(heap.slice_string(parent, 5, 0), None); // start past length
        assert_eq!(heap.slice_string(parent, 4, 1), None);
        // Offset arithmetic itself must not wrap.
        assert_eq!(heap.slice_string(parent, u32::MAX, 1), None);
    }

    #[test]
    fn sliced_windows_read_through_any_parent_shape() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        // Parent is a rope (kept lazy by size), so slicing exercises a
        // composite parent.
        let left = latin(&mut heap, &"L".repeat(70));
        let right = latin(&mut heap, &"R".repeat(70));
        let rope = heap.concat(left, right);

        // Window straddling the internal leaf boundary.
        let mid = heap.slice_string(rope, 65, 10).expect("in-bounds window");
        assert_eq!(heap.get(mid).len(), 10);
        heap.flatten(mid);
        assert_eq!(latin_bytes(&heap, mid), b"LLLLLRRRRR");

        // Slice of a slice: offsets compose through the chain.
        let nested = heap.slice_string(mid, 3, 4).expect("in-bounds window");
        heap.flatten(nested);
        assert_eq!(latin_bytes(&heap, nested), b"LLRR");

        // Slice of a plain flat leaf.
        let flat_parent = latin(&mut heap, "abcdef");
        let tail = heap
            .slice_string(flat_parent, 3, 3)
            .expect("in-bounds window");
        heap.flatten(tail);
        assert_eq!(latin_bytes(&heap, tail), b"def");
    }

    #[test]
    fn string_hash_caches_once_and_matches_across_encodings() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let narrow = latin(&mut heap, "abc");
        let wide = heap.alloc(V12Str::utf16(vec![97, 98, 99]));

        let h1 = heap.string_hash(narrow);
        assert!(heap.get(narrow).hash.is_some(), "first computation caches");
        let h2 = heap.string_hash(wide);
        assert_eq!(h1, h2, "equal texts hash equally across encodings");
        // Second call serves the cache and agrees.
        assert_eq!(heap.string_hash(narrow), h1);

        // Different text, different hash (FNV makes accidental agreement
        // vanishingly unlikely for realistic neighbors).
        let other = latin(&mut heap, "abd");
        assert_ne!(heap.string_hash(other), h1);
    }

    #[test]
    fn strings_equal_compares_text_not_representation() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let narrow = latin(&mut heap, "abc");
        let wide = heap.alloc(V12Str::utf16(vec![97, 98, 99]));
        let changed = latin(&mut heap, "abd");
        let shorter = latin(&mut heap, "ab");

        assert!(heap.strings_equal(narrow, narrow));
        assert!(
            heap.strings_equal(narrow, wide),
            "same text, mixed encoding"
        );
        assert!(heap.strings_equal(wide, narrow));
        assert!(!heap.strings_equal(narrow, changed));
        assert!(!heap.strings_equal(narrow, shorter));

        // Equal texts built by different lazy routes compare equal too.
        let ab = latin(&mut heap, &"q".repeat(70));
        let c = latin(&mut heap, &"r".repeat(70));
        let rope_ab = heap.concat(ab, c);
        let d = latin(&mut heap, &"s".repeat(140));
        let rope_cd = heap.concat(d, c); // same tail shape, different head
        assert!(!heap.strings_equal(rope_ab, rope_cd));

        let twin_left = latin(&mut heap, &"q".repeat(70));
        let twin = heap.concat(twin_left, c);
        assert!(
            heap.strings_equal(rope_ab, twin),
            "equal ropes, unequal nodes"
        );
    }

    #[test]
    fn intern_returns_the_same_id_across_shapes_and_encodings() {
        let mut heap = Heap::new(GcPolicy::NoGC);

        let first = heap.intern_string(V12Str::latin1(b"hello".to_vec()));
        let again = heap.intern_string(V12Str::latin1(b"hello".to_vec()));
        let wide = heap.intern_string(V12Str::utf16("hello".encode_utf16().collect()));
        assert_eq!(first, again, "re-interning returns the canonical id");
        assert_eq!(first, wide, "encoding must not matter to identity");
        assert!(heap.get(first).is_flat(), "canonical instances are flat");
        assert!(heap.get(first).hash.is_some(), "canonical hash precomputed");

        let other = heap.intern_string(V12Str::latin1(b"hellp".to_vec()));
        assert_ne!(first, other);

        // Empty strings intern to one instance as well.
        let e1 = heap.intern_string(V12Str::latin1(Vec::new()));
        let e2 = heap.intern_string(V12Str::utf16(Vec::new()));
        assert_eq!(e1, e2);
        assert_ne!(e1, first);
    }

    #[test]
    fn interning_a_composite_dedups_onto_the_canonical_instance() {
        let mut heap = Heap::new(GcPolicy::NoGC);

        // Same text, two different rope shapes (left-leaning vs right-leaning),
        // each level sized past the eager-flatten bound to stay lazy.
        let a = latin(&mut heap, &"w".repeat(80));
        let b = latin(&mut heap, &"x".repeat(80));
        let c = latin(&mut heap, &"y".repeat(80));
        let ab = heap.concat(a, b);
        let left_leaning = heap.concat(ab, c);
        let b2 = latin(&mut heap, &"x".repeat(80));
        let c2 = latin(&mut heap, &"y".repeat(80));
        let bc = heap.concat(b2, c2);
        let a2 = latin(&mut heap, &"w".repeat(80));
        let right_leaning = heap.concat(a2, bc);
        assert!(!heap.get(left_leaning).is_flat());
        assert!(!heap.get(right_leaning).is_flat());

        // Cloning the resident value hands `intern_string` an owned seed:
        // its composite children are still-resident handles.
        let from_left = heap.intern_string(heap.get(left_leaning).clone());
        let from_right = heap.intern_string(heap.get(right_leaning).clone());

        assert_eq!(from_left, from_right, "structure must not affect identity");
        assert!(heap.get(from_left).is_flat());
        assert_eq!(heap.get(from_left).len(), 240);
    }

    #[test]
    fn deep_cons_chains_walk_iteratively_without_stack_growth() {
        // Native recursion over a chain this deep would blow the stack; the
        // explicit-task-stack walker (and flatten/hash on top of it) must not.
        const DEPTH: usize = 50_000;
        let mut heap = Heap::new(GcPolicy::NoGC);
        let twig = latin(&mut heap, "t");
        root(&mut heap, twig);

        let mut head = twig;
        for len_next in 2u32..=(DEPTH as u32) + 1 {
            // Built directly rather than via `concat`, which would eagerly
            // flatten these tiny results away.
            let node = heap.alloc(V12Str {
                storage: StrStorage::Cons {
                    left: head,
                    right: twig,
                    len: len_next,
                },
                hash: None,
            });
            root(&mut heap, node); // link into rooted storage before next alloc
            head = node;
        }
        assert_eq!(heap.get(head).len(), DEPTH + 1);

        let hash = heap.string_hash(head);
        heap.flatten(head);
        assert!(heap.get(head).is_flat());
        assert_eq!(heap.string_hash(head), hash, "cached hash survives flatten");
        let bytes = latin_bytes(&heap, head);
        assert_eq!(bytes.len(), DEPTH + 1);
        assert!(bytes.iter().all(|&b| b == b't'));
    }

    #[test]
    fn gc_stress_one_keeps_cons_and_slice_chains_alive() {
        let mut heap = Heap::new(GcPolicy::NoGC);
        heap.gc_stress(Some(1));

        // Every concat/slice pins its operands across the internal
        // allocation; each new head is rooted before the next call.
        let a = latin(&mut heap, &"A".repeat(70));
        root(&mut heap, a);
        let b = latin(&mut heap, &"B".repeat(70));
        root(&mut heap, b);
        let rope = heap.concat(a, b); // stays lazy: 140 units
        root(&mut heap, rope);
        let longer = heap.concat(rope, b);
        root(&mut heap, longer);
        let window = heap.slice_string(longer, 60, 40).expect("in-bounds");
        root(&mut heap, window);

        // Stress cadence has been collecting throughout; settle the state.
        heap.force_collect();

        assert_eq!(heap.live_count::<V12Str>(), 5, "whole chain must survive");

        // Contents intact after the churn: window spans the A/B boundary
        // of `rope` inside `longer`.
        heap.flatten(window);
        let bytes = latin_bytes(&heap, window);
        assert_eq!(bytes.len(), 40);
        assert!((0..10).all(|i| bytes[i] == b'A'));
        assert!((10..40).all(|i| bytes[i] == b'B'));

        // Hashing still works on churned survivors.
        let h = heap.string_hash(longer);
        assert_eq!(heap.string_hash(longer), h);

        // Unroot everything: the entire chain dies together.
        heap.roots_mut().0.clear();
        heap.force_collect();
        assert_eq!(heap.live_count::<V12Str>(), 0);
    }
}
