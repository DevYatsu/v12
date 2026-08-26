#![forbid(unsafe_code)]

//! Requirement 5: FunctionBuilder adversarial stress.
//!
//! - ≥50-level nested handler ranges validate.
//! - A 65k-instruction function builds, validates, and disassembles with
//!   exact line accounting.
//! - Double-binding panics; binding *before* jumping is documented backward
//!   branching (no panic).
//! - Forward and backward jumps across multi-word wide sequences patch to
//!   the right absolute pcs — the pc-accounting regression trap.

mod common;

use common::render;
use v12_bytecode::{FunctionBuilder, HandlerRange, Instr, Opcode, WideOp};

#[test]
fn deeply_nested_handler_ranges_validate() {
    const LEVELS: u32 = 50;
    const CODE_LEN: u32 = 200;

    // Level i owns [i, CODE_LEN - i): starts ascend (sorted), each range
    // strictly contains the next (nested), depths strictly increase.
    let handlers: Vec<HandlerRange> = (0..LEVELS)
        .map(|i| HandlerRange {
            start: i,
            end: CODE_LEN - i,
            target: CODE_LEN - 1,
            stack_depth: i + 1,
        })
        .collect();

    let mut fb = common::fn_with(CODE_LEN, handlers);
    fb.max_regs = LEVELS as u16;
    fb.validate().expect("50-level nesting must validate");

    // The disassembler prints every row of a deep table unchanged.
    let text = render(&fb);
    assert_eq!(
        text.matches("depth=").count(),
        LEVELS as usize,
        "handler table rows missing from disassembly"
    );
}

/// The same nesting built through the builder's own pc() bookkeeping.
#[test]
fn builder_pushed_deep_nesting_matches_pcs() {
    let mut b = FunctionBuilder::new(Some("onion"));
    b.reserve_regs(4);

    // One throwing op per level; record each pc via the builder itself.
    let starts: Vec<u32> = (0..25)
        .map(|_| {
            let s = b.pc();
            b.emit(Instr::new(Opcode::NewObject, 0, 0, 0));
            s
        })
        .collect();

    while b.pc() < 52 {
        b.emit(Instr::new(Opcode::Move, 2, 0, 0));
    }

    // Outermost first: starts ascend, ends descend, so every later range is
    // strictly contained in the previous one and depths increase with it.
    for (level, &s) in starts.iter().enumerate() {
        let level = level as u32;
        b.push_handler(HandlerRange {
            start: s,
            end: 52 - level,
            target: 51,
            stack_depth: level + 1,
        });
    }

    let fb = b.finish();
    fb.validate()
        .expect("builder-computed handler pcs must validate");
    assert_eq!(fb.handlers.len(), 25);
    // All handlers open at pc 0 and close progressively: sorted by start.
    assert!(fb.handlers.windows(2).all(|w| w[0].start <= w[1].start));
}

