//! Static bytecode analysis — SSA construction / loop versioning support.

use crate::{FunctionBytecode, Instr, Opcode, WideOp};

// ---------------------------------------------------------------------------
// Static bytecode analysis — SSA construction / loop versioning support
// ---------------------------------------------------------------------------

/// Maximum number of *body* ops (excluding the terminating `Return`/`Throw`)
/// for a callee to be considered an inline candidate by the tier-2 optimizer.
///
/// Chosen so a small accessor/binary-op function splices into its caller
/// without multiplying code size more than ~3× per call site.
pub const MAX_INLINE_SIZE: usize = 20;

/// Width of the instruction starting at word index `pc`.
///
/// Narrow instructions are one word; a `Wide` escape occupies the documented
/// payload length of its [`WideOp`]. A malformed trailing wide sequence (e.g.,
/// truncated payload) counts as one word so the walk terminates on any input.
#[must_use]
pub fn instr_width(instrs: &[Instr], pc: usize) -> usize {
    let Some(instr) = instrs.get(pc) else {
        return 0;
    };
    match instr.op() {
        Some(Opcode::Wide) => WideOp::try_decode(&instrs[pc.min(instrs.len())..])
            .map(|(_, width)| width)
            .unwrap_or(1),
        _ => 1,
    }
}

/// All instruction-start word offsets ("logical" pcs), skipping wide payloads.
///
/// This is the canonical iteration order for analysis passes and gives every
/// wide op exactly one identity as a single logical op.
#[must_use]
pub fn logical_pcs(fb: &FunctionBytecode) -> Vec<u32> {
    let mut pcs = Vec::new();
    let mut pc = 0usize;
    while pc < fb.instrs.len() {
        pcs.push(pc as u32);
        let width = instr_width(&fb.instrs, pc);
        if width == 0 {
            break;
        }
        pc += width;
    }
    pcs
}

/// Returns the logical pc immediately after `pc`, or `None` past the end.
///
/// Precondition: `pc` is itself an instruction start (a member of
/// [`logical_pcs`]); calling this mid-instruction yields a meaningless result.
#[must_use]
pub fn next_logical_pc(fb: &FunctionBytecode, pc: u32) -> Option<u32> {
    let next = pc + instr_width(&fb.instrs, pc as usize) as u32;
    (next < fb.instrs.len() as u32).then_some(next)
}

/// Whether the instruction at `pc` is an explicit loop-header marker.
///
/// `LoopHeader` pseudo-ops are emitted by the compiler for structured loops
/// so optimizers can find headers without control-flow recovery.
#[must_use]
pub fn is_loop_header(fb: &FunctionBytecode, pc: u32) -> bool {
    fb.instrs
        .get(pc as usize)
        .and_then(|instr| instr.op())
        .is_some_and(|op| op == Opcode::LoopHeader)
}

/// Word offsets of all `LoopHeader` markers, in program order.
#[must_use]
pub fn loop_headers(fb: &FunctionBytecode) -> Vec<u32> {
    logical_pcs(fb)
        .into_iter()
        .filter(|&pc| is_loop_header(fb, pc))
        .collect()
}

/// One counted loop identified around `header`.
///
/// A loop is *counted* when its backedge update has the canonical self-
/// increment shape (`r{dst} <- r{dst} ± r{c}` with `dst` in `{b, c}`), which
/// is what the compiler emits for induction variables. Non-counted loops may
/// still be peeled; only counted loops are eligible for unrolling because
/// their trip count is loop-bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountedLoop {
    /// The `LoopHeader` marker pc (inclusive start of the body range).
    pub header: u32,
    /// First pc in `(header, backedge)` whose conditional branch leaves the
    /// loop; `None` when no such site was found statically.
    pub exit: Option<u32>,
    /// Induction variable register, when detected via a canonical update.
    pub induction: Option<u8>,
    /// Pc of the unconditional `Jump` back to `header`.
    pub backedge: u32,
}

/// Detects the loop anchored at `header`, reporting counted-ness via
/// [`CountedLoop::induction`].
///
/// Only cheap static shape checks are performed; the caller decides whether
/// to act based on feedback hotness.
#[must_use]
pub fn find_counted_loop(fb: &FunctionBytecode, header: u32) -> Option<CountedLoop> {
    // Backedge: first unconditional jump targeting `header` below it.
    let mut backedge = None;
    let mut exit = None;
    let mut induction = None;

    let mut pc = header as usize + instr_width(&fb.instrs, header as usize);
    while pc < fb.instrs.len() {
        let Some(op) = fb.instrs[pc].op() else {
            break;
        };
        match op {
            Opcode::LoopHeader => break,
            Opcode::Jump => {
                if fb.instrs[pc].imm24() == header {
                    backedge = Some(pc as u32);
                    break;
                }
            }
            Opcode::JumpIfFalse | Opcode::JumpIfTrue => {
                let target = u32::from(fb.instrs[pc].imm16());
                let is_backedge = target >= header && target < pc as u32;
                if !is_backedge {
                    exit.get_or_insert(pc as u32);
                }
            }
            // Canonical counted update: `r{dst} <- r{dst} ± r{c}`.
            Opcode::Add | Opcode::Sub => {
                let i = fb.instrs[pc];
                let dst = i.a();
                if dst == i.b() || dst == i.c() {
                    induction.get_or_insert(dst);
                }
            }
            _ => {}
        }
        let width = instr_width(&fb.instrs, pc);
        if width == 0 {
            break;
        }
        pc += width;
    }

    Some(CountedLoop {
        header,
        exit,
        induction,
        backedge: backedge?,
    })
}

/// Whether `callee` qualifies for inlining at a call site: small enough under
/// [`MAX_INLINE_SIZE`] and terminated (so splicing cannot run off the end).
#[must_use]
pub fn is_inline_candidate(callee: &FunctionBytecode) -> bool {
    let pcs = logical_pcs(callee);
    let Some(&last) = pcs.last() else {
        return false;
    };
    matches!(
        callee.instrs[last as usize].op(),
        Some(Opcode::Return | Opcode::Throw)
    ) && pcs.len() <= MAX_INLINE_SIZE + 1
}
