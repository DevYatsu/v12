//! Element storage ([`ElementsArray`]): integer-indexed data held in one of
//! several increasingly general representations.
//!
//! ## The lattice
//!
//! Seven kinds, totally ordered from most specialized to most general:
//!
//! ```text
//! PackedSmi < HoleySmi < PackedDouble < HoleyDouble
//!           < PackedObject < HoleyObject < Dictionary
//! ```
//!
//! Every legitimate transition (appending a hole, widening a number, storing
//! a non-number, going sparse) moves strictly upward in this order, so
//! choosing the next representation is a `max` of the current kind and the
//! minimum kind that admits the incoming value. A total order is sound here
//! because each rung's element encoding is derivable from the rung below it
//! — no information is ever destroyed by moving up — which makes the simpler
//! ordering safe where classic engines use a branching lattice. One subtlety:
//! a *hole* arriving at a packed kind selects the holey sibling of the same
//! payload type (PackedSmi → HoleySmi), never a higher payload rung, so hole
//! insertion alone never changes how existing elements decode.
//!
//! ## Conversions
//!
//! Conversions are one-way and preserve every visible value: packed arrays
//! reinterpret in place, holey arrays carry the internal hole marker across,
//! and dictionary conversion simply stops leaving gaps implicit. Doubles are
//! canonicalized once at the write boundary (NaN payloads unobservable
//! through [`crate::JsValue`]), so re-boxing during conversion is lossless.
//!
//! ## Dictionary mode
//!
//! Two conditions force the hash-backed [`ElementsDictionary`]: indices at or
//! beyond [`ELEMENTS_TO_DICTIONARY_INDEX`] ("huge"), and writes whose gap
//! above the current length exceeds [`MAX_FAST_ELEMENT_GAP`] ("sparse"). Both
//! exist for the same reason — a flat vector whose occupancy drops far below
//! its length trades memory and clearing cost for nothing.
//!
//! Dictionary entries cache their key hash at insertion (pure function of the
//! index, stable across runs). Recomputing the mix is cheap, but the stored
//! copy makes scans and future rehashing pay nothing per probe.

use crate::gc::{MarkSink, Trace};
use crate::value::JsValue;

use std::vec::Vec;

/// Writes at or above this index switch the store to dictionary form.
///
/// V8's `ElementsKind` lattice switches to dictionary elements at a similar
/// huge-index bound (its `kMaxFastIndex`/`kMaxFastArrayLength` policy) for
/// the same reason: a flat vector grown to a sparse, huge index wastes memory
/// and clearing cost on every operation. The exact value is a tuning
/// constant; the *lattice-and-escape* shape is the shared precedent.
pub const ELEMENTS_TO_DICTIONARY_INDEX: u32 = 1024;

/// Writes leaving a gap larger than this above the current length switch the
/// store to dictionary form.
pub const MAX_FAST_ELEMENT_GAP: u32 = 64;

/// Which element representation a store currently uses.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ElementsKind {
    /// Dense; every element is a small integer.
    PackedSmi,
    /// Small integers with internal holes.
    HoleySmi,
    /// Dense; every element is a double.
    PackedDouble,
    /// Doubles with internal holes.
    HoleyDouble,
    /// Dense; arbitrary values.
    PackedObject,
    /// Arbitrary values with internal holes.
    HoleyObject,
    /// Hash-backed sparse store; terminal rung.
    Dictionary,
}

impl ElementsKind {
    /// Position in the generalization order; higher = more general.
    pub fn rank(self) -> u8 {
        match self {
            ElementsKind::PackedSmi => 0,
            ElementsKind::HoleySmi => 1,
            ElementsKind::PackedDouble => 2,
            ElementsKind::HoleyDouble => 3,
            ElementsKind::PackedObject => 4,
            ElementsKind::HoleyObject => 5,
            ElementsKind::Dictionary => 6,
        }
    }

