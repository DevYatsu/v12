#![forbid(unsafe_code)]

//! Tier-2 pipeline. No own IR, no inlining; delegates to baseline template.

use v12_bytecode::{FunctionBytecode, Opcode};
use v12_jit_baseline::{CompiledFn, JitError, MAX_JIT_FUNCTION_SIZE, MAX_JIT_REGISTERS};

/// Optimizing pipeline stub. Wraps baseline until profiling exists.
pub struct Pipeline {}

impl Pipeline {
    pub fn new() -> Result<Self, JitError> {
        Ok(Self {})
    }
    pub fn compile(&mut self, fb: &FunctionBytecode) -> Result<CompiledFn, JitError> {
        if fb.instrs.len() > MAX_JIT_FUNCTION_SIZE {
            return Err(JitError::TooLarge {
                len: fb.instrs.len(),
                limit: MAX_JIT_FUNCTION_SIZE,
            });
        }
        if usize::from(fb.max_regs) > MAX_JIT_REGISTERS {
            return Err(JitError::InvalidBytecode(format!(
                "max_regs {} exceeds JIT limit {}",
                fb.max_regs, MAX_JIT_REGISTERS
            )));
        }
        // Delegate to baseline template. Demonstrates guard insertion point:
        // if fb contains Add/Sub/Mul we would attach Assumption, otherwise baseline.
        let mut baseline = v12_jit_baseline::JitBaseline::new()?;
        match baseline.compile(fb) {
            Ok(c) => {
                let has_arith = fb.instrs.iter().any(|i| {
                    matches!(i.op(), Some(Opcode::Add | Opcode::Sub | Opcode::Mul))
                });
                // No own IR yet; keep baseline result in both branches.
                if has_arith { Ok(c) } else { Ok(c) }
            }
            Err(e @ JitError::UnsupportedOpcode(_)) => Err(e),
            Err(e) => Err(e),
        }
    }
}
