//! Executable memory management for the baseline JIT.
//!
//! # Design
//!
//! The baseline JIT emits machine code that must be executable. Two strategies
//! exist:
//!
//! * **Development path (used in tests):** `cranelift-jit` owns a
//!   `JITModule` that allocates `mmap` pages internally, toggles them between
//!   writable and executable, and flushes the instruction cache. No embedder
//!   code calls `mprotect` directly.
//!
//! * **Production path (documented, not active in tests):** The engine
//!   `mmap`s a region with `PROT_READ | PROT_WRITE`, emits code, then
//!   toggles to `PROT_READ | PROT_EXEC` with `mprotect` on Linux. On
//!   macOS arm64 the region is created with `MAP_JIT` and the write
//!   protect is toggled with `pthread_jit_write_protect_np`. After each
//!   toggle the engine flushes the instruction cache with
//!   `__builtin___clear_cache` (Linux) or the equivalent `sys_icache_invalidate`
//!   on Darwin.
//!
//! # Safety
//!
//! Production toggling requires `unsafe` to call `mmap`, `mprotect`,
//! `pthread_jit_write_protect_np`, and `__builtin___clear_cache`. Those
//! call sites are the only `unsafe` in the crate and are isolated here.
//! Development builds avoid the toggle entirely by relying on
//! `cranelift-jit`, so this module currently contains no `unsafe` code.
//!
//! When the production path is enabled, each `unsafe` block will be
//! preceded by a `// SAFETY:` comment explaining why the raw pointer and
//! length are valid, why the protection transition is sound (W^X), and
//! why the cache flush covers exactly the emitted bytes.

// Clippy allowances for the production path are documented here so
// `cargo clippy --all-targets` stays clean when the feature is off.
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_safety_doc)]

/// Maximum size of a single JIT code region in bytes.
///
/// Chosen to keep a single `mmap` region below one page on most
/// targets while leaving headroom for typical baseline functions. Larger
/// functions are rejected with [`crate::JitError::TooLarge`] and fall back
/// to the interpreter.
pub const MAX_CODE_REGION_BYTES: usize = 64 * 1024;

/// Maximum number of bytes a single function may emit before it is
/// considered too large for the baseline tier.
pub const MAX_JIT_EMIT_BYTES: usize = 32 * 1024;

/// Documents the W^X toggle for the production path.
///
/// This function is a no-op in development builds. In production it would:
/// 1. `mprotect` the region to `PROT_READ | PROT_WRITE`.
/// 2. Copy the emitted bytes.
/// 3. `mprotect` the region to `PROT_READ | PROT_EXEC`.
/// 4. Flush the instruction cache.
pub fn wx_toggle_production_path_docs_only() {
    // No-op in the `cranelift-jit` development path.
}