    /// True for the three hole-bearing rungs.
    pub fn is_holey(self) -> bool {
        matches!(
            self,
            ElementsKind::HoleySmi | ElementsKind::HoleyDouble | ElementsKind::HoleyObject
        )
    }

    /// True for the terminal hash-backed rung.
    pub fn is_dictionary(self) -> bool {
        self == ElementsKind::Dictionary
    }

    /// The hole-bearing sibling of the same payload type (packed → holey;
    /// holey kinds and the dictionary map to themselves).
    pub fn to_holey(self) -> ElementsKind {
        match self {
            ElementsKind::PackedSmi => ElementsKind::HoleySmi,
            ElementsKind::PackedDouble => ElementsKind::HoleyDouble,
            ElementsKind::PackedObject => ElementsKind::HoleyObject,
            other => other,
        }
    }
}

/// An element value as seen by the write path, decoded once and carried with
/// its payload so later storage never has to re-parse a `JsValue`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ValueClass<'a> {
    /// Internal absent-element marker.
    Hole,
    /// Small integer within Smi range.
    Smi(i32),
    /// Any double.
    Double(f64),
    /// Anything else (objects, strings, symbols, …).
    Other(&'a JsValue),
}

impl ValueClass<'_> {
    /// The canonical `JsValue` this class stores back as.
    fn to_value(self) -> JsValue {
        match self {
            ValueClass::Hole => JsValue::hole(),
            ValueClass::Smi(n) => smi_value(n),
            ValueClass::Double(d) => JsValue::from_f64(d),
            ValueClass::Other(v) => *v,
        }
    }
}

fn classify(value: &JsValue) -> ValueClass<'_> {
    if value.is_hole() {
        ValueClass::Hole
    } else if let Some(n) = value.as_smi() {
        ValueClass::Smi(n)
    } else if let Some(d) = value.as_f64() {
        ValueClass::Double(d)
    } else {
        ValueClass::Other(value)
    }
}

/// Boxes a small integer. Total even for out-of-range input (which the write
/// path never produces): those widen to a double rather than panic, keeping
/// every conversion branch infallible.
fn smi_value(n: i32) -> JsValue {
    JsValue::from_i32_smi(n).unwrap_or_else(|| JsValue::from_f64(f64::from(n)))
}

/// Sparse element store: index → value, with per-entry cached key hashes.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementsDictionary {
    entries: rustc_hash::FxHashMap<u32, DictEntry>,
    /// Highest index ever inserted, plus one — the array-length view.
    length: u32,
}

/// One dictionary entry: value plus the hash cached at insertion time.
#[derive(Clone, Debug, PartialEq)]
pub struct DictEntry {
    pub hash: u64,
    pub value: JsValue,
}

/// Pure, seedless mix of an element index (splitmix64 finalizer): stable
/// across processes, uniformly spread, cheap enough to recompute.
fn element_key_hash(index: u32) -> u64 {
    let mut z = u64::from(index) ^ 0x9E37_79B9_7F4A_7C15;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl ElementsDictionary {
    /// Empty dictionary.
    pub fn new() -> Self {
        Self {
            entries: rustc_hash::FxHashMap::default(),
            length: 0,
        }
    }

    /// Inserts or overwrites `index`, refreshing the cached hash.
    pub fn insert(&mut self, index: u32, value: JsValue) {
        self.entries.insert(
            index,
            DictEntry {
                hash: element_key_hash(index),
                value,
            },
        );
        self.length = self.length.max(index + 1);
    }

    /// Value at `index`, or `None` when absent.
    pub fn get(&self, index: u32) -> Option<JsValue> {
        self.entries.get(&index).map(|e| e.value)
    }

    /// Number of present elements (holes do not exist here).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no elements are present.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Array-length view: highest index ever inserted, plus one.
    pub fn length(&self) -> u32 {
        self.length
    }

    /// Cached hash for `index` as recorded at insertion (`None` when
    /// absent). Exposed so tooling can observe the caching policy.
    pub fn cached_hash(&self, index: u32) -> Option<u64> {
        self.entries.get(&index).map(|e| e.hash)
    }

    /// Iterates `(index, entry)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &DictEntry)> {
        self.entries.iter().map(|(k, e)| (*k, e))
    }

    /// Rough retained bytes for GC accounting.
    fn retained_bytes(&self) -> usize {
        self.entries.capacity()
            * (core::mem::size_of::<u32>() + core::mem::size_of::<DictEntry>() + 1)
    }
}

