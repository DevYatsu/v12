//! Shared helpers for the v12-bytecode integration test binaries.
//!
//! Everything here is deterministic: randomness comes from a seeded
//! splitmix64 PRNG so any failure reproduces exactly from the seed printed
//! by the failing assertion.

#![allow(dead_code)] // each test binary links only the helpers it uses

use std::fmt;

use v12_bytecode::{Const, ConstantPool, FunctionBytecode, HandlerRange, Instr, Opcode, WideOp};

/// The 57 assigned opcode discriminants. Hardcoded on purpose: if someone
/// adds or renumbers an opcode, the exhaustive sweep fails until this list
/// is updated alongside [`Opcode`].
pub const KNOWN_DISCRIMINANTS: &[u8] = &[
    1, 2, 3, 4, // Move, LoadConst, LoadInt, Wide
    10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, // Add .. BitNot
    24, 25, 26, 27, 28, 29, 30, 31, 32, 33, // Eq .. TypeOf
    34, 35, 36, 37, // Jump, JumpIfFalse, JumpIfTrue, LoopHeader
    38, 39, 40, // Call, Return, Throw
    41, 42, 43, 44, 45, 46, 47, 48, 49, // GetProperty .. SetEnvSlot
    50, 51, 52, // CreateGenerator, SuspendYield, Await
    53, 54, // In, InstanceOf
    55, 56, 57, 58, 59, 60, 61, 62, // CopyArrayRest .. SetGlobal, Construct
];

pub const EXPECTED_OPCODE_COUNT: usize = 57;

/// Seeded splitmix64. Deterministic across platforms and runs.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform-ish value in `0..n`; `n` must be nonzero. Modulo bias is
    /// irrelevant at fuzz-test sample sizes.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn coin(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A `FunctionBytecode` with `len` filler instructions and room for
/// handlers whose targets stay in bounds (`len` words of code).
pub fn fn_with(len: u32, handlers: Vec<HandlerRange>) -> FunctionBytecode {
    let instrs: Vec<Instr> = (0..len)
        .map(|i| Instr::new(Opcode::Move, i as u8, 0, 1))
        .collect();
    FunctionBytecode {
        name_hint: None,
        max_regs: 2,
        spans: vec![(0, 0); len as usize],
        instrs,
        consts: ConstantPool::new(),
        handlers,
        pc_map: Vec::new(),
        is_strict: false,
        fixed_params: 0,
        has_rest: false,
        rest_reg: 0,
    }
}

/// Wide payload word count per discriminant (header excluded).
pub const WIDE_PAYLOAD_WORDS: [u32; 11] = [2, 3, 2, 2, 2, 2, 2, 1, 1, 1, 2];

/// Total encoded width per discriminant (header included).
pub const WIDE_TOTAL_WORDS: [u32; 11] = [3, 4, 3, 3, 3, 3, 3, 2, 2, 2, 3];

/// Builds a random `WideOp` for structured roundtrip fuzzing.
pub fn random_wide_op(rng: &mut Rng) -> WideOp {
    match rng.below(11) {
        0 => WideOp::LoadConstW {
            dst: rng.next_u32() as u16,
            const_id: rng.next_u32(),
        },
        1 => WideOp::LoadIntW {
            dst: rng.next_u32() as u16,
            value: rng.next_u64() as i64,
        },
        2 => WideOp::GetEnvSlotW {
            dst: rng.next_u32() as u16,
            depth: rng.next_u32() as u16,
            slot: rng.next_u32() as u16,
        },
        3 => WideOp::SetEnvSlotW {
            src: rng.next_u32() as u16,
            depth: rng.next_u32() as u16,
            slot: rng.next_u32() as u16,
        },
        4 => WideOp::CallW {
            dst: rng.next_u32() as u16,
            func: rng.next_u32() as u16,
            argc: rng.next_u32() as u16,
        },
        5 => WideOp::CopyObjectRestW {
            dst: rng.next_u32() as u16,
            src: rng.next_u32() as u16,
            excl_base: rng.next_u32() as u16,
            excl_count: rng.next_u32() as u16,
        },
        6 => WideOp::CopyArrayRestW {
            dst: rng.next_u32() as u16,
            src: rng.next_u32() as u16,
            start: rng.next_u32() as u16,
        },
        7 => WideOp::RegExt {
            mask: rng.next_u32() as u8,
            a_hi: rng.next_u32() as u8,
            b_hi: rng.next_u32() as u8,
            c_hi: rng.next_u32() as u8,
        },
        8 => WideOp::ClosureW {
            dst: rng.next_u32() as u16,
            function_index: rng.next_u32() as u16,
        },
        9 => WideOp::NewEnvironmentW {
            depth: rng.next_u32() as u16,
            slots: rng.next_u32() as u16,
        },
        _ => WideOp::ConstructW {
            dst: rng.next_u32() as u16,
            func: rng.next_u32() as u16,
            argc: rng.next_u32() as u16,
        },
    }
}

