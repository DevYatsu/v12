//! Structured error type for the bytecode crate.
//!
//! The ISA is pure data: it never panics on malformed input and never returns
//! bare `String` errors. Every failure mode is a named [`BytecodeError`]
//! variant so callers (the JIT tiers, the compiler, the interpreter's
//! cross-tier validation) can distinguish corrupt bytecode from an
//! out-of-range constant from a structural-invariant violation without
//! string matching.

use crate::Opcode;

/// A malformed or out-of-contract bytecode stream.
///
/// The interpreter's *internal* dispatch may still panic on invariant
/// violations ("panics are engine bugs" — the stream it runs is
/// compiler-emitted, not host input). This type covers the boundaries where
/// bytecode crosses a subsystem: `WideOp::try_decode`, `ConstantPool::insert`,
/// `FunctionBytecode::validate`, and the JIT tiers' structural checks.
///
/// The operand-contract variants (`UnassignedOpcode` through
/// `FlagNotBoolean`) are what make `opspec.rs` load-bearing: each one is a
/// rejection `FunctionBytecode::validate` can derive from the [`crate::Role`]
/// table alone, and each replaces a *panic* or an out-of-bounds read in the
/// interpreter's dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BytecodeError {
    /// A `Wide`-prefixed sequence was truncated or missing a header word, or a
    /// `RegExt` prefix had no narrow instruction to extend.
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
    /// A word at an instruction boundary carried an unassigned opcode byte.
    ///
    /// `Instr::op` decodes it to `None`; every dispatcher treats that as
    /// corrupt bytecode and panics, so rejecting it here is the difference
    /// between a `Result` and a panic.
    UnassignedOpcode {
        /// Word index of the offending instruction.
        pc: u32,
        /// The raw opcode byte.
        byte: u8,
    },
    /// A register operand named a register at or past the frame's register
    /// file (`max_regs`).
    ///
    /// Reachable for a positional operand too — a packed `(getter, setter)`
    /// pair or a `NewArray` element window extends past its base register —
    /// which is why `operand` is a rendered description rather than a bare
    /// role name.
    RegisterOutOfRange {
        /// Word index of the offending instruction.
        pc: u32,
        /// The opcode whose operand is out of range.
        op: Opcode,
        /// The operand as `opspec::Role` names it, or the positional
        /// expression it came from (e.g. `"pair base + 1"`).
        operand: String,
        /// The register index the operand resolved to.
        reg: u32,
        /// The frame's declared register count.
        max_regs: u32,
    },
    /// `GetEnvSlot`/`SetEnvSlot` named a slot at or past the size of an
    /// environment **this function allocated**.
    ///
    /// Environments inherited from the closure's capture point are not in a
    /// `FunctionBytecode`, so a depth naming one is not checked at all — see
    /// the module docs in `validate.rs` for why no bound is derivable for it.
    EnvSlotOutOfRange {
        /// Word index of the offending instruction.
        pc: u32,
        /// The opcode whose operand is out of range.
        op: Opcode,
        /// The environment depth the access names.
        depth: u32,
        /// The slot index within that environment.
        slot: u32,
        /// Slots the environment was allocated with.
        env_slots: u32,
    },
    /// `LoadConst` named a constant-pool entry the function does not have.
    ///
    /// `Interp::const_value` panics on this, so the check is a panic turned
    /// into a `Result`.
    ConstIdOutOfRange {
        /// Word index of the offending instruction.
        pc: u32,
        /// The opcode whose operand is out of range.
        op: Opcode,
        /// The constant id the operand resolved to.
        id: u32,
        /// The function's constant-pool length.
        pool: usize,
    },
    /// A branch target past the end of the instruction stream.
    ///
    /// A target *equal* to `instrs` is legal: it is the implicit
    /// `return undefined` completion point.
    BranchTargetOutOfRange {
        /// Word index of the offending instruction.
        pc: u32,
        /// The branching opcode.
        op: Opcode,
        /// The resolved target pc.
        target: u32,
        /// Number of instruction words in the function.
        instrs: usize,
    },
    /// A `Role::Flag` operand that is neither `0` nor `1`.
    ///
    /// The flag is a behavioural switch (`IteratorClose`'s completion path,
    /// `Await`'s async-from-sync form), read as `rb == 0` / `rc == 1`, so any
    /// other value silently takes one of the two paths without being one.
    FlagNotBoolean {
        /// Word index of the offending instruction.
        pc: u32,
        /// The opcode whose operand is out of contract.
        op: Opcode,
        /// The value found in the flag slot.
        value: u8,
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
            Self::UnassignedOpcode { pc, byte } => {
                write!(f, "unassigned opcode byte {byte:#04x} at pc {pc}")
            }
            Self::RegisterOutOfRange {
                pc,
                op,
                operand,
                reg,
                max_regs,
            } => write!(
                f,
                "{op:?} at pc {pc}: {operand} names register {reg}, outside the \
                 {max_regs}-register frame"
            ),
            Self::EnvSlotOutOfRange {
                pc,
                op,
                depth,
                slot,
                env_slots,
            } => write!(
                f,
                "{op:?} at pc {pc}: slot {slot} is outside the {env_slots}-slot \
                 environment at depth {depth}"
            ),
            Self::ConstIdOutOfRange { pc, op, id, pool } => write!(
                f,
                "{op:?} at pc {pc}: constant {id} is outside the {pool}-entry pool"
            ),
            Self::BranchTargetOutOfRange {
                pc,
                op,
                target,
                instrs,
            } => write!(
                f,
                "{op:?} at pc {pc}: target {target} is past the {instrs}-word stream"
            ),
            Self::FlagNotBoolean { pc, op, value } => {
                write!(f, "{op:?} at pc {pc}: flag {value} is neither 0 nor 1")
            }
        }
    }
}

impl std::error::Error for BytecodeError {}