impl Default for ElementsDictionary {
    fn default() -> Self {
        Self::new()
    }
}

/// All element storage for one object. See the module docs for the kind
/// lattice and the conversion policy.
#[derive(Clone, Debug, PartialEq)]
pub struct ElementsArray {
    storage: ElementsStorage,
}

#[derive(Clone, Debug, PartialEq)]
enum ElementsStorage {
    PackedSmi(Vec<i32>),
    HoleySmi(Vec<JsValue>),
    PackedDouble(Vec<f64>),
    HoleyDouble(Vec<JsValue>),
    PackedObject(Vec<JsValue>),
    HoleyObject(Vec<JsValue>),
    Dictionary(ElementsDictionary),
}

impl Default for ElementsArray {
    fn default() -> Self {
        Self {
            storage: ElementsStorage::PackedSmi(Vec::new()),
        }
    }
}

impl ElementsStorage {
    /// Current representation of this storage variant.
    fn kind(&self) -> ElementsKind {
        match self {
            ElementsStorage::PackedSmi(_) => ElementsKind::PackedSmi,
            ElementsStorage::HoleySmi(_) => ElementsKind::HoleySmi,
            ElementsStorage::PackedDouble(_) => ElementsKind::PackedDouble,
            ElementsStorage::HoleyDouble(_) => ElementsKind::HoleyDouble,
            ElementsStorage::PackedObject(_) => ElementsKind::PackedObject,
            ElementsStorage::HoleyObject(_) => ElementsKind::HoleyObject,
            ElementsStorage::Dictionary(_) => ElementsKind::Dictionary,
        }
    }
}

impl ElementsArray {
    /// A fresh packed-Smi store.
    pub const fn new() -> Self {
        Self {
            storage: ElementsStorage::PackedSmi(Vec::new()),
        }
    }

    /// Current representation.
    pub fn kind(&self) -> ElementsKind {
        self.storage.kind()
    }

    /// Length view: element count for fast kinds, highest-index-plus-one for
    /// dictionaries.
    pub fn len(&self) -> usize {
        match &self.storage {
            ElementsStorage::PackedSmi(v) => v.len(),
            ElementsStorage::HoleySmi(v)
            | ElementsStorage::HoleyDouble(v)
            | ElementsStorage::PackedObject(v)
            | ElementsStorage::HoleyObject(v) => v.len(),
            ElementsStorage::PackedDouble(v) => v.len(),
            ElementsStorage::Dictionary(dict) => dict.length() as usize,
        }
    }

    /// True when no elements are stored (or the length view is zero).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Element at `index`; `None` when out of bounds or absent (a hole).
    pub fn get(&self, index: u32) -> Option<JsValue> {
        let i = index as usize;
        match &self.storage {
            ElementsStorage::PackedSmi(v) => v.get(i).map(|&n| smi_value(n)),
            ElementsStorage::HoleySmi(v) => v.get(i).copied().filter(|v| !v.is_hole()),
            ElementsStorage::PackedDouble(v) => v.get(i).map(|&d| JsValue::from_f64(d)),
            ElementsStorage::HoleyDouble(v) => v.get(i).copied().filter(|v| !v.is_hole()),
            ElementsStorage::PackedObject(v) => v.get(i).copied(),
            ElementsStorage::HoleyObject(v) => v.get(i).copied().filter(|v| !v.is_hole()),
            ElementsStorage::Dictionary(dict) => dict.get(index),
        }
    }

