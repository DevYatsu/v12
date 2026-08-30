//! Structured error type for the bytecode crate.
//!
//! The ISA is pure data: it never panics on malformed input and never returns
//! bare `String` errors. Every failure mode is a named [`BytecodeError`]
//! variant so callers (the JIT tiers, the compiler, the interpreter's
//! cross-tier validation) can distinguish corrupt bytecode from an
//! out-of-range constant from a structural-invariant violation without
//! string matching.

/// A malformed or out-of-contract bytecode stream.
///
/// The interpreter's *internal* dispatch may still panic on invariant
/// violations ("panics are engine bugs" — the stream it runs is
/// compiler-emitted, not host input). This type covers the boundaries where
/// bytecode crosses a subsystem: `WideOp::try_decode`, `ConstantPool::insert`,
/// `FunctionBytecode::validate`, and the JIT tiers' structural checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BytecodeError {
    /// A `Wide`-prefixed sequence was truncated or missing a header word.
    TruncatedWide {
        /// Zero-based payload word index that was missing.
        word: usize,
    },
    /// A `Wide` header carried an opcode other than `Opcode::Wide`.
    WideHeaderNotWide,
    /// A `Wide` header carried an unknown discriminant byte.
    UnknownWideDiscriminant {
        /// The discriminant value from the header's `c` slot.
        discriminant: u32,
    },
    /// The constant pool reached its [`crate::MAX_CONSTANTS`] capacity.
    ConstantPoolFull,
    /// A `FunctionBytecode` failed its structural validation.
    InvalidFunction {
        /// Human-readable reason, matching the pre-structured messages so
        /// callers that stringify (compiler errors, disassembler) keep their
        /// exact output.
        reason: String,
    },
    /// `max_regs` was zero (registers index from 0, so a frame is empty).
    ZeroMaxRegs,
    /// A handler target pc fell outside the instruction stream.
    HandlerTargetOutOfBounds {
        /// The offending target pc.
        target: u32,
        /// Number of instructions in the function.
        instrs: usize,
    },
}

impl std::fmt::Display for BytecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TruncatedWide { word } => {
                write!(f, "wide op: missing payload word {word}")
            }
            Self::WideHeaderNotWide => {
                write!(f, "wide op: header opcode is not Wide")
            }
            Self::UnknownWideDiscriminant { discriminant } => {
                write!(f, "wide op: unknown discriminant {discriminant:#x}")
            }
            Self::ConstantPoolFull => write!(f, "constant pool full"),
            Self::InvalidFunction { reason } => write!(f, "{reason}"),
            Self::ZeroMaxRegs => write!(f, "max_regs must be greater than zero"),
            Self::HandlerTargetOutOfBounds { target, instrs } => {
                write!(
                    f,
                    "handler target {target} is out of bounds ({instrs} instrs)"
                )
            }
        }
    }
}

impl std::error::Error for BytecodeError {}
