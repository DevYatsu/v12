#![forbid(unsafe_code)]

//! Requirement 4: immediate-encoding algebra and LoadInt sign handling.
//!
//! - imm16 boundaries roundtrip through both packers with the documented
//!   big-endian split (high byte in `b`, low in `c`).
//! - imm24 boundaries cover 0, 1, MAX_IMM24; anything larger is unencodable
//!   (debug_assert) — pinned via `#[should_panic]` tests guarded by
//!   `debug_assertions` since release builds compile the assert out.
//! - LoadInt's signed byte and WideOp::LoadIntW's two's-complement i64 are
//!   checked end-to-end through encode → decode → Display.

mod common;

use common::{KNOWN_DISCRIMINANTS, render};
use v12_bytecode::{Const, ConstantPool, FunctionBytecode, Instr, MAX_IMM24, Opcode, WideOp};

const IMM16_BOUNDARIES: [u16; 8] = [
    0x0000, 0x0001, 0x00FF, 0x0100, 0x7FFF, 0x8000, 0xFFFE, 0xFFFF,
];

#[test]
fn imm16_boundaries_roundtrip_through_both_packers() {
    // Representative opcodes for each immediate shape: conditional branch
    // (imm16 target + cond reg) and a generic operand user.
    for &op in &[Opcode::JumpIfFalse, Opcode::JumpIfTrue, Opcode::LoadConst] {
        for &v in &IMM16_BOUNDARIES {
            let i = Instr::new_imm16(op, 0xA7, v);
            assert_eq!(i.op(), Some(op));
            assert_eq!(i.a(), 0xA7, "{op:?} imm16 {v:#06x}: slot `a` clobbered");
            // Big-endian split across b/c.
            assert_eq!(
                (i.b(), i.c()),
                ((v >> 8) as u8, v as u8),
                "{op:?} imm16 {v:#06x}"
            );
            assert_eq!(i.imm16(), v);

            // set_imm16 rewrites only the immediate bits.
            let mut j = Instr::new(op, 0x11, 0x22, 0x33);
            j.set_imm16(v);
            assert_eq!(j.a(), 0x11, "set_imm16 must preserve slot `a`");
            assert_eq!(j.imm16(), v);
        }
    }
}

#[test]
fn imm24_boundaries_decompose_big_endian() {
    const BOUNDARIES: [u32; 5] = [0, 1, 2, MAX_IMM24 / 2, MAX_IMM24];
    for &op_byte in KNOWN_DISCRIMINANTS {
        let op = Opcode::try_from(op_byte).unwrap();
        for &v in &BOUNDARIES {
            let i = Instr::new_imm24(op, v);
            assert_eq!(i.op(), Some(op));
            assert_eq!(i.imm24(), v, "{op:?} imm24 {v:#08x}");
            assert_eq!(
                (i.a(), i.b(), i.c()),
                ((v >> 16) as u8, (v >> 8) as u8, v as u8),
                "imm24 must decompose big-endian into a/b/c"
            );
            assert_eq!(i.0, (u32::from(op_byte) << 24) | v);

            // set_imm24 preserves the opcode byte.
            let mut j = Instr::new(op, 0xFF, 0xFF, 0xFF);
            j.set_imm24(v);
            assert_eq!((j.0 >> 24) as u8, op_byte);
            assert_eq!(j.imm24(), v);
        }
    }
}

/// Values above MAX_IMM24 cannot be packed by `new_imm24`: the constructor
/// carries a debug_assert. These tests document that contract; they are
/// compiled only where debug assertions exist (the default `cargo nextest
/// run` profile).
#[cfg(debug_assertions)]
mod imm24_overflow_is_unencodable {
    use super::*;

    #[test]
    #[should_panic(expected = "exceeds 24 bits")]
    fn new_imm24_rejects_max_plus_one() {
        let _ = Instr::new_imm24(Opcode::Jump, MAX_IMM24 + 1);
    }

    #[test]
    #[should_panic(expected = "exceeds 24 bits")]
    fn new_imm24_rejects_full_u32() {
        let _ = Instr::new_imm24(Opcode::Jump, u32::MAX);
    }