    /// Appends at the end, generalizing as needed.
    pub fn push(&mut self, value: JsValue) {
        self.set(self.len() as u32, value);
    }

    /// Removes and returns the last element (`None` when empty). Shrinks the
    /// flat backing vectors; dictionary storage removes the highest index.
    pub fn pop(&mut self) -> Option<JsValue> {
        match &mut self.storage {
            ElementsStorage::PackedSmi(v) => v.pop().map(smi_value),
            ElementsStorage::HoleySmi(v)
            | ElementsStorage::HoleyDouble(v)
            | ElementsStorage::PackedObject(v)
            | ElementsStorage::HoleyObject(v) => v.pop(),
            ElementsStorage::PackedDouble(v) => v.pop().map(JsValue::from_f64),
            ElementsStorage::Dictionary(dict) => {
                if dict.length() == 0 {
                    return None;
                }
                let last = dict.length() - 1;
                let v = dict.entries.remove(&last).map(|e| e.value);
                // Recompute the length view: highest remaining index + 1.
                dict.length = dict.entries.keys().max().map_or(0, |&k| k + 1);
                v
            }
        }
    }

    /// Iterates present elements in index order (`None` for holes, which the
    /// caller treats as absent). Dictionary storage iterates its entries.
    pub fn iter(&self) -> Box<dyn Iterator<Item = JsValue> + '_> {
        match &self.storage {
            ElementsStorage::PackedSmi(v) => Box::new(v.iter().map(|&n| smi_value(n))),
            ElementsStorage::HoleySmi(v)
            | ElementsStorage::HoleyDouble(v)
            | ElementsStorage::PackedObject(v)
            | ElementsStorage::HoleyObject(v) => Box::new(v.iter().copied()),
            ElementsStorage::PackedDouble(v) => Box::new(v.iter().map(|&d| JsValue::from_f64(d))),
            ElementsStorage::Dictionary(dict) => Box::new(dict.iter().map(|(_, e)| e.value)),
        }
    }

    /// Writes `index`, generalizing along the lattice whenever the current
    /// rung cannot hold the value (module docs cover the policy). Data below
    /// `index` is preserved through every conversion.
    pub fn set(&mut self, index: u32, value: JsValue) {
        let class = classify(&value);

        // Representation choice: dictionary escape hatch, else the lattice
        // join of what we have and what the write needs.
        let target = match self.plan(&class, index) {
            Some(kind) => kind,
            None => {
                self.convert_to(ElementsKind::Dictionary);
                ElementsKind::Dictionary
            }
        };
        if target != self.kind() {
            self.convert_to(target);
        }

        // Gap-filling above the current end needs hole-capable storage,
        // which `plan` already guaranteed by promoting the kind. Only fill
        // when a gap actually exists — a dense write (`index == fast_len`)
        // on a packed kind must not demand holey storage.
        if index as usize > self.fast_len() {
            self.fill_gaps(index as usize);
        }

        // Snapshot the post-write kind: the dictionary arm below needs it in
        // a guard, but borrowing `self` immutably inside the `&mut self.storage`
        // match is not allowed.
        let is_dict = self.kind() == ElementsKind::Dictionary;
        match (&mut self.storage, class) {
            (_, class) if is_dict => {
                if let ElementsStorage::Dictionary(dict) = &mut self.storage {
                    dict.insert(index, class.to_value());
                }
            }
            (ElementsStorage::PackedSmi(v), ValueClass::Smi(n)) => put_at(v, index, n),
            (ElementsStorage::HoleySmi(v), ValueClass::Smi(n)) => put_at(v, index, smi_value(n)),
            (ElementsStorage::HoleySmi(v), ValueClass::Hole) => put_at(v, index, JsValue::hole()),
            (ElementsStorage::PackedDouble(v), ValueClass::Double(d)) => put_at(v, index, d),
            // Small integers ride in double rungs losslessly.
            (ElementsStorage::PackedDouble(v), ValueClass::Smi(n)) => {
                put_at(v, index, f64::from(n))
            }
            (ElementsStorage::HoleyDouble(v), ValueClass::Double(d)) => {
                put_at(v, index, JsValue::from_f64(d))
            }
            (ElementsStorage::HoleyDouble(v), ValueClass::Smi(n)) => {
                put_at(v, index, JsValue::from_f64(f64::from(n)))
            }
            (ElementsStorage::HoleyDouble(v), ValueClass::Hole) => {
                put_at(v, index, JsValue::hole())
            }
            // Object rungs admit every class.
            (ElementsStorage::PackedObject(v), class) => put_at(v, index, class.to_value()),
            (ElementsStorage::HoleyObject(v), class) => put_at(v, index, class.to_value()),
            // Any remaining combination is a plan/store disagreement; the
            // dictionary absorbs it rather than corrupting a fast rung.
            (_, class) => {
                self.convert_to(ElementsKind::Dictionary);
                if let ElementsStorage::Dictionary(dict) = &mut self.storage {
                    dict.insert(index, class.to_value());
                }
            }
        }
    }

    /// Current element count of the flat backing vector (dictionary counts
    /// as zero-length here; its inserts go through the map).
    fn fast_len(&self) -> usize {
        match &self.storage {
            ElementsStorage::PackedSmi(v) => v.len(),
            ElementsStorage::HoleySmi(v)
            | ElementsStorage::HoleyDouble(v)
            | ElementsStorage::PackedObject(v)
            | ElementsStorage::HoleyObject(v) => v.len(),
            ElementsStorage::PackedDouble(v) => v.len(),
            ElementsStorage::Dictionary(_) => 0,
        }
    }

    /// Chooses the post-write kind, or `None` to escape to dictionary form.
    fn plan(&self, class: &ValueClass<'_>, index: u32) -> Option<ElementsKind> {
        let current = self.kind();
        if current.is_dictionary() {
            return Some(current);
        }
        // Sparse or huge: flat storage would waste memory chasing this index.
        if index >= ELEMENTS_TO_DICTIONARY_INDEX
            || index.saturating_sub(self.len() as u32) > MAX_FAST_ELEMENT_GAP
        {
            return None;
        }
        let needs_gap_fill = index as usize > self.fast_len();
        let minimum = match class {
            // A hole never changes how neighbors decode: holey sibling only.
            ValueClass::Hole => current.to_holey(),
            // Smis fit every rung; only gap-filling forces holeiness.
            ValueClass::Smi(_) => {
                if needs_gap_fill {
                    current.to_holey()
                } else {
                    current
                }
            }
            ValueClass::Double(_) => {
                if current.is_holey() || needs_gap_fill {
                    ElementsKind::HoleyDouble
                } else {
                    ElementsKind::PackedDouble
                }
            }
            ValueClass::Other(_) => {
                if current.is_holey() || needs_gap_fill {
                    ElementsKind::HoleyObject
                } else {
                    ElementsKind::PackedObject
                }
            }
        };
        Some(if minimum.rank() > current.rank() {
            minimum
        } else {
            current
        })
    }

    fn convert_to(&mut self, target: ElementsKind) {
        // Swap in a harmless placeholder so the source can be moved out and
        // consumed by the conversion.
        let source = std::mem::replace(
            &mut self.storage,
            ElementsStorage::Dictionary(ElementsDictionary::new()),
        );
        self.storage = convert_storage(source, target);
    }

    /// Appends holes until the flat vector reaches `len`. Only legal — and
    /// only reachable — on holey rungs, which storage conversion promotes to
    /// before any gap fill.
    fn fill_gaps(&mut self, len: usize) {
        debug_assert!(
            self.kind().is_holey(),
            "gap fill requested on non-holey storage"
        );
        while self.fast_len() < len {
            match &mut self.storage {
                ElementsStorage::HoleySmi(v)
                | ElementsStorage::HoleyDouble(v)
                | ElementsStorage::HoleyObject(v) => v.push(JsValue::hole()),
                _ => break,
            }
        }
    }

    /// Rough retained bytes for GC accounting.
    pub(crate) fn retained_bytes(&self) -> usize {
        const UNIT: usize = core::mem::size_of::<JsValue>();
        match &self.storage {
            ElementsStorage::PackedSmi(v) => v.capacity() * core::mem::size_of::<i32>(),
            ElementsStorage::HoleySmi(v)
            | ElementsStorage::HoleyDouble(v)
            | ElementsStorage::PackedObject(v)
            | ElementsStorage::HoleyObject(v) => v.capacity() * UNIT,
            ElementsStorage::PackedDouble(v) => v.capacity() * core::mem::size_of::<f64>(),
            ElementsStorage::Dictionary(dict) => dict.retained_bytes(),
        }
    }
}

