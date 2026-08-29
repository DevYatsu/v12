//! Error types and limits shared by the JIT tiers.

use v12_bytecode::Opcode;

/// Maximum number of bytecode instructions a function may contain before a
/// JIT tier refuses to compile it.
///
/// Large functions stay on the interpreter to bound JIT memory and compile
/// time. The execution driver falls back transparently.
pub const MAX_JIT_FUNCTION_SIZE: usize = 8192;

/// Maximum number of registers a JIT-compiled function may use.
///
/// The bytecode verifier already ensures `max_regs > 0`; this limit guards
/// the JIT's register-file handling.
pub const MAX_JIT_REGISTERS: usize = 512;

/// Reasons a function could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JitError {
    /// The `jit` cargo feature is disabled.
    Disabled,
    /// The function's bytecode is too large for the tier.
    TooLarge {
        /// Actual instruction count.
        len: usize,
        /// Allowed limit.
        limit: usize,
    },
    /// The function uses an opcode the tier does not yet support.
    UnsupportedOpcode(Opcode),
    /// The function uses a wide operation the tier does not yet support.
    UnsupportedWideOp(String),
    /// The bytecode is structurally invalid (e.g., truncated wide sequence).
    InvalidBytecode(String),
    /// Cranelift failed to build or verify the function.
    Cranelift(String),
}

impl std::fmt::Display for JitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "JIT disabled"),
            Self::TooLarge { len, limit } => {
                write!(f, "function too large for JIT: {len} > {limit}")
            }
            Self::UnsupportedOpcode(op) => {
                write!(f, "unsupported opcode for JIT: {op:?}")
            }
            Self::UnsupportedWideOp(msg) => {
                write!(f, "unsupported wide op for JIT: {msg}")
            }
            Self::InvalidBytecode(msg) => write!(f, "invalid bytecode: {msg}"),
            Self::Cranelift(msg) => write!(f, "cranelift error: {msg}"),
        }
    }
}

impl std::error::Error for JitError {}
