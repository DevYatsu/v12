#![forbid(unsafe_code)]

//! Deoptimization map. Reuses baseline `PcMapEntry` 1:1 block mapping.

use v12_bytecode::PcMapEntry;

/// Maps JIT pcs to bytecode pcs. Deopt materialization = `regs[..max_regs]` copy + interp `set_pc`.
#[derive(Debug, Clone, Default)]
pub struct DeoptMap {
    pc_map: Vec<PcMapEntry>,
    live_regs: Vec<u8>,
}

impl DeoptMap {
    pub fn from_pc_map(pc_map: Vec<PcMapEntry>) -> Self {
        Self {
            pc_map,
            live_regs: Vec::new(),
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
        self.pc_map.iter().find(|e| e.jit_pc == jit_pc).map(|e| e.bc_pc)
    }
    pub fn pc_map(&self) -> &[PcMapEntry] {
        &self.pc_map
    }
    pub fn live_regs(&self) -> &[u8] {
        &self.live_regs
    }
}

/// Valid deopt target exists in map.
#[inline]
pub fn is_valid_deopt(pc_map: &[PcMapEntry], bc_pc: u32) -> bool {
    pc_map.iter().any(|e| e.bc_pc == bc_pc)
}
