//! Re-exports the std [`From`]/[`TryFrom`] conversions between [`JsValue`]
//! and Rust types.
//!
//! The impls themselves live in `v12-heap`, next to [`JsValue`], because the
//! orphan rule requires a std trait impl for `JsValue` (a foreign type here)
//! to live in the crate that defines it. This module re-exports the types and
//! documents the boundary:
//!
//! * JS → Rust: `impl TryFrom<JsValue> for T` with `Error = DecodeError`
//!   (`f64`, `i32`, `bool`, `Handle<V12Str>`, `Handle<JsObject>`, …).
//! * Rust → JS: `impl From<T> for JsValue` (`f64`, `i32`, `bool`,
//!   `Handle<V12Str>`, `Handle<JsObject>`, `()`).
//!
//! Only **zero-alloc** conversions fit std `From`/`TryFrom` (no heap
//! parameter). Heap-dependent conversions — owned `String` *content* (ropes
//! must be flattened), interning, building objects — are explicit
//! [`Heap`](v12_heap::Heap) methods (`Heap::intern_text`,
//! `Heap::string_content`, …), keeping allocation visible at the call site.

pub use v12_heap::DecodeError;
