#![forbid(unsafe_code)]

//! Stub when `jit` feature disabled. Mirrors baseline stub.

use v12_bytecode::FunctionBytecode;
use v12_jit_baseline::{CompiledFn, FunctionId, JitError};

use crate::OptCompiler;

/// No codegen when disabled.
pub struct JitOpt;

impl JitOpt {
    pub fn new() -> Result<Self, JitError> {
        Ok(Self)
    }
    pub fn is_enabled(&self) -> bool {
        false
    }
    pub fn compile(
        &mut self,
        _fb: &FunctionBytecode,
        _id: FunctionId,
    ) -> Result<CompiledFn, JitError> {
        Err(JitError::Disabled)
    }
    pub fn clear(&mut self) {}
}

impl OptCompiler for JitOpt {
    fn compile(&mut self, fb: &FunctionBytecode, id: FunctionId) -> Result<CompiledFn, JitError> {
        JitOpt::compile(self, fb, id)
    }
}