#[test]
fn build_and_disassemble_65k_instruction_function() {
    const TARGET_WORDS: usize = 65_000;

    let mut b = FunctionBuilder::new(Some("big"));
    b.reserve_regs(8);

    // Loop skeleton with one wide sequence per iteration plus occasional
    // forward skips, so backpatching interacts with multi-word ops at scale.
    let top = b.label();
    b.bind(top);

    let wide_ops = [
        WideOp::LoadConstW {
            dst: 3,
            const_id: 7,
        },
        WideOp::LoadIntW { dst: 4, value: -1 },
        WideOp::GetEnvSlotW {
            dst: 5,
            depth: 1,
            slot: 2,
        },
        WideOp::SetEnvSlotW {
            src: 5,
            depth: 1,
            slot: 2,
        },
        WideOp::CallW {
            dst: 6,
            func: 1,
            argc: 2,
        },
    ];

    let mut iterations = 0usize;
    let mut forward_jumps = Vec::new(); // (fixup_pc, expected_target)
    while b.pc() < TARGET_WORDS as u32 {
        let op = &wide_ops[iterations % wide_ops.len()];
        for w in op.encode() {
            b.emit_spanned(w, (10 * iterations as u32, 10 * iterations as u32 + 5));
        }

        // Every 16th iteration: forward skip over two filler words.
        if iterations.is_multiple_of(16) {
            let skip = b.label();
            let at = b.pc();
            b.emit_jump(Opcode::Jump, 0, skip);
            b.emit(Instr::new(Opcode::Move, 0, 0, 1));
            b.emit(Instr::new(Opcode::Move, 0, 0, 2));
            b.bind(skip);
            forward_jumps.push((at, b.pc()));
        }
        iterations += 1;
    }

    let counter_check = b.label();
    b.emit_jump(Opcode::JumpIfFalse, 7, counter_check);
    let back_at = b.pc();
    b.emit_jump(Opcode::Jump, 0, top); // backward over everything emitted

    b.bind(counter_check);
    b.emit(Instr::new(Opcode::Return, 0, 0, 0));

    let fb = b.finish();
    assert!(fb.instrs.len() >= TARGET_WORDS);
    fb.validate()
        .expect("large function must satisfy invariants");

    // Backward unconditional jump lands on the loop head (pc after `bind(top)`).
    let head_pc = 0u32; // bind(top) happened before any emission
    assert_eq!(
        fb.instrs[back_at as usize].imm24(),
        head_pc,
        "backward jump must wrap to the loop head"
    );
    // Conditional patch kept its cond register.
    assert_eq!(fb.instrs[(back_at - 1) as usize].a(), 7);

    for &(at, want) in &forward_jumps {
        assert_eq!(
            fb.instrs[at as usize].imm24(),
            want,
            "forward jump at pc {at} patched to {want}"
        );
    }

    // Disassembly line accounting: header + one line per decode unit.
    let text = render(&fb);
    let lines = text.lines().count();
    let expected_units = 1 + expected_decode_units(&fb.instrs);
    assert_eq!(lines, expected_units, "line accounting drifted");
}

/// Walk mirroring Display's traversal to count printed instruction units.
fn expected_decode_units(instrs: &[Instr]) -> usize {
    let mut units = 0usize;
    let mut pc = 0usize;
    while pc < instrs.len() {
        units += 1;
        if instrs[pc].op() == Some(Opcode::Wide)
            && let Ok((_, width)) = WideOp::try_decode(&instrs[pc..])
        {
            pc += width;
            continue;
        }
        pc += 1;
    }
    units
}

#[test]
#[should_panic(expected = "bound more than once")]
fn double_binding_a_label_panics() {
    let mut b = FunctionBuilder::new(None);
    let l = b.label();
    b.bind(l);
    b.bind(l);
}

#[test]
#[should_panic(expected = "never bound")]
fn finish_panics_when_any_label_is_unbound() {
    let mut b = FunctionBuilder::new(None);
    let _orphan = b.label();
    b.emit(Instr::new(Opcode::Return, 0, 0, 0));
    b.finish();
}

/// Documented behavior: binding a label then branching to it yields a
/// *backward* branch to the bound pc — that is how loops close. No panic.
#[test]
fn jump_to_already_bound_label_is_backward_branch() {
    let mut b = FunctionBuilder::new(None);
    let loop_head = b.label();
    b.bind(loop_head); // binds to pc 0 (next emitted instruction)
    assert_eq!(b.pc(), 0);

    b.emit(Instr::new_imm24(Opcode::LoopHeader, 0)); // pc 0
    b.emit(Instr::new(Opcode::Add, 0, 0, 1)); // pc 1
    let jump_at = b.pc(); // 2
    b.emit_jump(Opcode::JumpIfTrue, 2, loop_head);
    let after = b.pc(); // 3
    b.emit(Instr::new(Opcode::Return, 0, 0, 0));

    let fb = b.finish();
    assert_eq!(
        fb.instrs[jump_at as usize].imm16(),
        0,
        "target is the bound pc"
    );
    assert!(jump_at < after);
    assert!(fb.validate().is_ok());
}