/// Flat-storage index for a JS element index. JS array indices are `u32`
/// by spec (the `ElementsArray` API surface) while `Vec`s index by `usize`;
/// this is the single lossless widening point for the two domains.
#[inline]
fn flat_index(index: u32) -> usize {
    index as usize
}

fn put_at<T: Clone>(v: &mut Vec<T>, index: u32, item: T) {
    let i = flat_index(index);
    if i < v.len() {
        v[i] = item;
    } else {
        debug_assert_eq!(i, v.len(), "fast-path writes must be dense");
        v.push(item);
    }
}

/// Total reinterpretation of a stored value as a double: smis widen, doubles
/// pass through, anything else (unreachable by construction) becomes NaN.
fn as_double(v: JsValue) -> f64 {
    if let Some(d) = v.as_f64() {
        d
    } else if let Some(n) = v.as_smi() {
        f64::from(n)
    } else {
        f64::NAN
    }
}

/// Converts one storage representation into another. Only upward lattice
/// moves are ever requested (see [`ElementsArray::plan`]); the exhaustive
/// match covers exactly those pairs, with the dictionary as the safe
/// universal fallback for anything else — value-preserving, since every
/// element can live in the map.
fn convert_storage(source: ElementsStorage, target: ElementsKind) -> ElementsStorage {
    if source.kind() == target {
        return source;
    }

    // Shared builders over flat vectors.
    let smis_as_values = |v: &[i32]| v.iter().map(|&n| smi_value(n)).collect::<Vec<JsValue>>();
    let doubles_as_values = |v: &[f64]| {
        v.iter()
            .map(|&d| JsValue::from_f64(d))
            .collect::<Vec<JsValue>>()
    };

    match source {
        ElementsStorage::PackedSmi(v) => match target {
            ElementsKind::HoleySmi => ElementsStorage::HoleySmi(smis_as_values(&v)),
            ElementsKind::PackedDouble => {
                ElementsStorage::PackedDouble(v.iter().map(|&n| f64::from(n)).collect())
            }
            ElementsKind::HoleyDouble => ElementsStorage::HoleyDouble(doubles_as_values(
                &v.iter().map(|&n| f64::from(n)).collect::<Vec<f64>>(),
            )),
            ElementsKind::PackedObject => ElementsStorage::PackedObject(smis_as_values(&v)),
            ElementsKind::HoleyObject => ElementsStorage::HoleyObject(smis_as_values(&v)),
            _ => {
                dictionary_from_pairs(v.iter().enumerate().map(|(i, &n)| (i as u32, smi_value(n))))
            }
        },
        ElementsStorage::HoleySmi(v) => match target {
            ElementsKind::HoleyDouble => ElementsStorage::HoleyDouble(
                v.into_iter()
                    .map(|u| {
                        if u.is_hole() {
                            u
                        } else {
                            JsValue::from_f64(as_double(u))
                        }
                    })
                    .collect(),
            ),
            ElementsKind::HoleyObject => ElementsStorage::HoleyObject(v),
            _ => dictionary_from_pairs(
                v.into_iter()
                    .enumerate()
                    .filter(|(_, u)| !u.is_hole())
                    .map(|(i, u)| (i as u32, u)),
            ),
        },
        ElementsStorage::PackedDouble(v) => match target {
            ElementsKind::HoleyDouble => ElementsStorage::HoleyDouble(doubles_as_values(&v)),
            ElementsKind::PackedObject => ElementsStorage::PackedObject(doubles_as_values(&v)),
            ElementsKind::HoleyObject => ElementsStorage::HoleyObject(doubles_as_values(&v)),
            _ => dictionary_from_pairs(
                v.into_iter()
                    .enumerate()
                    .map(|(i, d)| (i as u32, JsValue::from_f64(d))),
            ),
        },
        ElementsStorage::HoleyDouble(v) => match target {
            ElementsKind::HoleyObject => ElementsStorage::HoleyObject(v),
            _ => dictionary_from_pairs(
                v.into_iter()
                    .enumerate()
                    .filter(|(_, u)| !u.is_hole())
                    .map(|(i, u)| (i as u32, u)),
            ),
        },
        ElementsStorage::PackedObject(v) => match target {
            ElementsKind::HoleyObject => ElementsStorage::HoleyObject(v),
            _ => dictionary_from_pairs(v.into_iter().enumerate().map(|(i, u)| (i as u32, u))),
        },
        ElementsStorage::HoleyObject(v) => dictionary_from_pairs(
            v.into_iter()
                .enumerate()
                .filter(|(_, u)| !u.is_hole())
                .map(|(i, u)| (i as u32, u)),
        ),
        ElementsStorage::Dictionary(dict) => ElementsStorage::Dictionary(dict),
    }
}

