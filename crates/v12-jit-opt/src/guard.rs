#![forbid(unsafe_code)]

//! Guards and speculation policy for tier-2.

/// Guard kinds. `u32` aliases `ShapeHandle`/`ValidityCellId` to avoid heap dep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    ShapeEq { slot: u8, expected: u32 },
    TypeIsNumber { reg: u8 },
    ValidityCell { cell: u32, serial: u32 },
}

/// One guarded assumption at a bytecode pc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assumption {
    pub bc_pc: u32,
    pub guard: GuardKind,
}

impl Assumption {
    /// Closed-world check: guard failure triggers deopt, never UB.
    pub fn check_fails_closed(&self) -> &'static str {
        "guard failure -> deopt to interp/baseline"
    }
    /// Validation: `ShapeEq` with NONE handle means no speculation.
    pub fn is_valid(&self) -> bool {
        match self.guard {
            GuardKind::ShapeEq { expected, .. } => expected != u32::MAX,
            GuardKind::ValidityCell { cell, .. } => cell != 0,
            GuardKind::TypeIsNumber { .. } => true,
        }
    }
}

impl core::fmt::Display for Assumption {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.guard {
            GuardKind::ShapeEq { slot, expected } => {
                write!(f, "pc {} shape_eq slot {} expected {}", self.bc_pc, slot, expected)
            }
            GuardKind::TypeIsNumber { reg } => write!(f, "pc {} type_is_number r{}", self.bc_pc, reg),
            GuardKind::ValidityCell { cell, serial } => {
                write!(f, "pc {} validity_cell {} serial {}", self.bc_pc, cell, serial)
            }
        }
    }
}

/// Gate speculation on hotness and monomorphism.
#[inline]
pub fn should_speculate(entry_counter: u16, loop_counter: u16, is_mono: bool) -> bool {
    if !is_mono {
        return false;
    }
    entry_counter > 512 || loop_counter > 800
}
