#![forbid(unsafe_code)]
#![allow(dead_code)]

//! Deoptimization map. Reuses baseline `PcMapEntry` 1:1 block mapping.
//!
//! On guard failure the runtime jumps to a trampoline that materializes the
//! interpreter frame: `regs[..max_regs]` are copied and `interp.set_pc` is
//! set to the recorded `bc_pc`. The trampoline is a Cranelift block that
//! calls the `deopt` helper; here we only track the mapping.
//!
//! The map also records every [`Assumption`] emitted for the function so
//! the optimizer can bound code size via [`MAX_GUARDS_PER_FUNCTION`].
//!
//! Loop versioning records a `ValidityCell` assumption at the peeled header
//! so that a later prototype mutation invalidates the fast loop.

use v12_bytecode::PcMapEntry;

use crate::guard::{Assumption, GuardKind, MAX_GUARDS_PER_FUNCTION};

// ---------------------------------------------------------------------------
// Validity-cell registry — loop versioning support (Oracle 1, finding 5)
// ---------------------------------------------------------------------------

/// Registry for validity-cell serials observed during optimization.
///
/// Real cells live on `Heap` (`ValidityCellId` + serial). `LoopValidity`
/// is a lightweight snapshot used by the optimizer to emit a `ValidityCell`
/// guard for a peeled loop and to test invalidation without holding a
/// `Heap`.
///
///
/// ```text
/// // loop peeled at pc 3 with cell 7 serial 42
/// let mut cells = ValidityRegistry::default();
/// cells.observe(7, 42);
/// let guard = cells.guard_for(3, 7).unwrap();
/// assert!(guard.check_validity_cell(42));
/// assert!(!guard.check_validity_cell(43)); // mutated
/// ```
#[derive(Debug, Default, Clone)]
pub struct ValidityRegistry {
    cells: std::collections::HashMap<u32, u32>,
}

impl ValidityRegistry {
    /// Records that `cell` was observed at `serial`.
    pub fn observe(&mut self, cell: u32, serial: u32) {
        self.cells.insert(cell, serial);
    }

    /// Current serial for `cell`, if observed.
    pub fn serial_for(&self, cell: u32) -> Option<u32> {
        self.cells.get(&cell).copied()
    }

    /// Builds a `ValidityCell` assumption at `bc_pc` for `cell` with its
    /// observed serial. Returns `None` if the cell is `0` (`NONE`) or has
    /// not been observed.
    pub fn guard_for(&self, bc_pc: u32, cell: u32) -> Option<Assumption> {
        let serial = self.serial_for(cell)?;
        if cell == 0 {
            return None;
        }
        Some(Assumption {
            bc_pc,
            guard: GuardKind::ValidityCell { cell, serial },
        })
    }

    /// Whether `cell`'s serial matches the registry's expectation.
    pub fn is_still_valid(&self, cell: u32, current_serial: u32) -> bool {
        self.serial_for(cell).is_some_and(|s| s == current_serial)
    }
}

/// Maps JIT pcs to bytecode pcs. Deopt materialization = `regs[..max_regs]` copy + interp `set_pc`.
#[derive(Debug, Clone, Default)]
pub struct DeoptMap {
    pc_map: Vec<PcMapEntry>,
    live_regs: Vec<u8>,
    guards: Vec<Assumption>,
    /// Validity cells observed for loop versioning (snapshot).
    validity: ValidityRegistry,
}

impl DeoptMap {
    pub fn from_pc_map(pc_map: Vec<PcMapEntry>) -> Self {
        Self {
            pc_map,
            live_regs: Vec::new(),
            guards: Vec::new(),
            validity: ValidityRegistry::default(),
        }
    }
    pub fn with_live_regs(mut self, regs: Vec<u8>) -> Self {
        self.live_regs = regs;
        self
    }
    pub fn lookup(&self, bc_pc: u32) -> Option<usize> {
        self.pc_map.iter().position(|e| e.bc_pc == bc_pc)
    }
    pub fn deopt_pc(&self, jit_pc: u32) -> Option<u32> {
        self.pc_map
            .iter()
            .find(|e| e.jit_pc == jit_pc)
            .map(|e| e.bc_pc)
    }
    pub fn pc_map(&self) -> &[PcMapEntry] {
        &self.pc_map
    }
    pub fn live_regs(&self) -> &[u8] {
        &self.live_regs
    }

    /// Records a guard assumption. Returns `false` when the per-function guard
    /// budget is exhausted; the caller should fall back to unspecialized code.
    pub fn record_guard(&mut self, assumption: Assumption) -> bool {
        if self.guards.len() >= MAX_GUARDS_PER_FUNCTION {
            return false;
        }
        self.guards.push(assumption);
        true
    }

