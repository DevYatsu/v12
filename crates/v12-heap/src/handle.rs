//! Typed handles: a `Handle<T>` is a `u32` index into one space of the
//! heap's slot storage. The `JsValue` tag names the space, so converting a
//! value back to a handle is a pure reinterpretation — no header check.
//! Handles are only produced by [`crate::Heap::alloc`]; indices
//! are heap-local, so comparing handles obtained from different `Heap`s is a
//! logic error no build mode detects.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;

/// Identifies one slot-storage space of the heap. Array order in `Heap` must
/// match this enum's declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Space {
    Objects,
    Strings,
    Symbols,
    Bigints,
    Shapes,
}

impl Space {
    /// Human-readable name for diagnostics and panic messages.
    pub fn name(self) -> &'static str {
        match self {
            Space::Objects => "object",
            Space::Strings => "string",
            Space::Symbols => "symbol",
            Space::Bigints => "bigint",
            Space::Shapes => "shape",
        }
    }

    /// Index into the `Heap`'s per-space arrays (`marked`, `alive`, `free`,
    /// …). Single conversion point for the enum-to-array-index cast; the
    /// array order in `Heap` must match this enum's declaration order.
    #[inline]
    pub fn as_index(self) -> usize {
        self as usize
    }
}

/// Marks a type as living in exactly one heap space.
pub trait HeapSpace: Sized {
    const SPACE: Space;
}

/// Typed index into a [`crate::Heap`] space. `Copy`, zero overhead; the type
/// parameter exists purely for space safety at compile time.
///
/// The payload is `PhantomData<fn() -> T>`: no drop glue, covariant-free,
/// and `Send`/`Sync` neutrality independent of `T`.
pub struct Handle<T> {
    index: u32,
    _space: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    pub(crate) fn new(index: u32) -> Self {
        Self {
            index,
            _space: PhantomData,
        }
    }

    /// The slot index within its space.
    pub fn index(self) -> u32 {
        self.index
    }

    /// The slot index as a storage-array index.
    ///
    /// `index()` stays `u32` because handle indices ride in the `JsValue`
    /// payload encoding; this is the single widening point for the many
    /// `slots[i]` accesses.
    #[inline]
    pub fn slot(self) -> usize {
        self.index as usize
    }
}

impl<T: HeapSpace> Handle<T> {
    /// The space this handle indexes.
    pub fn space() -> Space {
        T::SPACE
    }
}

// Manual impls: derives would impose `T: …` bounds that the representation
// does not need.

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl<T> Eq for Handle<T> {}

impl<T> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<T: HeapSpace> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handle<{}>({})", T::SPACE.name(), self.index)
    }
}
