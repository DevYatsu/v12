//! Shared dev-only test harness for the v12 workspace.
//!
//! Not shipped: dev-dependency of `v12-bccompiler`, `v12-interp`, and
//! `v12-bytecode` test binaries. Depends on the real compiler so callers
//! get `eval_src`-style source-in, value-out helpers.

pub mod mini;
pub use mini::*;