    /// Records a `ValidityCell` guard for a loop peel at `bc_pc`.
    ///
    /// The validity registry must have observed `cell`/`serial` beforehand
    /// (e.g., via `observe_validity`). Returns `false` when budget exhausted
    /// or cell is `0`.
    pub fn record_validity_guard(&mut self, bc_pc: u32, cell: u32, serial: u32) -> bool {
        if cell == 0 {
            return false;
        }
        self.validity.observe(cell, serial);
        let assumption = Assumption {
            bc_pc,
            guard: GuardKind::ValidityCell { cell, serial },
        };
        self.record_guard(assumption)
    }

    /// Observes a validity cell without emitting a guard (for deferred emission).
    pub fn observe_validity(&mut self, cell: u32, serial: u32) {
        self.validity.observe(cell, serial);
    }

    /// All recorded guards in emission order.
    pub fn guards(&self) -> &[Assumption] {
        &self.guards
    }

    /// Number of recorded guards.
    pub fn guard_count(&self) -> usize {
        self.guards.len()
    }

    /// Borrows the validity registry (loop versioning).
    pub fn validity(&self) -> &ValidityRegistry {
        &self.validity
    }
}

/// Valid deopt target exists in map.
#[allow(dead_code)]
#[inline]
pub fn is_valid_deopt(pc_map: &[PcMapEntry], bc_pc: u32) -> bool {
    pc_map.iter().any(|e| e.bc_pc == bc_pc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::GuardKind;
    use v12_heap::Heap;

    #[test]
    fn record_guard_respects_limit() {
        let mut map = DeoptMap::default();
        for i in 0..MAX_GUARDS_PER_FUNCTION {
            let ok = map.record_guard(Assumption {
                bc_pc: i as u32,
                guard: GuardKind::TypeIsSmi { reg: 0 },
            });
            assert!(ok, "guard {i} should fit");
        }
        let overflow = map.record_guard(Assumption {
            bc_pc: 999,
            guard: GuardKind::TypeIsSmi { reg: 0 },
        });
        assert!(!overflow, "overflow guard must be rejected");
        assert_eq!(map.guard_count(), MAX_GUARDS_PER_FUNCTION);
    }

    #[test]
    fn pc_map_helpers() {
        let entries = vec![
            PcMapEntry {
                jit_pc: 0,
                bc_pc: 0,
            },
            PcMapEntry {
                jit_pc: 10,
                bc_pc: 1,
            },
            PcMapEntry {
                jit_pc: 20,
                bc_pc: 2,
            },
        ];
        let map = DeoptMap::from_pc_map(entries.clone());
        assert_eq!(map.lookup(1), Some(1));
        assert_eq!(map.deopt_pc(10), Some(1));
        assert!(is_valid_deopt(&entries, 2));
        assert!(!is_valid_deopt(&entries, 99));
    }

    #[test]
    fn shape_guard_roundtrip_via_deopt_map() {
        let heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let shape = heap.root_shape();
        let mut map = DeoptMap::from_pc_map(vec![PcMapEntry {
            jit_pc: 0,
            bc_pc: 5,
        }]);
        let ok = map.record_guard(Assumption {
            bc_pc: 5,
            guard: GuardKind::ShapeEq { expected: shape },
        });
        assert!(ok);
        assert_eq!(map.guards()[0].bc_pc, 5);
    }

    #[test]
    fn validity_cell_guard_for_loop_peel() {
        let mut map = DeoptMap::from_pc_map(vec![PcMapEntry {
            jit_pc: 0,
            bc_pc: 3,
        }]);
        assert!(map.record_validity_guard(3, 7, 42));
        assert_eq!(map.guard_count(), 1);
        let g = map.guards()[0];
        assert!(matches!(
            g.guard,
            GuardKind::ValidityCell {
                cell: 7,
                serial: 42
            }
        ));
        assert!(g.check_validity_cell(42));
        assert!(!g.check_validity_cell(43));
        assert!(map.validity().is_still_valid(7, 42));
        assert!(!map.validity().is_still_valid(7, 43));
        // NONE cell 0 never valid
        assert!(!map.record_validity_guard(3, 0, 1));
    }

    #[test]
    fn validity_registry_guard_for() {
        let mut reg = ValidityRegistry::default();
        reg.observe(5, 100);
        let g = reg.guard_for(10, 5).unwrap();
        assert!(matches!(
            g.guard,
            GuardKind::ValidityCell {
                cell: 5,
                serial: 100
            }
        ));
        assert!(reg.is_still_valid(5, 100));
        assert!(!reg.is_still_valid(5, 101));
        assert!(reg.guard_for(10, 0).is_none());
        assert!(reg.guard_for(10, 99).is_none());
    }
}