/// Random constant of a random variant.
pub fn random_const(rng: &mut Rng) -> Const {
    match rng.below(5) {
        0 => Const::F64(f64::from_bits(rng.next_u64())),
        1 => Const::Str32(rng.next_u32()),
        2 => Const::BigIntId(rng.next_u32()),
        3 => Const::BigU64(rng.next_u64()),
        _ => Const::Null,
    }
}

/// One instruction whose opcode byte is guaranteed assigned but whose
/// operands are arbitrary garbage.
pub fn random_known_op_instr(rng: &mut Rng) -> Instr {
    let disc = KNOWN_DISCRIMINANTS[rng.below(KNOWN_DISCRIMINANTS.len() as u64) as usize];
    let operands = rng.next_u32() & 0x00FF_FFFF;
    Instr((u32::from(disc) << 24) | operands)
}

/// Assembles a pseudo-random function exercising every Display path:
/// unknown opcode bytes, well-formed and deliberately malformed wide
/// sequences, constants, and handler rows. Deterministic per seed.
pub struct RandomFunction {
    pub fb: FunctionBytecode,
}

pub fn random_function(rng: &mut Rng, min_words: usize) -> RandomFunction {
    let mut instrs: Vec<Instr> = Vec::new();
    while instrs.len() < min_words || (rng.coin(50) && instrs.len() < min_words + 16) {
        let roll = rng.below(100);
        if roll < 45 {
            // Fully raw word: may carry an unassigned opcode byte.
            instrs.push(Instr(rng.next_u32()));
        } else if roll < 60 {
            instrs.push(random_known_op_instr(rng));
        } else if roll < 90 {
            // Well-formed wide sequence, sometimes followed by junk padding
            // that the disassembler must still render.
            let op = random_wide_op(rng);
            instrs.extend(op.encode());
            if rng.coin(30) {
                instrs.push(Instr(rng.next_u32()));
            }
        } else {
            // Malformed wide header: known or unknown discriminant with
            // missing / insufficient payload.
            let disc = if rng.coin(50) {
                rng.below(6)
            } else {
                rng.below(256)
            } as u32;
            instrs.push(Instr::new(
                Opcode::Wide,
                rng.next_u32() as u8,
                rng.next_u32() as u8,
                disc as u8,
            ));
            if rng.coin(50) {
                instrs.push(Instr(rng.next_u32()));
            }
        }
    }

    let mut consts = ConstantPool::new();
    for _ in 0..rng.below(4) {
        let _ = consts.insert(random_const(rng));
    }
    let handlers = (0..rng.below(3))
        .map(|_| HandlerRange {
            start: rng.next_u32(),
            end: rng.next_u32(),
            target: rng.next_u32(),
            stack_depth: rng.next_u32(),
        })
        .collect();

    RandomFunction {
        fb: FunctionBytecode {
            name_hint: None,
            // Keep max_regs modest so Display stays readable and arithmetic
            // cannot overflow u16.
            max_regs: 1 + rng.below(4096) as u16,
            spans: vec![(0, 0); instrs.len()],
            instrs,
            consts,
            handlers,
            pc_map: Vec::new(),
            is_strict: rng.coin(50),
            fixed_params: 0,
            has_rest: false,
            rest_reg: 0,
        },
    }
}

/// Renders through the Display impl, mapping a formatting error to a panic
/// so tests fail loudly instead of silently skipping assertions.
pub fn render(fb: &FunctionBytecode) -> String {
    struct Writer<'a>(&'a mut String);
    impl fmt::Write for Writer<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0.push_str(s);
            Ok(())
        }
    }
    let mut out = String::new();
    use fmt::Write as _;
    write!(Writer(&mut out), "{fb}").expect("Display must not fail");
    out
}
