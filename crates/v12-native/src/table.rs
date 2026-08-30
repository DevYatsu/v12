//! Compile-time builtin dispatch: the [`native_table!`] macro.
//!
//! Builtins known at compile time are dispatched through a `match` over
//! [`NativeId`] — the compiler emits a jump table. There is no lookup
//! structure for them at runtime: adding a builtin is one line in the table,
//! and the dispatch match grows at compile time.

/// Declares the compile-time builtin table.
///
/// Expands to `builtin_dispatch`, a `match` over [`NativeId`] — the compiler
/// turns it into a jump table. No array, no `HashMap`, no runtime
/// construction: the builtin set is fixed in source and dispatch is compiled
/// code.
///
/// Entries are `Handler` fn pointers. A typed entry is wrapped at the table
/// via the [`typed_wrapper!`](crate::typed_wrapper) helper:
/// `NativeId::X => typed_wrapper!(path::to::handler)` expands to a
/// non-capturing closure coerced to `Handler`, so the wrap is inlined into
/// the match arm — still compile-time, still a plain fn pointer.
///
/// The macro is instantiated where the handler implementations live (the
/// engine), so `v12-native` has no dependency on the builtins.
#[macro_export]
macro_rules! native_table {
    ($( $id:ident => $handler:expr ),* $(,)?) => {
        /// Compile-time dispatch over every builtin. `None` means "not a
        /// builtin" — the caller falls through to the runtime registry.
        pub fn builtin_dispatch(
            id: $crate::NativeId,
            heap: &mut $crate::Heap,
            this: $crate::JsValue,
            args: &[$crate::JsValue],
        ) -> Option<Result<$crate::JsValue, $crate::Throw>> {
            match id {
                $( $crate::NativeId::$id => Some(($handler)(heap, this, args)), )*
                _ => None,
            }
        }
    };
}
