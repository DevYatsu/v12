#![forbid(unsafe_code)]

//! Requirement 2: the Display disassembler never panics.
//!
//! ≥10k seeded-random `FunctionBytecode` values — including WIDE-prefixed
//! sequences with arbitrary following words, malformed wide headers, unknown
//! opcode bytes, and junk handler tables — must all render non-empty text.
//!
//! One structural property is asserted exactly: the pcs listed by Display
//! are precisely those produced by walking the words with
//! [`v12_bytecode::WideOp::try_decode`] folding successful multi-word ops,
//! i.e. Display's traversal can never diverge from the public decoder.

mod common;

use common::{Rng, random_function, render};
use v12_bytecode::{FunctionBytecode, Instr, Opcode, WideOp};

/// Mirrors the documented Display traversal: every word either starts a line
/// or is folded into a preceding successfully-decoded wide sequence.
fn expected_pcs(instrs: &[Instr]) -> Vec<u32> {
    let mut pcs = Vec::new();
    let mut pc = 0usize;
    while pc < instrs.len() {
        pcs.push(pc as u32);
        if instrs[pc].op() == Some(Opcode::Wide)
            && let Ok((_, width)) = WideOp::try_decode(&instrs[pc..])
        {
            pc += width;
            continue;
        }
        pc += 1;
    }
    pcs
}

#[test]
fn display_never_panics_on_10k_random_functions() {
    const SAMPLES: u64 = 12_345;
    let mut rng = Rng::new(0xD15F_F00D);

    for i in 0..SAMPLES {
        // Empty functions are part of the space; min_words 0 lets the
        // generator emit nothing at all.
        let min_words = rng.below(24) as usize;
        let rf = random_function(&mut rng, min_words);
        let text = render(&rf.fb);

        assert!(!text.is_empty(), "sample {i}: empty disassembly");
        assert!(
            text.starts_with("function <anon>(max_regs="),
            "sample {i}: missing header line:\n{text}"
        );
        assert!(text.ends_with('\n'), "sample {i}: unterminated output");

        // Collect the pcs Display actually printed ({pc:04}: prefix).
        let mut listed: Vec<u32> = Vec::new();
        for line in text.lines() {
            let head = line.split(':').next().unwrap_or("");
            if let Ok(pc) = head.parse::<u32>() {
                // Handler rows like "[0, 5) -> 1" never parse as u32, and
                // const rows start with 'k', so only pc prefixes land here.
                listed.push(pc);
            }
        }

        let want = expected_pcs(&rf.fb.instrs);
        assert_eq!(
            listed, want,
            "sample {i}: Display traversal diverged from WideOp::try_decode"
        );
        assert!(
            want.iter().all(|&pc| (pc as usize) < rf.fb.instrs.len()),
            "sample {i}: listed pc out of bounds"
        );
    }
}

/// A hand-built torture case: every documented malformed-wide shape in one
/// function, all of which must render with diagnostics instead of panicking.
#[test]
fn display_renders_documented_malformed_wide_shapes() {
    let instrs = vec![
        // Header only, no payload at all.
        Instr::new_imm24(Opcode::Wide, 0),
        // LoadIntW needs two payload words; provide one whose own opcode
        // byte is unassigned so the orphan renders as .word.
        Instr::new_imm24(Opcode::Wide, 1),
        Instr(0x0011_1111),
        // Unknown discriminant with plenty of trailing words.
        Instr::new_imm24(Opcode::Wide, 200),
        Instr(0x2222_2222),
        Instr(0x3333_3333),
        // Valid LoadConstW followed by an unassigned opcode byte.
        Instr::new(Opcode::Wide, 7, 0, 0),
        Instr(42),
        Instr(0x0500_0000),
    ];
    let fb = FunctionBytecode {
        name_hint: Some("torture".into()),
        max_regs: 8,
        spans: vec![(0, 0); instrs.len()],
        instrs,
        consts: Default::default(),
        handlers: Vec::new(),
        pc_map: Vec::new(),
        is_strict: false,
    };

    let text = render(&fb);
    assert!(text.contains("malformed"), "expected diagnostics:\n{text}");
    assert!(
        text.contains(".word"),
        "unknown bytes must render as .word:\n{text}"
    );
    assert!(
        text.contains("load_const_w r7, k42"),
        "valid wide op must still decode:\n{text}"
    );

    // The lone 0x1111_1111 orphaned by the truncated LoadIntW renders as its
    // own .word line, proving failed decodes advance exactly one word.
    assert!(
        text.contains("0002: .word 0x00111111"),
        "orphaned payload must be listed at pc 2:\n{text}"
    );
}