/// Builds dictionary storage from `(index, value)` pairs.
fn dictionary_from_pairs(pairs: impl Iterator<Item = (u32, JsValue)>) -> ElementsStorage {
    let mut dict = ElementsDictionary::new();
    for (index, value) in pairs {
        dict.insert(index, value);
    }
    ElementsStorage::Dictionary(dict)
}

impl Trace for ElementsArray {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        match &self.storage {
            ElementsStorage::PackedSmi(_) | ElementsStorage::PackedDouble(_) => {}
            ElementsStorage::HoleySmi(v)
            | ElementsStorage::HoleyDouble(v)
            | ElementsStorage::PackedObject(v)
            | ElementsStorage::HoleyObject(v) => v.trace(sink),
            ElementsStorage::Dictionary(dict) => {
                for (_, entry) in dict.iter() {
                    entry.value.trace(sink);
                }
            }
        }
    }
}

impl crate::object::SizeEstimate for ElementsArray {
    fn approx_size(&self) -> usize {
        core::mem::size_of::<Self>() + self.retained_bytes()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn dense_smi_writes_fill_lattice() {
        let mut arr = ElementsArray::new();
        arr.set(0, JsValue::from_i32_smi(5).unwrap());
        arr.set(1, JsValue::from_i32_smi(6).unwrap());
        assert_eq!(arr.len(), 2);
        assert_eq!(arr.get(0).and_then(|v| v.as_smi()), Some(5));
        assert_eq!(arr.get(1).and_then(|v| v.as_smi()), Some(6));
    }

    #[test]
    fn pop_removes_last_and_shrinks() {
        let mut arr = ElementsArray::new();
        arr.set(0, JsValue::from_i32_smi(5).unwrap());
        arr.set(1, JsValue::from_i32_smi(6).unwrap());
        assert_eq!(arr.pop().and_then(|v| v.as_smi()), Some(6));
        assert_eq!(arr.len(), 1);
        assert_eq!(arr.pop().and_then(|v| v.as_smi()), Some(5));
        assert_eq!(arr.len(), 0);
        assert_eq!(arr.pop(), None);
    }
}