/// Regression guard: labels bind to word-accurate pcs even when wide
/// sequences sit between the branch and its target, in both directions.
#[test]
fn jumps_across_wide_sequences_patch_to_word_accurate_pcs() {
    let mut b = FunctionBuilder::new(Some("wide-hops"));

    // Backward target before any wide ops.
    let head = b.label();
    b.bind(head); // pc 0

    // Forward branch emitted first, patched later across five wide ops.
    let exit = b.label();
    b.emit_jump(Opcode::JumpIfFalse, 9, exit); // pc 0

    let load_const_w = WideOp::LoadConstW {
        dst: 1,
        const_id: 0xDEAD_BEEF,
    }
    .encode();
    let load_int_w = WideOp::LoadIntW {
        dst: 2,
        value: i64::MIN,
    }
    .encode();
    let call_w = WideOp::CallW {
        dst: 3,
        func: 4,
        argc: 1000,
    }
    .encode();
    let get_env_w = WideOp::GetEnvSlotW {
        dst: 4,
        depth: 0x1234,
        slot: 0x5678,
    }
    .encode();
    let set_env_w = WideOp::SetEnvSlotW {
        src: 5,
        depth: 0x0102,
        slot: 0x0304,
    }
    .encode();

    for words in [&load_const_w, &load_int_w, &call_w, &get_env_w, &set_env_w] {
        for w in words.iter() {
            b.emit(*w);
        }
    }
    // Words so far: 1 (branch) + 2+3+2+2+2 (wides) = 12. Next pc is 12...
    assert_eq!(b.pc(), 12);
    let mid = b.label();
    b.bind(mid); // binds to pc 12

    // Another forward hop from inside the block over a trailing wide pair.
    let done = b.label();
    b.emit_jump(Opcode::Jump, 0, done); // pc 12
    for w in load_int_w.iter().chain(load_const_w.iter()) {
        b.emit(*w);
    } // 3 + 2 payload words: pcs 13..17
    b.bind(done); // binds to pc 18

    // Backward unconditional jump all the way to the head.
    let back_at = b.pc(); // 18
    b.emit_jump(Opcode::Jump, 0, head);

    b.bind(exit); // pc 19: fall-through target of the opening conditional
    b.emit(Instr::new(Opcode::Return, 0, 0, 0));

    let fb = b.finish();
    assert_eq!(fb.instrs.len(), 20);

    // Conditional forward branch (imm16) lands exactly on the wide-free zone.
    assert_eq!(fb.instrs[0].a(), 9, "cond register survives patching");
    assert_eq!(
        fb.instrs[0].imm16(),
        19,
        "exit binds after the backward jump"
    );
    // Unconditional forward hop over the trailing wides.
    assert_eq!(fb.instrs[12].imm24(), 18);
    // Unconditional backward hop over all five wide sequences.
    assert_eq!(fb.instrs[back_at as usize].imm24(), 0);

    // The mid label was bound but never branched to; finish accepted it,
    // matching the docs (only *unbound* labels are errors).
    assert!(fb.validate().is_ok());

    // Disassembly decodes every wide sequence intact despite the branches
    // sharing the stream.
    let text = render(&fb);
    for needle in [
        "load_const_w r1",
        "load_int_w r2",
        "call_w r3",
        "get_env_slot_w r4",
        "set_env_slot_w r5",
    ] {
        assert!(text.contains(needle), "missing {needle}:\n{text}");
    }
}

#[test]
fn hundreds_of_labels_all_patch_independently() {
    const LABELS: u32 = 500;

    let mut b = FunctionBuilder::new(None);
    let mut fixups = Vec::new();
    let labels: Vec<_> = (0..LABELS).map(|_| b.label()).collect();

    for l in &labels {
        let at = b.pc();
        b.emit_jump(Opcode::JumpIfTrue, 1, *l);
        b.emit(Instr::new(Opcode::Move, 0, 0, 0)); // padding the target away
        fixups.push((at, b.pc()));
        b.bind(*l);
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));
    }

    let fb = b.finish();
    for (at, want) in fixups {
        assert_eq!(fb.instrs[at as usize].imm16(), want as u16);
    }
    // Spot-check the last label actually landed where expected.
    let last = fb.instrs.len() - 1;
    assert_eq!(fb.instrs[last].op(), Some(Opcode::Return));
}
