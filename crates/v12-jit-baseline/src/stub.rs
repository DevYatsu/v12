//! Stub implementation when the `jit` feature is disabled.
//!
//! All compilation attempts report `JitError::Disabled` without pulling in
//! Cranelift.

use v12_bytecode::FunctionBytecode;

use crate::cache::{CompiledFn, JitCache};
use crate::error::JitError;

/// Baseline JIT stub — no code generation when the feature is off.
pub struct JitBaseline {
    cache: JitCache,
}

impl JitBaseline {
    /// Creates a stub JIT. Always succeeds, even though compilation will
    /// later report disabled.
    pub fn new() -> Result<Self, JitError> {
        Ok(Self {
            cache: JitCache::new(),
        })
    }

    /// Attempts to compile `bytecode` but always reports `Disabled`.
    pub fn compile(&mut self, _bytecode: &FunctionBytecode) -> Result<CompiledFn, JitError> {
        Err(JitError::Disabled)
    }

    /// Borrows the cache.
    pub fn cache(&self) -> &JitCache {
        &self.cache
    }

    /// Mutably borrows the cache.
    pub fn cache_mut(&mut self) -> &mut JitCache {
        &mut self.cache
    }
}
