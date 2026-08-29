#![forbid(unsafe_code)]

//! Requirement 1: exhaustive + randomized decode sweeps.
//!
//! Every one of the 256 possible first bytes is fed through every decoding
//! path (`Instr::op`, operand accessors, `WideOp::try_decode`,
//! `ConstantPool`, Display), followed by ≥100k seeded-random u32 words.
//! Contract: nothing panics, `.op()` is `Some` exactly for assigned
//! discriminants, and unassigned bytes produce the documented diagnostics.

mod common;

use common::{
    EXPECTED_OPCODE_COUNT, KNOWN_DISCRIMINANTS, Rng, WIDE_TOTAL_WORDS, random_wide_op, render,
};
use v12_bytecode::{Const, ConstantPool, FunctionBytecode, Instr, Opcode, WideOp};

/// The oracle: which of the 256 opcode bytes does `Opcode::try_from` accept,
/// and does it agree with the hardcoded discriminant list?
#[test]
fn try_from_accepts_exactly_the_documented_discriminants() {
    let mut accepted = Vec::new();
    for byte in 0u8..=255 {
        if Opcode::try_from(byte).is_ok() {
            accepted.push(byte);
        }
    }
    assert_eq!(accepted.len(), EXPECTED_OPCODE_COUNT);
    assert_eq!(
        accepted, KNOWN_DISCRIMINANTS,
        "TryFrom drifted from the hardcoded list"
    );
}

/// Exhaustive: all 256 first bytes × assorted payload words. `.op()` must be
/// `Some` iff the byte is assigned; accessors and Display must agree.
#[test]
fn sweep_all_256_opcode_bytes_through_every_decode_path() {
    const PAYLOADS: [u32; 5] = [
        0x0000_0000,
        0xFFFF_FFFF,
        0xDEAD_BEEF,
        0x7FFF_7F7F,
        0x00FF_FFFF,
    ];

    for byte_val in 0..=256u32 {
        let byte = byte_val as u8;
        let known = Opcode::try_from(byte).ok();
        assert_eq!(
            known.is_some(),
            KNOWN_DISCRIMINANTS.contains(&byte),
            "byte {byte:#04x}: TryFrom and hardcoded list disagree"
        );

        for &payload in &PAYLOADS {
            // Payload patterns are operand-bit shapes; they must stay inside
            // the 24 operand bits so the top byte really is `byte`.
            let word = (u32::from(byte) << 24) | (payload & v12_bytecode::MAX_IMM24);
            let instr = Instr(word);

            // `.op()` returns Some exactly when the byte is a known Opcode.
            assert_eq!(instr.op(), known, "word {word:#010x}");

            // Operand decoding is total: any bits, no panic. Operands are
            // the low 24 bits; the top byte is the opcode.
            let (a, b, c) = (instr.a(), instr.b(), instr.c());
            assert_eq!(
                u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c),
                payload & v12_bytecode::MAX_IMM24
            );
            assert_eq!(u32::from(instr.imm16()), payload & 0xFFFF);
            assert_eq!(instr.imm24(), word & v12_bytecode::MAX_IMM24);

            // WideOp::try_decode on this word as a slice: defined result.
            match WideOp::try_decode(&[instr]) {
                Ok((op, width)) => {
                    assert_eq!(known, Some(Opcode::Wide), "only Wide headers decode");
                    let disc = usize::try_from(u32::from(c)).unwrap();
                    assert!(disc < WIDE_TOTAL_WORDS.len(), "disc {disc} unknown");
                    assert_eq!(width, WIDE_TOTAL_WORDS[disc] as usize, "word {word:#010x}");
                    // `RegExt` is the one variant that uses the header `a`
                    // slot (for its slot mask); all others carry registers
                    // exclusively in payload words.
                    if let WideOp::RegExt { mask, .. } = op {
                        assert_eq!(mask, a, "RegExt mask rides in header slot `a`");
                    }
                }
                Err(reason) => {
                    if known == Some(Opcode::Wide) {
                        // Known discriminant on a single-word slice can only
                        // fail on the missing payload; anything else is an
                        // unassigned discriminant in slot `c`.
                        if u32::from(c) <= 4 {
                            assert!(
                                reason.contains("missing payload"),
                                "truncated wide must report missing payload, got: {reason}"
                            );
                        } else {
                            assert!(
                                reason.contains("unknown discriminant"),
                                "unknown wide discriminant must be reported, got: {reason}"
                            );
                        }
                    } else {
                        assert!(
                            reason.contains("not Wide") || reason.contains("unknown discriminant"),
                            "unexpected decode error for {byte:#04x}: {reason}"
                        );
                    }
                }
            }

            // Display path on a single-word function: non-empty, never panics.
            let fb_one = FunctionBytecode::with_instructions(vec![instr], 1);
            let text = render(&fb_one);
            assert!(!text.is_empty());
            if known.is_none() {
                assert!(
                    text.contains(&format!(".word 0x{word:08x}")),
                    "unassigned byte {byte:#04x} must render as .word, got:\n{text}"
                );
            }
        }
    }
}

