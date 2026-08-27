#![forbid(unsafe_code)]

//! Tier-2 speculative optimizing JIT over Cranelift (post-v1 milestone).
//!
//! Baseline template is reused; speculation adds monomorphic guards and
//! deoptimizes to interpreter/baseline on failure.
// ponytail: mono shape/type guard + deopt to interp/baseline. Upgrade when profile >5% in GetProperty/Add -> poly IC, loop versioning, OSR, inlining.

pub use v12_bytecode::{FunctionBytecode, PcMapEntry};
pub use v12_jit_baseline::{
    CompiledFn, FunctionId, JitCache, JitError, MAX_JIT_FUNCTION_SIZE, MAX_JIT_REGISTERS,
};

mod deopt;
mod guard;
#[cfg(feature = "jit")]
mod compile;
#[cfg(not(feature = "jit"))]
mod stub;

pub use deopt::DeoptMap;
pub use guard::{Assumption, GuardKind};
#[cfg(not(feature = "jit"))]
pub use stub::JitOpt;

/// Tier-2 compiler trait. One method to keep surface minimal.
pub trait OptCompiler {
    fn compile(&mut self, fb: &FunctionBytecode, id: FunctionId) -> Result<CompiledFn, JitError>;
}

#[cfg(feature = "jit")]
/// Speculative optimizer. Delegates to baseline template until profiling gates fire.
pub struct JitOpt {
    inner: Option<compile::Pipeline>,
}

#[cfg(feature = "jit")]
impl JitOpt {
    pub fn new() -> Result<Self, JitError> {
        Ok(Self {
            inner: Some(compile::Pipeline::new()?),
        })
    }
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }
    pub fn compile(&mut self, fb: &FunctionBytecode, _id: FunctionId) -> Result<CompiledFn, JitError> {
        match &mut self.inner {
            Some(p) => p.compile(fb),
            None => Err(JitError::Disabled),
        }
    }
    pub fn clear(&mut self) {
        self.inner = None;
    }
}

#[cfg(feature = "jit")]
impl OptCompiler for JitOpt {
    fn compile(&mut self, fb: &FunctionBytecode, id: FunctionId) -> Result<CompiledFn, JitError> {
        JitOpt::compile(self, fb, id)
    }
}