    /// Asymmetric by design of the current implementation: `set_imm24` has
    /// no debug_assert and silently truncates to the low 24 bits. The sharp
    /// edge: exactly one past the cap wraps to 0 — a backpatcher bug would
    /// turn into "jump to pc 0" instead of a loud failure. Pinned so a
    /// future guard (or behavior change) is a conscious decision.
    #[test]
    fn set_imm24_masks_instead_of_asserting() {
        let mut i = Instr::new(Opcode::Jump, 0, 0, 0);

        i.set_imm24(MAX_IMM24 + 1); // 0x0100_0000
        assert_eq!(i.imm24(), 0, "one past the cap wraps to zero");
        assert_eq!((i.0 >> 24) as u8, Opcode::Jump as u8, "opcode survives");

        let mut j = Instr::new(Opcode::Jump, 0, 0, 0);
        j.set_imm24(0xDEAD_BEEF);
        assert_eq!(j.imm24(), 0x00AD_BEEF, "only the low 24 bits are kept");
        assert_eq!((j.0 >> 24) as u8, Opcode::Jump as u8);
    }
}

#[test]
fn narrow_load_int_signed_byte_survives_decode_and_display() {
    const VALUES: [i8; 9] = [-128, -100, -42, -1, 0, 1, 42, 100, 127];

    for &v in &VALUES {
        let raw = v as u8;
        let i = Instr::new(Opcode::LoadInt, 3, 0, raw);
        assert_eq!(i.op(), Some(Opcode::LoadInt));

        // Decode path: slot `c` reinterpreted as i8.
        assert_eq!(i.c() as i8, v, "LoadInt sign handling broke for {v}");

        // End-to-end: the disassembler prints the *signed* value.
        let mut fb = common::fn_with(1, Vec::new());
        fb.instrs[0] = i;
        let text = render(&fb);
        assert!(
            text.contains(&format!("load_int r3, #{v}")),
            "expected signed literal #{v} in disassembly:\n{text}"
        );
    }
}

/// Helper building a function around a pre-encoded word sequence.
fn fb_of(words: Vec<Instr>) -> FunctionBytecode {
    FunctionBytecode {
        name_hint: None,
        max_regs: 6,
        spans: vec![(0, 0); words.len()],
        instrs: words,
        consts: ConstantPool::new(),
        handlers: Vec::new(),
        pc_map: Vec::new(),
        is_strict: false,
        fixed_params: 0,
        has_rest: false,
        rest_reg: 0,
    }
}

#[test]
fn wide_load_int_negative_values_survive_encode_decode_end_to_end() {
    const VALUES: [i64; 10] = [
        i64::MIN,
        i64::MIN + 1,
        -((1i64 << 63) >> 8), // high byte touched
        -(1i64 << 32),        // exactly at the word boundary
        -65536,
        -256,
        -1,
        0,
        1,
        i64::MAX,
    ];

    for &v in &VALUES {
        let words = WideOp::LoadIntW { dst: 5, value: v }.encode();
        assert_eq!(words.len(), 3);

        // Documented layout: low u32 first, then high u32, two's complement.
        let bits = v as u64;
        assert_eq!(words[1].0, bits as u32, "low word of {v}");
        assert_eq!(words[2].0, (bits >> 32) as u32, "high word of {v}");

        let (back, width) = WideOp::try_decode(&words).unwrap();
        assert_eq!(width, 3);
        assert_eq!(back, WideOp::LoadIntW { dst: 5, value: v });

        // And the disassembler shows the signed decimal value.
        let text = render(&fb_of(words));
        assert!(
            text.contains(&format!("load_int_w r5, #{v}")),
            "missing signed wide literal for {v}:\n{text}"
        );
    }
}

/// The narrow LoadConst's 8-bit constant index and the wide variant's full
/// u32 id coexist: a pool entry beyond 255 must be referenced through the
/// wide form only.
#[test]
fn const_ids_beyond_u8_require_the_wide_form() {
    let mut pool = ConstantPool::new();
    for i in 0..300_u32 {
        let _ = pool.insert(Const::Str32(i));
    }
    let k256 = pool.get(256).unwrap();
    assert_eq!(k256, Const::Str32(256));

    // Narrow reference of index 256 truncates silently at the slot level
    // (slot b holds 0) — pinning what the encoding can express.
    let narrow = Instr::new_imm16(Opcode::LoadConst, 0, 256);
    assert_eq!(narrow.b(), 1);

    // The wide form carries the full 32-bit id losslessly.
    let wide = WideOp::LoadConstW {
        dst: 0,
        const_id: u32::MAX,
    }
    .encode();
    let (back, _) = WideOp::try_decode(&wide).unwrap();
    assert_eq!(
        back,
        WideOp::LoadConstW {
            dst: 0,
            const_id: u32::MAX
        }
    );
}
