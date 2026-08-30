//! Unified native dispatch for the v12 engine.
//!
//! This crate is the single source of truth for the native-function surface
//! shared by the interpreter (`v12-interp`) and the engine (`v12-engine`):
//!
//! * [`NativeId`] — one collision-proof enum for every native function index
//!   (the three index spaces that used to live in `v12-engine` constants,
//!   duplicated `v12-interp` constants, and the interp's internal `NativeFn`
//!   fallback enum collapse into this one type).
//! * [`Throw`] — the error type for native handlers (a distinct type from the
//!   success `JsValue`, so a handler reads "produce a value or throw one").
//! * [`NativeSig`] — a trait implemented for argument tuples; the dispatch
//!   converts `&[JsValue]` into the tuple's Rust types automatically.
//! * std [`From`]/[`TryFrom`] conversions between [`JsValue`] and Rust types
//!   (zero-alloc only; heap-dependent conversions are explicit [`Heap`]
//!   methods).
//! * [`KindMethods`] — a const method table per object [`Kind`], declared via
//!   the [`builtin_methods!`] macro.
//!
//! Dependency direction: `v12-heap ← v12-native ← {v12-interp, v12-engine}`.

mod convert;
mod id;
mod methods;
mod registry;
mod sig;
mod table;
mod throw;

pub use convert::DecodeError;
pub use id::{NativeId, UnknownNativeId};
pub use methods::{BUILTIN_METHODS, KindMethods, Method, lookup_method};
pub use registry::{EmptyNativeRegistry, Handler, NativeRegistry, ProgramTable, RuntimeRegistry};
pub use sig::NativeSig;
pub use throw::Throw;

// Re-exported for the macros and downstream users.
pub use v12_heap::{Heap, JsValue, Kind};
