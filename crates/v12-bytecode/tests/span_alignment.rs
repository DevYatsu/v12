#![forbid(unsafe_code)]

//! Requirement 7: span alignment invariant.
//!
//! `emit_spanned` must keep `spans` index-aligned with `instrs`: for any
//! function the builder produces, either both are empty or every span key is
//! `< instrs.len()` — in fact lengths stay equal, element for element.
//!
//! The sharp edge is label backpatching: patching a branch rewrites
//! instruction words in place and must never disturb span parallelism. The
//! property below compares each built function's spans against an
//! independently maintained mirror of exactly what the caller pushed,
//! including through wide multi-word emissions and patched branches.

mod common;

use common::Rng;
use v12_bytecode::{FunctionBuilder, Instr, Opcode, SpanPair, WideOp};

const LABEL_COUNT: usize = 4;

/// One randomized build sequence with its mirrored expectation.
fn mirror_build(rng: &mut Rng) -> (v12_bytecode::FunctionBytecode, Vec<SpanPair>) {
    let mut b = FunctionBuilder::new(Some("span-fuzz"));
    let mut expected: Vec<SpanPair> = Vec::new();
    let labels: Vec<_> = (0..LABEL_COUNT).map(|_| b.label()).collect();
    let mut bound = [false; LABEL_COUNT];

    let steps = rng.below(80) as usize;
    for i in 0..steps {
        match rng.below(6) {
            0 | 1 => {
                // Plain emit: unknown span sentinel (0, 0).
                b.emit(Instr::new(Opcode::Move, i as u8, 0, 1));
                expected.push((0, 0));
            }
            2 | 3 => {
                // Spanned emit: caller-chosen pair, kept verbatim.
                let span = ((i * 2) as u32, (i * 2 + 1) as u32);
                b.emit_spanned(Instr::new(Opcode::Return, i as u8, 0, 0), span);
                expected.push(span);
            }
            4 => {
                // Wide sequence word-by-word, one span per logical op — the
                // shape a front-end produces for oversized operands.
                let span = (i as u32 * 10, i as u32 * 10 + 7);
                let wide = WideOp::LoadIntW {
                    dst: i as u8,
                    value: i as i64,
                };
                for w in wide.encode() {
                    b.emit_spanned(w, span);
                    expected.push(span);
                }
            }
            _ => {
                // Branch: fixup records the pc; patching happens in finish().
                let target = labels[rng.below(LABEL_COUNT as u64) as usize];
                let op = if rng.coin(50) {
                    Opcode::Jump
                } else {
                    Opcode::JumpIfFalse
                };
                b.emit_jump(op, 2, target);
                expected.push((0, 0));
            }
        }
        if rng.coin(30) {
            let k = rng.below(LABEL_COUNT as u64) as usize;
            if !bound[k] {
                b.bind(labels[k]);
                bound[k] = true;
            }
        }
    }

    // finish() requires every label bound; close the stragglers.
    for (k, slot) in bound.iter_mut().enumerate() {
        if !*slot {
            b.bind(labels[k]);
            *slot = true;
        }
    }

    let fb = b.finish();
    (fb, expected)
}

#[test]
fn spans_stay_index_aligned_under_random_build_sequences() {
    const BUILDS: u64 = 3_000;
    let mut rng = Rng::new(0x5EED_51A2);

    for build_idx in 0..BUILDS {
        let (fb, expected) = mirror_build(&mut rng);

        // Stated invariant form: no span key may exceed the instruction
        // range, and emptiness coincides.
        assert_eq!(
            fb.spans.is_empty(),
            fb.instrs.is_empty(),
            "build {build_idx}: spans/instrs emptiness diverged"
        );
        assert!(
            fb.spans
                .iter()
                .enumerate()
                .all(|(idx, _)| idx < fb.instrs.len()),
            "build {build_idx}: span table outlives the instruction stream"
        );

        // Stronger form: exact index-for-index equality with what was pushed.
        assert_eq!(
            fb.spans.len(),
            fb.instrs.len(),
            "build {build_idx}: lengths diverged"
        );
        assert_eq!(
            fb.spans, expected,
            "build {build_idx}: spans drifted from pushed values"
        );
    }
}

/// Deterministic regression: patching forward and backward branches over
/// wide sequences leaves the span column untouched, word for word. Branches
/// themselves go through `emit_jump` (which records the fixup) so their
/// words carry the `(0, 0)` sentinel; neighboring spanned words must be
/// unaffected by the in-place patches.
#[test]
fn backpatching_does_not_disturb_span_alignment() {
    let mut b = FunctionBuilder::new(None);

    let head = b.label();
    b.bind(head); // pc 0

    let exit = b.label();
    b.emit_jump(Opcode::JumpIfFalse, 9, exit); // pc 0, patched later to pc 3
    let lcw = WideOp::LoadConstW {
        dst: 1,
        const_id: 9,
    }
    .encode();
    b.emit_spanned(lcw[0], (120, 130)); // pc 1
    b.emit_spanned(lcw[1], (120, 130)); // pc 2
    b.bind(exit);
    b.emit_spanned(Instr::new(Opcode::Return, 0, 0, 0), (140, 150)); // pc 3
    let back_at = b.pc(); // 4
    b.emit_jump(Opcode::Jump, 0, head); // backward branch over everything
    b.emit_spanned(Instr::new(Opcode::Move, 0, 0, 0), (160, 170)); // pc 5

    let fb = b.finish();

    let want = vec![
        (0u32, 0u32),
        (120, 130),
        (120, 130),
        (140, 150),
        (0, 0),
        (160, 170),
    ];
    assert_eq!(fb.instrs.len(), want.len());
    assert_eq!(fb.spans, want, "patched function must keep spans parallel");

    // The patches really happened, so this guards a real backpatch run:
    // forward conditional lands on the Return, backward jump wraps to head.
    assert_eq!(fb.instrs[0].a(), 9, "cond register survives patching");
    assert_eq!(fb.instrs[0].imm16(), 3);
    assert_eq!(fb.instrs[back_at as usize].imm24(), 0);
}
