//! Peephole post-pass: smi-safe constant folding, dead-jump
//! elimination, and jump-to-jump threading.
//!
//! Safety invariant relied on by folding: `LoadInt` only ever targets fresh
//! single-use temporaries in the current emitter (locals live in registers
//! and are never materialized via `LoadInt`), so deleting a folded load
//! cannot break other readers.
//!
//! Pipeline per round:
//! 1. Mark deletions — folded literal triples and dead branches.
//! 2. Compact once, remapping every branch target and handler entry through
//!    the index map. Deleted instructions "absorb" incoming edges correctly:
//!    anything that pointed at a deleted branch lands on its fall-through,
//!    which is where the deleted branch went anyway.
//! 3. Thread jumps through unconditional jumps (pure rewiring).
//!
//! Rounds repeat until convergence (bounded).

use v12_bytecode::{FunctionBytecode, Instr, Opcode, WideOp};

const MAX_ROUNDS: usize = 8;

pub(crate) fn optimize(fb: &mut FunctionBytecode) {
    for _ in 0..MAX_ROUNDS {
        if !round(fb) {
            break;
        }
    }
}

/// One mark → compact → thread sweep; returns whether anything changed.
fn round(fb: &mut FunctionBytecode) -> bool {
    let n = fb.instrs.len();
    if n == 0 {
        return false;
    }

    // --- layout: wide ops span multiple words; spans stay 1:1 with words ----
    // `width_of[i] > 0` marks a header word carrying its total word count;
    // payload words hold 0.
    let mut width_of = vec![0usize; n];
    let mut pc = 0usize;
    while pc < n {
        let w = word_width(&fb.instrs[pc..]);
        width_of[pc] = w;
        pc += w;
    }

    let mut keep = vec![true; n];
    let mut changed = false;

    // --- constant folding -----------------------------------------------------
    // Pattern over three consecutive single-word headers:
    //   [LoadInt rX #i] [LoadInt rY #j] [Add|Sub|Mul rZ, rX, rY]
    // becomes [LoadInt rZ #(i op j)] plus two deletions.
    //
    // Consecutive headers here are literally adjacent indices because both
    // loads are verified narrow (width 1).
    for i in 1..n.saturating_sub(2) {
        // Skip stale (already-deleted) words: their bytes no longer execute.
        if !keep[i] || !keep[i - 1] {
            continue;
        }
        if width_of[i] != 1 || fb.instrs[i].op() != Some(Opcode::LoadInt) {
            continue;
        }
        let (h1, h2, h3) = (i - 1, i, i + 1);
        if width_of[h1] != 1 || fb.instrs[h1].op() != Some(Opcode::LoadInt) || width_of[h3] < 1 {
            continue;
        }
        let Some(op3) = fb.instrs[h3].op() else {
            continue;
        };
        if !matches!(op3, Opcode::Add | Opcode::Sub | Opcode::Mul) {
            continue;
        }
        let (rx, lx) = (fb.instrs[h1].a(), imm8(fb.instrs[h1]));
        let (ry, ly) = (fb.instrs[h2].a(), imm8(fb.instrs[h2]));
        let (rz, b3, c3) = (fb.instrs[h3].a(), fb.instrs[h3].b(), fb.instrs[h3].c());
        let direct = b3 == rx && c3 == ry;
        let swapped = matches!(op3, Opcode::Add | Opcode::Mul) && b3 == ry && c3 == rx;
        if !(direct || swapped) {
            continue;
        }
        let (a, b) = if direct { (lx, ly) } else { (ly, lx) };
        let folded = match op3 {
            Opcode::Add => a.checked_add(b),
            Opcode::Sub => a.checked_sub(b),
            Opcode::Mul => a.checked_mul(b),
            _ => unreachable!("filtered above"),
        }
        .and_then(|v| i8::try_from(v).ok());
        if let Some(v) = folded {
            // Rewrite in place at h1; delete h2/h3 groups.
            fb.instrs[h1] = Instr::new(Opcode::LoadInt, rz, 0, v as u8);
            keep[h2] = false;
            keep[h3] = false;
            for k in 1..width_of[h2] {
                keep[h2 + k] = false;
            }
            for k in 1..width_of[h3] {
                keep[h3 + k] = false;
            }
            changed = true;
        }
    }

    // --- dead branches -------------------------------------------------------------
    // Any branch whose target is the immediately-next header is redundant:
    // taken or not, execution continues at that same instruction. The
    // instruction after the *last* header counts as `n` (implicit return).
    {
        let headers: Vec<usize> = (0..n).filter(|&i| width_of[i] > 0).collect();
        for (pos, &h) in headers.iter().enumerate() {
            let next = headers.get(pos + 1).copied().unwrap_or(n);
            match fb.instrs[h].op() {
                Some(Opcode::Jump) => {
                    if fb.instrs[h].imm24() as usize == next {
                        keep[h] = false;
                        changed = true;
                    }
                }
                Some(Opcode::JumpIfFalse | Opcode::JumpIfTrue)
                    if fb.instrs[h].imm16() as usize == next =>
                {
                    keep[h] = false;
                    changed = true;
                }
                _ => {}
            }
        }
    }

    // Payload words live or die with their header.
    let mut pc = 0usize;
    while pc < n {
        let w = width_of[pc];
        if !keep[pc] {
            for k in 1..w {
                keep[pc + k] = false;
            }
        } else {
            for k in 1..w {
                keep[pc + k] = true;
            }
        }
        pc += w;
    }

    // --- compaction ---------------------------------------------------------------
    if keep.iter().any(|&k| !k) {
        let mut map = vec![0u32; n + 1];
        let mut running = 0u32;
        let mut new_instrs = Vec::with_capacity(n);
        let mut new_spans = Vec::with_capacity(n);
        for idx in 0..n {
            map[idx] = running;
            if keep[idx] {
                new_instrs.push(fb.instrs[idx]);
                new_spans.push(fb.spans[idx]);
                running += 1;
            }
        }
        map[n] = running;

        for instr in new_instrs.iter_mut() {
            match instr.op() {
                Some(Opcode::Jump) => {
                    let t = instr.imm24() as usize;
                    instr.set_imm24(map[t.min(n)]);
                }
                Some(Opcode::JumpIfFalse) | Some(Opcode::JumpIfTrue) => {
                    let t = instr.imm16() as usize;
                    instr.set_imm16(map[t.min(n)] as u16);
                }
                _ => {}
            }
        }

        let mut handlers = std::mem::take(&mut fb.handlers);
        for h in handlers.iter_mut() {
            h.start = map[h.start as usize];
            h.end = map[h.end as usize];
            h.target = map[h.target as usize];
        }
        handlers.retain(|h| h.end > h.start);
        fb.handlers = handlers;
        fb.instrs = new_instrs;
        fb.spans = new_spans;
        changed = true;
    }

    // --- jump-to-jump threading (pure rewiring, no reindexing) ----------------------
    for i in 0..fb.instrs.len() {
        match fb.instrs[i].op() {
            Some(Opcode::Jump) => {
                let mut t = fb.instrs[i].imm24() as usize;
                let mut hops = 0usize;
                while hops < 16 && t != i {
                    if fb.instrs.get(t).and_then(|w| w.op()) == Some(Opcode::Jump) {
                        t = fb.instrs[t].imm24() as usize;
                        hops += 1;
                    } else {
                        break;
                    }
                }
                if hops > 0 {
                    fb.instrs[i].set_imm24(t as u32);
                    changed = true;
                }
            }
            Some(Opcode::JumpIfFalse | Opcode::JumpIfTrue) => {
                let mut t = fb.instrs[i].imm16() as usize;
                let mut hops = 0usize;
                while hops < 16 {
                    if fb.instrs.get(t).and_then(|w| w.op()) == Some(Opcode::Jump) {
                        t = fb.instrs[t].imm24() as usize;
                        hops += 1;
                    } else {
                        break;
                    }
                }
                if hops > 0 {
                    debug_assert!(t <= u32::from(u16::MAX) as usize);
                    fb.instrs[i].set_imm16(t as u16);
                    changed = true;
                }
            }
            _ => {}
        }
    }

    changed
}

/// Sign-extended i8 immediate of a `LoadInt`.
fn imm8(instr: Instr) -> i32 {
    i8::from_be_bytes([instr.c()]) as i32
}

/// Wide sequences count as one logical word.
fn word_width(words: &[Instr]) -> usize {
    match words.first().and_then(|w| w.op()) {
        Some(Opcode::Wide) => WideOp::try_decode(words).map(|(_, w)| w).unwrap_or(1),
        _ => 1,
    }
}