/// Seeded fuzz: ≥100k random u32 words through every decoding path.
#[test]
fn fuzz_100k_random_words_through_every_decode_path() {
    const SAMPLES: u64 = 100_001;
    let seed = 0xDEC0_DE12_u64;
    let mut rng = Rng::new(seed);

    for i in 0..SAMPLES {
        let word = rng.next_u32();
        let instr = Instr(word);
        let byte = (word >> 24) as u8;

        match instr.op() {
            Some(op) => {
                assert_eq!(u32::from(op as u8), u32::from(byte));
                // Roundtrip through the packers keeps every field.
                assert_eq!(Instr::new(op, instr.a(), instr.b(), instr.c()), instr);
                assert_eq!(
                    Instr::new_imm16(op, instr.a(), instr.imm16()).imm16(),
                    instr.imm16()
                );
                // imm24 packer masks to 24 bits by construction.
                let packed24 = Instr::new_imm24(op, instr.imm24());
                assert_eq!(packed24.op(), Some(op));
                assert_eq!(packed24.imm24(), instr.imm24());
            }
            None => assert!(!KNOWN_DISCRIMINANTS.contains(&byte), "sample {i}"),
        }

        // Random-length windows around the word exercise truncation paths.
        let window = 1 + rng.below(4) as usize;
        let mut words: Vec<Instr> = vec![instr];
        for _ in 1..window {
            words.push(Instr(rng.next_u32()));
        }
        match WideOp::try_decode(&words) {
            Ok((_, width)) => {
                assert!(
                    width == 2 || width == 3,
                    "decoded width {width} impossible for sample {i} ({word:#010x})"
                );
                assert!(width <= words.len());
            }
            Err(reason) => assert!(!reason.is_empty(), "errors must explain themselves"),
        }

        // Pool decoding is total over arbitrary indices.
        let mut pool = ConstantPool::new();
        let idx = pool.insert(Const::BigU64(rng.next_u64())).unwrap();
        let _ = pool.get(idx);
        let _ = pool.get(rng.next_u32() as u16); // arbitrary index: Option only
    }
}

/// Structured wide-op fuzz: encode → decode roundtrips exactly; truncated
/// sequences always error; trailing junk after a full sequence is ignored.
#[test]
fn structured_wide_op_roundtrip_and_truncation_fuzz() {
    const SAMPLES: u64 = 20_000;
    let mut rng = Rng::new(0x5EED_1DE4);
    for _ in 0..SAMPLES {
        let op = random_wide_op(&mut rng);
        let encoded = op.encode();

        // Full sequence decodes back to the identical value.
        let (back, width) = WideOp::try_decode(&encoded).expect("well-formed sequence decodes");
        assert_eq!(back, op);
        assert_eq!(width, encoded.len());

        // Truncated at every prefix length: error mentioning what's missing.
        for cut in 0..encoded.len() {
            let err = WideOp::try_decode(&encoded[..cut])
                .expect_err("prefix shorter than the full sequence must not decode");
            assert!(
                err.contains("missing") || err.contains("header"),
                "truncation to {cut} reported: {err}"
            );
        }

        // Padding beyond the declared width is ignored by the decoder.
        let padded = encoded.clone();
        let mut junk = padded;
        junk.push(Instr(rng.next_u32()));
        let (_, width_junk) = WideOp::try_decode(&junk).unwrap();
        assert_eq!(width_junk, encoded.len(), "padding must not extend the op");

        // Header slot `b` is documented as zero but the decoder ignores it:
        // pin that as defined behavior rather than accidental lenience.
        let mut odd_header = encoded.clone();
        odd_header[0].0 |= 0x0000_AB00; // set slot b without touching a/c
        let (back_odd, _) = WideOp::try_decode(&odd_header).unwrap();
        assert_eq!(back_odd, op, "slot b in the wide header is don't-care");
    }
}

/// Bidirectional drift guard for the hardcoded inventory: every discriminant
/// the enum actually assigns must be listed, and every listed discriminant
/// must actually be assigned. Catches renumberings that keep the count
/// constant — which the exhaustive sweep alone would miss.
#[test]
fn known_discriminants_exactly_match_opcode_enum() {
    for d in 0u8..=255 {
        let assigned = Instr((u32::from(d) << 24)).op().is_some();
        let listed = KNOWN_DISCRIMINANTS.contains(&d);
        assert_eq!(
            assigned, listed,
            "discriminant {d}: enum says assigned={assigned}, KNOWN_DISCRIMINANTS says listed={listed}"
        );
    }
    assert_eq!(
        KNOWN_DISCRIMINANTS.len(),
        EXPECTED_OPCODE_COUNT,
        "list length drifted from EXPECTED_OPCODE_COUNT"
    );
}

/// Unknown discriminants on a real Wide header produce the documented
/// "unknown discriminant" diagnostic.
#[test]
fn wide_headers_with_unknown_discriminants_error_cleanly() {
    // Discriminants 11.. are unassigned (7..10 name RegExt, ClosureW,
    // NewEnvironmentW, and ConstructW).
    for disc in [11u32, 31, 99, 255] {
        let words = [
            Instr::new_imm24(Opcode::Wide, disc),
            Instr(0x1234_5678),
            Instr(0x9ABC_DEF0),
        ];
        let err = WideOp::try_decode(&words).expect_err("unassigned discriminant");
        assert!(err.contains("unknown discriminant"), "{err}");
        assert!(err.contains(&format!("{disc:#x}")), "{err}");
    }
}
