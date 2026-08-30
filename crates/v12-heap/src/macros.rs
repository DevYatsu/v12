//! The `assert_engine!` invariant-assertion macro.
//!
//! Engine-core hot paths validate their internal contracts (object shapes,
//! stack bounds, element tightness, handle liveness) at function entry/exit
//! in debug builds. The macro compiles out entirely in release builds — the
//! same contract as `debug_assert!` — but carries the "engine invariant"
//! prefix so a panic's provenance is unambiguous: it is an engine bug, not a
//! JS error, not a host error.

/// Asserts an engine-internal invariant in debug builds.
///
/// Compiles to nothing in release builds (like [`std::debug_assert`]). A
/// failed check panics with an `engine invariant:`-prefixed message so it is
/// never mistaken for a JS exception or a host error — per the project rule
/// "panics are engine bugs".
///
/// # Example
///
/// ```rust
/// use v12_heap::assert_engine;
///
/// let slots = 4;
/// assert_engine!(slots <= 8, "descriptor count within the inline cap");
/// assert_engine!(slots <= 8);
/// ```
#[macro_export]
macro_rules! assert_engine {
    ($cond:expr, $($arg:tt)+) => {
        if cfg!(debug_assertions) && !($cond) {
            panic!(
                "engine invariant: {}: {}",
                stringify!($cond),
                format_args!($($arg)+)
            );
        }
    };
    ($cond:expr) => {
        if cfg!(debug_assertions) && !($cond) {
            panic!("engine invariant: {}", stringify!($cond));
        }
    };
}
