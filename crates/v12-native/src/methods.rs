//! Const method tables per object kind, declared with the [`builtin_methods!`]
//! macro.
//!
//! The interpreter's `get_property` recognizes built-in methods structurally
//! (there are no real prototype objects in v1). These tables replace the old
//! hand-written `key_is(key_v, "push")` chains: each kind's surface is one
//! `builtin_methods!` block, and [`lookup_method`] dispatches O(1):
//!
//! * the outer `match` on [`Kind`] is a jump table over the enum
//!   discriminants;
//! * the inner `match` on the method-name `&str` is compiled by rustc into a
//!   bounded switch (length + leading-byte dispatch over the literal arms) —
//!   a constant number of comparisons, not a linear scan over the table.
//!
//! Zero allocation, no runtime construction, no dependencies.


// The built-in method surface, declared at the kinds.
//
// These are the *pure* name→native bindings the interpreter's `get_property`
// used to recognize with hand-written `key_is` chains. Special-cased reads
// (property accessors like `length`/`source`, well-known-symbol keys,
// per-instance checks like promise `then`) stay as explicit branches in the
// interpreter.
crate::builtin_methods! {
    StringPrim => {
        match => StringMatch,
        replace => StringReplace,
        search => StringSearch,
        split => StringSplit,
    },
    Iterator => {
        next => IteratorNext,
    },
    RegExp => {
        exec => RegExpExec,
        test => RegExpTest,
        toString => RegExpToString,
        compile => RegExpCompile,
    },
    Array => {
        push => ArrayPush,
        pop => ArrayPop,
        join => ArrayJoin,
        slice => ArraySlice,
        sort => ArraySort,
        entries => ArrayIteratorEntries,
        keys => ArrayIteratorKeys,
        values => ArrayIterator,
    },
    Map => {
        get => MapGet,
        set => MapSet,
        has => MapHas,
        delete => MapDelete,
    },
    Set => {
        add => SetAdd,
        has => SetHas,
        delete => SetDelete,
    },
}

/// Declares the built-in method surface for the receiver kinds.
///
/// Each arm is `KindVariant => { name => Native, … }`. The macro expands to
/// [`lookup_method`] — an outer `match` over the kinds (a jump table), each
/// arm an inner `match` over the method-name literals. rustc lowers the
/// string arms to a bounded switch, so a lookup is O(1) — a constant number
/// of comparisons, never a linear scan. Adding a kind with methods means
/// adding one arm; the compiler enforces exhaustiveness.
///
/// ```rust
/// v12_native::builtin_methods! {
///     Array => {
///         push => ArrayPush,
///         pop => ArrayPop,
///     },
///     StringPrim => {
///         match => StringMatch,
///     },
/// }
/// ```
#[macro_export]
macro_rules! builtin_methods {
    ($( $kind:ident => { $( $method:ident => $native:ident ),* $(,)? } ),* $(,)?) => {
        /// Looks up the native for `name` on receiver kind `kind`.
        ///
        /// O(1): the outer `match` on `kind` is a jump table over the enum
        /// discriminants; each arm's inner `match` on `name` is compiled by
        /// rustc into a bounded switch over the literal arms (length +
        /// leading-byte dispatch). Returns `None` for kinds without a declared
        /// surface or names that are not methods on it.
        pub fn lookup_method(kind: v12_heap::Kind, name: &str) -> Option<$crate::NativeId> {
            match kind {
                $(
                    v12_heap::Kind::$kind => match name {
                        $( stringify!($method) => Some($crate::NativeId::$native), )*
                        _ => None,
                    },
                )*
                _ => None,
            }
        }
    };
}
