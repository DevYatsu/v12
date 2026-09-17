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
        charAt => StringCharAt,
        charCodeAt => StringCharCodeAt,
        codePointAt => StringCodePointAt,
        at => StringAt,
        slice => StringSlice,
        substring => StringSubstring,
        substr => StringSubstr,
        indexOf => StringIndexOf,
        lastIndexOf => StringLastIndexOf,
        includes => StringIncludes,
        startsWith => StringStartsWith,
        endsWith => StringEndsWith,
        concat => StringConcat,
        repeat => StringRepeat,
        padStart => StringPadStart,
        padEnd => StringPadEnd,
        trim => StringTrim,
        trimStart => StringTrimStart,
        trimEnd => StringTrimEnd,
        toLowerCase => StringToLowerCase,
        toUpperCase => StringToUpperCase,
        toString => StringToString,
        valueOf => StringValueOf,
        localeCompare => StringLocaleCompare,
        replaceAll => StringReplaceAll,
        matchAll => StringMatchAll,
        toWellFormed => StringToWellFormed,
        isWellFormed => StringIsWellFormed,
        trimLeft => StringTrimLeft,
        trimRight => StringTrimRight,
        toLocaleLowerCase => StringToLocaleLowerCase,
        toLocaleUpperCase => StringToLocaleUpperCase,
        anchor => StringAnchor,
        big => StringBig,
        blink => StringBlink,
        bold => StringBold,
        fixed => StringFixed,
        fontcolor => StringFontcolor,
        fontsize => StringFontsize,
        italics => StringItalics,
        link => StringLink,
        small => StringSmall,
        strike => StringStrike,
        sub => StringSub,
        sup => StringSup,
    },
    NumberPrim => {
        toString => NumberProtoToString,
        toFixed => NumberToFixed,
        toPrecision => NumberToPrecision,
        toExponential => NumberToExponential,
        valueOf => NumberProtoValueOf,
    },
    BooleanPrim => {
        toString => BooleanProtoToString,
        valueOf => BooleanProtoValueOf,
    },
    Iterator => {
        next => IteratorNext,
        map => IteratorMap,
        filter => IteratorFilter,
        take => IteratorTake,
        drop => IteratorDrop,
        flatMap => IteratorFlatMap,
        reduce => IteratorReduce,
        toArray => IteratorToArray,
        forEach => IteratorForEach,
        some => IteratorSome,
        every => IteratorEvery,
        find => IteratorFind,
    },
    SymbolPrim => {
        toString => SymbolProtoToString,
        valueOf => SymbolProtoValueOf,
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
        // `sort` is the live install path (the interpreter's
        // `array_instance_surface` resolves it via this table): JS calls
        // dispatch to the interpreter's comparator-capable
        // `array_sort_callback` seam first; the engine registry function is
        // only the non-callback fallback for direct registry calls.
        sort => ArraySort,
        entries => ArrayIteratorEntries,
        keys => ArrayIteratorKeys,
        values => ArrayIterator,
        indexOf => ArrayIndexOf,
        lastIndexOf => ArrayLastIndexOf,
        includes => ArrayIncludes,
        concat => ArrayConcat,
        at => ArrayAt,
        reverse => ArrayReverse,
        shift => ArrayShift,
        unshift => ArrayUnshift,
        splice => ArraySplice,
        fill => ArrayFill,
        flat => ArrayFlat,
        copyWithin => ArrayCopyWithin,
        toString => ArrayToString,
        forEach => ArrayForEach,
        map => ArrayMap,
        filter => ArrayFilter,
        some => ArraySome,
        every => ArrayEvery,
        find => ArrayFind,
        findIndex => ArrayFindIndex,
        findLast => ArrayFindLast,
        findLastIndex => ArrayFindLastIndex,
        reduce => ArrayReduce,
        reduceRight => ArrayReduceRight,
        flatMap => ArrayFlatMap,
    },
    Map => {
        get => MapGet,
        set => MapSet,
        has => MapHas,
        delete => MapDelete,
        clear => MapClear,
        forEach => MapForEach,
        entries => MapEntries,
        keys => MapKeys,
        values => MapValues,
    },
    Set => {
        add => SetAdd,
        has => SetHas,
        delete => SetDelete,
        clear => SetClear,
        forEach => SetForEach,
        entries => SetEntries,
        keys => SetKeys,
        values => SetValues,
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
