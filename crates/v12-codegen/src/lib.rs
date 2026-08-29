//! Shared types and seams for the v12 JIT tiers (ADR-006).
//!
//! The two JIT crates (`v12-jit-baseline` and `v12-jit-opt`) used to depend
//! on each other (`v12-jit-opt` imported baseline types directly), which
//! inverts the dependency direction: a higher tier depending on a lower
//! one. The plan calls out:
//!
//! > Extract a `v12-codegen` (or `v12-jit-core`) with the shared seams both
//! > tiers need: deopt map, guard emission, executable-memory region,
//! > tier-up hooks. Both `v12-jit-baseline` and `v12-jit-opt` depend on
//! > `v12-codegen`, **not** on each other.
//!
//! This crate owns those shared seams. The Tier-1 implementation
//! ([`v12-jit-baseline`]) compiles bytecode to Cranelift IR with template
//! lowering. The Tier-2 implementation ([`v12-jit-opt`]) reuses the same
//! types for speculation, guards, and deopt. Both crates depend on this
//! crate; neither depends on the other.
//!
//! # Stability
//!
//! Public surface is minimal: the types both tiers need to share. New
//! shared machinery (e.g. a `TierPolicy` for tier-up thresholds) lives
//! here, not in either tier crate.
//!
//! # What's *not* here yet
//!
//! This is the v1 extraction: the shared types and module shells exist; the
//! actual `DeoptMap`, `GuardKind`, and `TierPolicy` types still live in
//! `v12-jit-opt`/`v12-jit-baseline` and will be moved here incrementally.
//! The dependency direction is what ADR-006 needs, and the migration
//! pathway is one mechanical move at a time.

#![forbid(unsafe_code)]

pub mod cache;
pub mod error;

pub use cache::{CompiledFn, FunctionId, JitCache, JitExecFn};
pub use error::{JitError, MAX_JIT_FUNCTION_SIZE, MAX_JIT_REGISTERS};

pub use v12_bytecode::{Const, FunctionBytecode, Instr, Opcode, WideOp};
pub use v12_heap::{
    Attrs, Descriptor, GcPolicy, Handle, Heap, JsObject, JsValue, PropKey, ShapeHandle,
    V12Str,
};
pub use v12_interp::{Interp, JSException, NativeRegistry, EmptyNativeRegistry};

/// Tier-up policy: when a function crosses the threshold, the engine asks
/// the configured tier (baseline → opt) to recompile it. v1 ships a stub
/// policy: every recompile is opt-in via [`TierPolicy::OnDemand`]; the
/// engine exposes hooks for future profiling-driven tier-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TierPolicy {
    /// Functions recompile only when the host explicitly asks (default).
    #[default]
    OnDemand,
    /// Reserved for v2: profile-driven, off by default.
    Profile,
}

/// Trait implemented by every tier to recompile a single bytecode function.
///
/// v1 has two implementors: the baseline template JIT (`JitBaseline`) and
/// the speculative opt JIT (`JitOpt`). The engine owns a `Vec<Box<dyn
/// TierCompiler>>` ordered by tier and calls them in sequence when a
/// `tier_up_pending` flag fires.
pub trait TierCompiler {
    /// Recompile `fb` into native code. The result is opaque; the engine
    /// hands it back to the next tier or to the runtime for patching.
    fn compile(
        &mut self,
        fb: &FunctionBytecode,
    ) -> Result<Box<dyn std::fmt::Debug + Send>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_policy_default_is_on_demand() {
        assert_eq!(TierPolicy::default(), TierPolicy::OnDemand);
    }

    #[test]
    fn bytecode_types_re_exported() {
        // Smoke test: the shared types are accessible from this crate so
        // both tiers can consume them without depending on each other.
        let _ = std::any::type_name::<Opcode>();
        let _ = std::any::type_name::<FunctionBytecode>();
    }
}
