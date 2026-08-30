#![forbid(unsafe_code)]

//! Fixed-width bytecode for the v12 JavaScript engine.
//!
//! Every instruction is a single 32-bit word ([`Instr`]): bits 31..24 hold
//! the [`Opcode`], bits 23..16 / 15..8 / 7..0 hold operand slots `a`, `b`,
//! `c`. Operands wider than 8 bits travel as [`Opcode::Wide`] plus trailing
//! raw payload words ([`WideOp`]). Branches address absolute bytecode pcs so
//! [`FunctionBuilder`] can backpatch resolved labels in place.

use std::collections::HashMap;
use std::fmt;

/// Source span as `(start, end)` byte offsets. Shape-compatible with
/// `oxc_span::Span`'s start/end pair so front-end spans forward without a
/// conversion layer.
pub type SpanPair = (u32, u32);

// ---------------------------------------------------------------------------
// Stage 1: core instruction encoding
// ---------------------------------------------------------------------------

/// Bytecode opcodes. Discriminant values are part of the serialized format
/// and must never be renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    Move = 1,
    LoadConst = 2,
    LoadInt = 3,
    Wide = 4,
    Add = 10,
    Sub = 11,
    Mul = 12,
    Div = 13,
    Mod = 14,
    Pow = 15,
    Neg = 16,
    BitAnd = 17,
    BitOr = 18,
    BitXor = 19,
    Shl = 20,
    Shr = 21,
    UShr = 22,
    BitNot = 23,
    Eq = 24,
    Ne = 25,
    Lt = 26,
    Le = 27,
    Gt = 28,
    Ge = 29,
    StrictEq = 30,
    StrictNe = 31,
    Not = 32,
    TypeOf = 33,
    Jump = 34,
    JumpIfFalse = 35,
    JumpIfTrue = 36,
    LoopHeader = 37,
    Call = 38,
    Return = 39,
    Throw = 40,
    GetProperty = 41,
    SetProperty = 42,
    DeleteProperty = 43,
    NewObject = 44,
    NewArray = 45,
    Closure = 46,
    NewEnvironment = 47,
    GetEnvSlot = 48,
    SetEnvSlot = 49,
    CreateGenerator = 50,
    SuspendYield = 51,
    Await = 52,
    /// `key in obj` — `r{a} = (r{b} in r{c})` where `b` is key, `c` is object.
    /// Throws TypeError if `r{c}` is not an object; otherwise tests
    /// HasProperty walking the prototype chain after ToPropertyKey on `r{b}`.
    In = 53,
    /// `obj instanceof ctor` — `r{a} = (r{b} instanceof r{c})`.
    /// Throws TypeError if `r{c}` is not an object with an object-typed
    /// `prototype`; otherwise walks `r{b}`'s prototype chain for identity
    /// against `r{c}.prototype`.
    InstanceOf = 54,
    /// Array rest slice for destructuring: `r{a} = r{b}[c..]`.
    /// Throws TypeError if `r{b}` is not an array.
    CopyArrayRest = 55,
    /// Check that `r{a}` is an array, else throw TypeError for spread.
    CheckIsArray = 56,
    /// Call with args array: `r{a} = r{b}(...r{c})` where `r{c}` is an array of arguments, `this` is `r{b+1}`.
    CallApply = 57,
    /// Object rest copy placeholder (narrow form unused; wide form carries excluded list).
    CopyObjectRest = 58,
    /// Append spread array's elements to destination array.
    ArrayAppend = 59,
    /// Global property get: `r_a = global["name"]` where name is Str32 const id.
    GetGlobal = 60,
    /// Global property set: `global["name"] = r_a`.
    SetGlobal = 61,
    /// Constructor invocation (`new f(args)`): same register layout as
    /// [`Opcode::Call`] (`a` = dst/header base, `b` = callee reg, `c` = argc
    /// narrow form; see [`WideOp`] notes for wide encoding parity).
    ///
    /// Semantics implemented by executors: only constructors ([[Construct]])
    /// may be invoked. For a bytecode function the executor allocates an
    /// instance whose [[Prototype]] is `callee.prototype` (created on first
    /// use when absent), binds it as `this`, runs the body, and yields the
    /// returned object when the body returns one, otherwise the instance
    /// itself. Anything else throws TypeError "not a constructor".
    Construct = 62,
    /// ES ToNumber: `r{a} = ToNumber(r{b})`. Supports the unary `+`
    /// operator; boxes the numeric result (Smi or double).
    ToNumber = 63,
    /// Copies every enumerable own property of `r{c}` onto the object `r{b}`
    /// (object spread merge; later writes win). `r{a}` is unused.
    MergeObject = 64,
    /// Defines an accessor property: `r{a}` = object, `r{b}` = key,
    /// `r{c}` = packed `(getter_fn, setter_fn)` pair register base. The
    /// getter/setter are function objects (or `undefined` for absent).
    DefineAccessor = 65,
    /// Jumps to `target` (imm16) when `r{a}` is `null` or `undefined`.
    /// Supports optional chaining (`a?.b`) short-circuiting.
    JumpIfNullish = 66,
    /// Sets `r{b}`'s `[[Prototype]]` to `r{c}` (the class `extends` wiring;
    /// also used by `Object.setPrototypeOf`). `r{a}` is unused. Rejects
    /// primitive targets with a TypeError.
    SetPrototype = 67,
}

impl TryFrom<u8> for Opcode {
    type Error = u8;

    /// Inverse of the discriminant mapping; `Err` carries the unassigned byte.
    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            1 => Ok(Self::Move),
            2 => Ok(Self::LoadConst),
            3 => Ok(Self::LoadInt),
            4 => Ok(Self::Wide),
            10 => Ok(Self::Add),
            11 => Ok(Self::Sub),
            12 => Ok(Self::Mul),
            13 => Ok(Self::Div),
            14 => Ok(Self::Mod),
            15 => Ok(Self::Pow),
            16 => Ok(Self::Neg),
            17 => Ok(Self::BitAnd),
            18 => Ok(Self::BitOr),
            19 => Ok(Self::BitXor),
            20 => Ok(Self::Shl),
            21 => Ok(Self::Shr),
            22 => Ok(Self::UShr),
            23 => Ok(Self::BitNot),
            24 => Ok(Self::Eq),
            25 => Ok(Self::Ne),
            26 => Ok(Self::Lt),
            27 => Ok(Self::Le),
            28 => Ok(Self::Gt),
            29 => Ok(Self::Ge),
            30 => Ok(Self::StrictEq),
            31 => Ok(Self::StrictNe),
            32 => Ok(Self::Not),
            33 => Ok(Self::TypeOf),
            34 => Ok(Self::Jump),
            35 => Ok(Self::JumpIfFalse),
            36 => Ok(Self::JumpIfTrue),
            37 => Ok(Self::LoopHeader),
            38 => Ok(Self::Call),
            39 => Ok(Self::Return),
            40 => Ok(Self::Throw),
            41 => Ok(Self::GetProperty),
            42 => Ok(Self::SetProperty),
            43 => Ok(Self::DeleteProperty),
            44 => Ok(Self::NewObject),
            45 => Ok(Self::NewArray),
            46 => Ok(Self::Closure),
            47 => Ok(Self::NewEnvironment),
            48 => Ok(Self::GetEnvSlot),
            49 => Ok(Self::SetEnvSlot),
            50 => Ok(Self::CreateGenerator),
            51 => Ok(Self::SuspendYield),
            52 => Ok(Self::Await),
            53 => Ok(Self::In),
            54 => Ok(Self::InstanceOf),
            55 => Ok(Self::CopyArrayRest),
            56 => Ok(Self::CheckIsArray),
            57 => Ok(Self::CallApply),
            58 => Ok(Self::CopyObjectRest),
            59 => Ok(Self::ArrayAppend),
            60 => Ok(Self::GetGlobal),
            61 => Ok(Self::SetGlobal),
            62 => Ok(Self::Construct),
            63 => Ok(Self::ToNumber),
            64 => Ok(Self::MergeObject),
            65 => Ok(Self::DefineAccessor),
            66 => Ok(Self::JumpIfNullish),
            67 => Ok(Self::SetPrototype),
            other => Err(other),
        }
    }
}

/// Largest value storable in an instruction's 24-bit immediate field.
pub const MAX_IMM24: u32 = 0x00FF_FFFF;

/// Bit layout of an [`Instr`] word: the opcode occupies the top byte;
/// operand slots `a`, `b`, `c` follow from high to low.
const SHIFT_OPCODE: u32 = 24;
const SHIFT_A: u32 = 16;
const SHIFT_B: u32 = 8;
const OPCODE_MASK: u32 = 0xFF00_0000;
/// Masks off everything but the `a`/`b`/`c` slots (clears the opcode byte).
const LOW_16_MASK: u32 = 0x0000_FFFF;

/// One fixed-width instruction word.
///
/// Layout: bits 31..24 opcode, 23..16 `a`, 15..8 `b`, 7..0 `c`. The tuple
/// field stays public so executors can transcode words cheaply, but prefer
/// the constructors and accessors: they keep the bit layout in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instr(pub u32);

impl Instr {
    /// Packs an opcode plus three 8-bit operand slots.
    #[inline]
    pub fn new(op: Opcode, a: u8, b: u8, c: u8) -> Self {
        Self(
            (op as u32) << SHIFT_OPCODE
                | u32::from(a) << SHIFT_A
                | u32::from(b) << SHIFT_B
                | u32::from(c),
        )
    }

    /// Packs an opcode, an 8-bit slot `a` (typically a destination register)
    /// and a 16-bit immediate split big-endian across `b` (high byte) and
    /// `c` (low byte).
    ///
    /// Named `new_imm16` rather than `imm16` because Rust forbids
    /// overloading: the zero-arg `.imm16()` accessor owns that name.
    #[inline]
    pub fn new_imm16(op: Opcode, a: u8, imm16: u16) -> Self {
        Self::new(op, a, (imm16 >> 8) as u8, imm16 as u8)
    }

    /// Packs an opcode plus a 24-bit immediate occupying all three slots.
    #[inline]
    pub fn new_imm24(op: Opcode, imm24: u32) -> Self {
        debug_assert!(imm24 <= MAX_IMM24, "immediate {imm24:#x} exceeds 24 bits");
        Self((op as u32) << SHIFT_OPCODE | (imm24 & MAX_IMM24))
    }

    /// Decodes the opcode byte; `None` for bytes outside the assigned
    /// discriminant range (reachable via the public tuple field or corrupt
    /// bytecode).
    #[inline]
    pub fn op(self) -> Option<Opcode> {
        Opcode::try_from((self.0 >> SHIFT_OPCODE) as u8).ok()
    }

    #[inline]
    pub fn a(self) -> u8 {
        (self.0 >> SHIFT_A) as u8
    }

    #[inline]
    pub fn b(self) -> u8 {
        (self.0 >> SHIFT_B) as u8
    }

    #[inline]
    pub fn c(self) -> u8 {
        self.0 as u8
    }

    /// Reassembles the big-endian 16-bit immediate from slots `b`/`c`.
    #[inline]
    pub fn imm16(self) -> u16 {
        (u16::from(self.b()) << 8) | u16::from(self.c())
    }

    /// Reassembles the 24-bit immediate spanning slots `a`/`b`/`c`.
    #[inline]
    pub fn imm24(self) -> u32 {
        self.0 & MAX_IMM24
    }

    /// Rewrites the 16-bit immediate in place; used for label backpatching.
    #[inline]
    pub fn set_imm16(&mut self, imm16: u16) {
        self.0 = (self.0 & !LOW_16_MASK) | u32::from(imm16);
    }

    /// Rewrites the 24-bit immediate in place; used for label backpatching.
    #[inline]
    pub fn set_imm24(&mut self, imm24: u32) {
        self.0 = (self.0 & OPCODE_MASK) | (imm24 & MAX_IMM24);
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    /// Every opcode, so encoding tests cannot silently skip new variants.
    const ALL_OPS: &[Opcode] = &[
        Opcode::Move,
        Opcode::LoadConst,
        Opcode::LoadInt,
        Opcode::Wide,
        Opcode::Add,
        Opcode::Sub,
        Opcode::Mul,
        Opcode::Div,
        Opcode::Mod,
        Opcode::Pow,
        Opcode::Neg,
        Opcode::BitAnd,
        Opcode::BitOr,
        Opcode::BitXor,
        Opcode::Shl,
        Opcode::Shr,
        Opcode::UShr,
        Opcode::BitNot,
        Opcode::Eq,
        Opcode::Ne,
        Opcode::Lt,
        Opcode::Le,
        Opcode::Gt,
        Opcode::Ge,
        Opcode::StrictEq,
        Opcode::StrictNe,
        Opcode::Not,
        Opcode::TypeOf,
        Opcode::Jump,
        Opcode::JumpIfFalse,
        Opcode::JumpIfTrue,
        Opcode::LoopHeader,
        Opcode::Call,
        Opcode::Return,
        Opcode::Throw,
        Opcode::GetProperty,
        Opcode::SetProperty,
        Opcode::DeleteProperty,
        Opcode::NewObject,
        Opcode::NewArray,
        Opcode::Closure,
        Opcode::NewEnvironment,
        Opcode::GetEnvSlot,
        Opcode::SetEnvSlot,
        Opcode::CreateGenerator,
        Opcode::SuspendYield,
        Opcode::Await,
        Opcode::In,
        Opcode::InstanceOf,
        Opcode::CopyArrayRest,
        Opcode::CheckIsArray,
        Opcode::CallApply,
        Opcode::CopyObjectRest,
        Opcode::ArrayAppend,
        Opcode::GetGlobal,
        Opcode::SetGlobal,
        Opcode::Construct,
        Opcode::ToNumber,
        Opcode::MergeObject,
        Opcode::DefineAccessor,
        Opcode::JumpIfNullish,
        Opcode::SetPrototype,
    ];

    #[test]
    fn discriminants_are_stable_and_unique() {
        assert_eq!(Opcode::Move as u8, 1);
        assert_eq!(Opcode::Wide as u8, 4);
        assert_eq!(Opcode::Await as u8, 52);
        assert_eq!(Opcode::In as u8, 53);
        assert_eq!(Opcode::InstanceOf as u8, 54);
        assert_eq!(Opcode::CopyArrayRest as u8, 55);
        assert_eq!(Opcode::CheckIsArray as u8, 56);
        assert_eq!(Opcode::CallApply as u8, 57);
        assert_eq!(Opcode::CopyObjectRest as u8, 58);
        assert_eq!(Opcode::ArrayAppend as u8, 59);
        assert_eq!(Opcode::GetGlobal as u8, 60);
        assert_eq!(Opcode::SetGlobal as u8, 61);
        assert_eq!(Opcode::Construct as u8, 62);
        let unique: std::collections::HashSet<u8> = ALL_OPS.iter().map(|&op| op as u8).collect();
        assert_eq!(unique.len(), ALL_OPS.len());
    }

    #[test]
    fn all_opcodes_roundtrip_through_new() {
        for (idx, &op) in ALL_OPS.iter().enumerate() {
            let i = Instr::new(op, idx as u8, 0xAB, 0xCD);
            assert_eq!(i.op(), Some(op), "{op:?}");
            assert_eq!(i.a(), idx as u8);
            assert_eq!(i.b(), 0xAB);
            assert_eq!(i.c(), 0xCD);
        }
    }

    #[test]
    fn all_opcodes_roundtrip_through_imm16_big_endian() {
        for &op in ALL_OPS {
            let i = Instr::new_imm16(op, 7, 0xBEEF);
            assert_eq!(i.op(), Some(op));
            assert_eq!(i.a(), 7);
            // Big-endian split: high byte in `b`, low byte in `c`.
            assert_eq!(i.b(), 0xBE);
            assert_eq!(i.c(), 0xEF);
            assert_eq!(i.imm16(), 0xBEEF);
        }
    }

    #[test]
    fn all_opcodes_roundtrip_through_imm24() {
        for &op in ALL_OPS {
            let i = Instr::new_imm24(op, 0x00AB_CDEF);
            assert_eq!(i.op(), Some(op));
            assert_eq!(i.imm24(), 0x00AB_CDEF);
            assert_eq!(i.a(), 0xAB);
            assert_eq!(i.b(), 0xCD);
            assert_eq!(i.c(), 0xEF);
            assert_eq!((op as u32) << 24 | 0x00AB_CDEF, i.0);
        }
    }

    #[test]
    fn setters_rewrite_only_immediate_bits() {
        let mut i = Instr::new(Opcode::JumpIfTrue, 3, 0xFF, 0xFF);
        i.set_imm16(0x1234);
        assert_eq!(i.a(), 3, "cond register must survive imm16 patch");
        assert_eq!(i.imm16(), 0x1234);

        let mut j = Instr::new(Opcode::Jump, 0xAA, 0xBB, 0xCC);
        j.set_imm24(0x0005_4321);
        assert_eq!((j.0 >> 24) as u8, Opcode::Jump as u8);
        assert_eq!(j.imm24(), 0x0005_4321);
    }

    #[test]
    fn unassigned_opcode_bytes_decode_to_none() {
        assert_eq!(Instr(0x0000_0000).op(), None);
        assert_eq!(Instr(0xFF00_0000).op(), None);
        assert_eq!(Instr(0x0500_0000).op(), None); // gap between Wide and Add
    }
}

// ---------------------------------------------------------------------------
// Stage 2: wide operands
// ---------------------------------------------------------------------------

/// Operations whose operands exceed the three 8-bit slots.
///
/// Encoding: the header is always `Opcode::Wide`; its `a` slot carries a
/// variant-specific byte (0 for most variants, the slot mask for
/// [`WideOp::RegExt`]) and the **low byte of its imm24 field** (i.e. slot
/// `c`) carries the variant discriminant. Payload words follow raw.
///
/// Register and slot fields are uniformly u16, packed two-per-word as
/// `(hi << 16) | lo` mirroring [`Instr::new_imm16`]'s big-endian convention.
/// This is what lifts the per-function 255-register / 255-slot / 255-function
/// ceilings: narrow instructions keep byte operands, anything overflowing
/// them escapes here (or through [`WideOp::RegExt`], which prefixes a narrow
/// instruction with high bytes for its register slots).
///
/// | Variant           | Header `a` slot | Payload words                                       |
/// |-------------------|-----------------|-----------------------------------------------------|
/// | `LoadConstW`      | 0               | `dst << 16 \| const_id >> 16`, `const_id & 0xFFFF`   |
/// | `LoadIntW`        | 0               | value low u32, value high u32 (i64), `dst`           |
/// | `GetEnvSlotW`     | 0               | `dst << 16 \| depth`, `slot`                         |
/// | `SetEnvSlotW`     | 0               | `src << 16 \| depth`, `slot`                         |
/// | `CallW`           | 0               | `dst << 16 \| func`, `argc`                          |
/// | `ConstructW`      | 0               | `dst << 16 \| func`, `argc`                          |
/// | `CopyObjectRestW` | 0               | `dst << 16 \| src`, `excl_base << 16 \| excl_count`  |
/// | `CopyArrayRestW`  | 0               | `dst << 16 \| src`, `start`                          |
/// | `RegExt`          | slot mask       | `a_hi << 16 \| b_hi << 8 \| c_hi`                    |
/// | `ClosureW`        | 0               | `dst << 16 \| function_index`                        |
/// | `NewEnvironmentW` | 0               | `depth << 16 \| slots`                               |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WideOp {
    LoadConstW {
        dst: u16,
        const_id: u32,
    },
    LoadIntW {
        dst: u16,
        value: i64,
    },
    GetEnvSlotW {
        dst: u16,
        depth: u16,
        slot: u16,
    },
    SetEnvSlotW {
        src: u16,
        depth: u16,
        slot: u16,
    },
    CallW {
        dst: u16,
        func: u16,
        argc: u16,
    },
    ConstructW {
        dst: u16,
        func: u16,
        argc: u16,
    },
    CopyObjectRestW {
        dst: u16,
        src: u16,
        excl_base: u16,
        excl_count: u16,
    },
    CopyArrayRestW {
        dst: u16,
        src: u16,
        start: u16,
    },
    /// High bytes for register operands of the *next* narrow instruction.
    ///
    /// The compiler emits `[RegExt][payload][narrow instr]` when one or more
    /// register operands of a narrow op exceed `u8::MAX`. `mask` (in the
    /// header's `a` slot) says which of the narrow instruction's `a`/`b`/`c`
    /// slots are registers extended here: bit 0 → `a`, bit 1 → `b`, bit 2 →
    /// `c`. Slots without their mask bit keep the narrow instruction's byte
    /// value (immediate slots are never masked, so e.g. `LoadConst`'s 16-bit
    /// const id survives untouched).
    ///
    /// The interpreter executes the header, payload, and narrow instruction
    /// as one logical op (3 words). The pair is always emitted atomically by
    /// the compiler, so no label or handler target can land between them.
    RegExt {
        mask: u8,
        a_hi: u8,
        b_hi: u8,
        c_hi: u8,
    },
    /// `Closure dst, fn#function_index` with a 16-bit function index — lifts
    /// the 255-functions-per-program ceiling of narrow `Closure`.
    ClosureW {
        dst: u16,
        function_index: u16,
    },
    /// `NewEnvironment depth, slots` with 16-bit operands — lifts the
    /// 255-environment-slots ceiling of narrow `NewEnvironment`.
    NewEnvironmentW {
        depth: u16,
        slots: u16,
    },
}

fn pack_hi_lo(hi: u16, lo: u16) -> u32 {
    (u32::from(hi) << 16) | u32::from(lo)
}

fn unpack_hi_lo(word: u32) -> (u16, u16) {
    ((word >> 16) as u16, word as u16)
}

impl WideOp {
    /// Discriminants are serialized in the header's low imm24 byte; fix them.
    pub const DISC_LOAD_CONST_W: u32 = 0;
    pub const DISC_LOAD_INT_W: u32 = 1;
    pub const DISC_GET_ENV_SLOT_W: u32 = 2;
    pub const DISC_SET_ENV_SLOT_W: u32 = 3;
    pub const DISC_CALL_W: u32 = 4;
    pub const DISC_COPY_OBJECT_REST_W: u32 = 5;
    pub const DISC_COPY_ARRAY_REST_W: u32 = 6;
    pub const DISC_REG_EXT: u32 = 7;
    pub const DISC_CLOSURE_W: u32 = 8;
    pub const DISC_NEW_ENVIRONMENT_W: u32 = 9;
    pub const DISC_CONSTRUCT_W: u32 = 10;

    /// Serializes to a header word plus the documented payload words.
    ///
    /// Header layout: `Opcode::Wide`, variant-specific byte in slot `a`,
    /// discriminant in the imm24 field's low byte (slot `c`); slot `b` stays
    /// zero.
    pub fn encode(self) -> Vec<Instr> {
        let hdr = |a: u8, disc: u32| Instr::new(Opcode::Wide, a, (disc >> 8) as u8, disc as u8);
        match self {
            Self::LoadConstW { dst, const_id } => vec![
                hdr(0, Self::DISC_LOAD_CONST_W),
                Instr(pack_hi_lo(dst, (const_id >> 16) as u16)),
                Instr(const_id & 0xFFFF),
            ],
            Self::LoadIntW { dst, value } => {
                let bits = value as u64; // two's complement split low/high
                vec![
                    hdr(0, Self::DISC_LOAD_INT_W),
                    Instr(bits as u32),
                    Instr((bits >> 32) as u32),
                    Instr(u32::from(dst)),
                ]
            }
            Self::GetEnvSlotW { dst, depth, slot } => vec![
                hdr(0, Self::DISC_GET_ENV_SLOT_W),
                Instr(pack_hi_lo(dst, depth)),
                Instr(u32::from(slot)),
            ],
            Self::SetEnvSlotW { src, depth, slot } => vec![
                hdr(0, Self::DISC_SET_ENV_SLOT_W),
                Instr(pack_hi_lo(src, depth)),
                Instr(u32::from(slot)),
            ],
            Self::CallW { dst, func, argc } | Self::ConstructW { dst, func, argc } => {
                let disc = match self {
                    Self::CallW { .. } => Self::DISC_CALL_W,
                    _ => Self::DISC_CONSTRUCT_W,
                };
                vec![
                    hdr(0, disc),
                    Instr(pack_hi_lo(dst, func)),
                    Instr(u32::from(argc)),
                ]
            }
            Self::CopyObjectRestW {
                dst,
                src,
                excl_base,
                excl_count,
            } => vec![
                hdr(0, Self::DISC_COPY_OBJECT_REST_W),
                Instr(pack_hi_lo(dst, src)),
                Instr(pack_hi_lo(excl_base, excl_count)),
            ],
            Self::CopyArrayRestW { dst, src, start } => vec![
                hdr(0, Self::DISC_COPY_ARRAY_REST_W),
                Instr(pack_hi_lo(dst, src)),
                Instr(u32::from(start)),
            ],
            Self::RegExt {
                mask,
                a_hi,
                b_hi,
                c_hi,
            } => vec![
                hdr(mask, Self::DISC_REG_EXT),
                Instr((u32::from(a_hi) << 16) | (u32::from(b_hi) << 8) | u32::from(c_hi)),
            ],
            Self::ClosureW {
                dst,
                function_index,
            } => vec![
                hdr(0, Self::DISC_CLOSURE_W),
                Instr(pack_hi_lo(dst, function_index)),
            ],
            Self::NewEnvironmentW { depth, slots } => vec![
                hdr(0, Self::DISC_NEW_ENVIRONMENT_W),
                Instr(pack_hi_lo(depth, slots)),
            ],
        }
    }

    /// Decodes a wide op from the front of `words`, returning the op and its
    /// total word count (header included). Errors cover truncation, a
    /// non-Wide header, and unknown discriminants.
    pub fn try_decode(words: &[Instr]) -> Result<(Self, usize), String> {
        let Some(header) = words.first() else {
            return Err("wide op: missing header word".into());
        };
        if header.op() != Some(Opcode::Wide) {
            return Err(format!(
                "wide op: header opcode is {:?}, not Wide",
                header.op()
            ));
        }
        // Closure keeps payload indexing tied to the documented layout.
        let payload = |i: usize| -> Result<u32, String> {
            words
                .get(i)
                .map(|w| w.0)
                .ok_or_else(|| format!("wide op: missing payload word {i}"))
        };
        match u32::from(header.c()) {
            Self::DISC_LOAD_CONST_W => {
                let (dst, const_hi) = unpack_hi_lo(payload(1)?);
                let const_lo = payload(2)?;
                Ok((
                    Self::LoadConstW {
                        dst,
                        const_id: (u32::from(const_hi) << 16) | const_lo,
                    },
                    3,
                ))
            }
            Self::DISC_LOAD_INT_W => {
                let lo = u64::from(payload(1)?);
                let hi = u64::from(payload(2)?);
                let dst = payload(3)?;
                Ok((
                    Self::LoadIntW {
                        dst: dst as u16,
                        value: ((hi << 32) | lo) as i64,
                    },
                    4,
                ))
            }
            Self::DISC_GET_ENV_SLOT_W => {
                let (dst, depth) = unpack_hi_lo(payload(1)?);
                let slot = payload(2)? as u16;
                Ok((Self::GetEnvSlotW { dst, depth, slot }, 3))
            }
            Self::DISC_SET_ENV_SLOT_W => {
                let (src, depth) = unpack_hi_lo(payload(1)?);
                let slot = payload(2)? as u16;
                Ok((Self::SetEnvSlotW { src, depth, slot }, 3))
            }
            Self::DISC_CALL_W | Self::DISC_CONSTRUCT_W => {
                let (dst, func) = unpack_hi_lo(payload(1)?);
                let argc = payload(2)? as u16;
                let op = if u32::from(header.c()) == Self::DISC_CALL_W {
                    Self::CallW { dst, func, argc }
                } else {
                    Self::ConstructW { dst, func, argc }
                };
                Ok((op, 3))
            }
            Self::DISC_COPY_OBJECT_REST_W => {
                let (dst, src) = unpack_hi_lo(payload(1)?);
                let (excl_base, excl_count) = unpack_hi_lo(payload(2)?);
                Ok((
                    Self::CopyObjectRestW {
                        dst,
                        src,
                        excl_base,
                        excl_count,
                    },
                    3,
                ))
            }
            Self::DISC_COPY_ARRAY_REST_W => {
                let (dst, src) = unpack_hi_lo(payload(1)?);
                let start = payload(2)? as u16;
                Ok((Self::CopyArrayRestW { dst, src, start }, 3))
            }
            Self::DISC_REG_EXT => {
                let ext = payload(1)?;
                Ok((
                    Self::RegExt {
                        mask: header.a(),
                        a_hi: (ext >> 16) as u8,
                        b_hi: (ext >> 8) as u8,
                        c_hi: ext as u8,
                    },
                    2,
                ))
            }
            Self::DISC_CLOSURE_W => {
                let (dst, function_index) = unpack_hi_lo(payload(1)?);
                Ok((
                    Self::ClosureW {
                        dst,
                        function_index,
                    },
                    2,
                ))
            }
            Self::DISC_NEW_ENVIRONMENT_W => {
                let (depth, slots) = unpack_hi_lo(payload(1)?);
                Ok((Self::NewEnvironmentW { depth, slots }, 2))
            }
            other => Err(format!("wide op: unknown discriminant {other:#x}")),
        }
    }
}

impl fmt::Display for WideOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LoadConstW { dst, const_id } => write!(f, "load_const_w r{dst}, k{const_id}"),
            Self::LoadIntW { dst, value } => write!(f, "load_int_w r{dst}, #{value}"),
            Self::GetEnvSlotW { dst, depth, slot } => {
                write!(f, "get_env_slot_w r{dst}, depth={depth} slot={slot}")
            }
            Self::SetEnvSlotW { src, depth, slot } => {
                write!(f, "set_env_slot_w r{src}, depth={depth} slot={slot}")
            }
            Self::CallW { dst, func, argc } => write!(f, "call_w r{dst}, r{func}, argc={argc}"),
            Self::ConstructW { dst, func, argc } => {
                write!(f, "construct_w r{dst}, r{func}, argc={argc}")
            }
            Self::CopyObjectRestW {
                dst,
                src,
                excl_base,
                excl_count,
            } => write!(
                f,
                "copy_object_rest_w r{dst}, r{src}, excl_base=r{excl_base} count={excl_count}"
            ),
            Self::CopyArrayRestW { dst, src, start } => {
                write!(f, "copy_array_rest_w r{dst}, r{src}, start={start}")
            }
            Self::RegExt {
                mask,
                a_hi,
                b_hi,
                c_hi,
            } => write!(
                f,
                "reg_ext mask={mask:#04x} a_hi={a_hi:#04x} b_hi={b_hi:#04x} c_hi={c_hi:#04x}"
            ),
            Self::ClosureW {
                dst,
                function_index,
            } => write!(f, "closure_w r{dst}, fn#{function_index}"),
            Self::NewEnvironmentW { depth, slots } => {
                write!(f, "new_environment_w depth={depth}, slots={slots}")
            }
        }
    }
}

#[cfg(test)]
mod wide_tests {
    use super::*;

    #[test]
    fn wide_ops_roundtrip_through_encode_decode() {
        let ops = [
            WideOp::LoadConstW {
                dst: 3,
                const_id: 0xDEAD_BEEF,
            },
            WideOp::LoadConstW {
                dst: 300,
                const_id: 7,
            },
            WideOp::LoadIntW { dst: 1, value: -5 },
            WideOp::LoadIntW {
                dst: 9,
                value: i64::MIN,
            },
            WideOp::LoadIntW {
                dst: 300,
                value: 1000,
            },
            WideOp::GetEnvSlotW {
                dst: 2,
                depth: 0x1234,
                slot: 0x5678,
            },
            WideOp::GetEnvSlotW {
                dst: 300,
                depth: 0,
                slot: 300,
            },
            WideOp::SetEnvSlotW {
                src: 9,
                depth: 7,
                slot: u16::MAX,
            },
            WideOp::CallW {
                dst: 0,
                func: 4,
                argc: 1000,
            },
            WideOp::CallW {
                dst: 300,
                func: 260,
                argc: 300,
            },
            WideOp::ConstructW {
                dst: 301,
                func: 302,
                argc: 256,
            },
            WideOp::CopyObjectRestW {
                dst: 1,
                src: 2,
                excl_base: 3,
                excl_count: 7,
            },
            WideOp::CopyArrayRestW {
                dst: 4,
                src: 5,
                start: 258,
            },
            WideOp::RegExt {
                mask: 0b011,
                a_hi: 1,
                b_hi: 2,
                c_hi: 0,
            },
            WideOp::ClosureW {
                dst: 300,
                function_index: 257,
            },
            WideOp::NewEnvironmentW {
                depth: 0,
                slots: 300,
            },
        ];
        for op in ops {
            let words = op.encode();
            assert_eq!(
                words[0].op(),
                Some(Opcode::Wide),
                "{op:?} header must be Wide"
            );
            let (back, width) = WideOp::try_decode(&words).unwrap();
            assert_eq!(back, op);
            assert_eq!(width, words.len());
        }
    }

    #[test]
    fn wide_word_layout_matches_docs() {
        let lc = WideOp::LoadConstW {
            dst: 6,
            const_id: 0xABCD_1234,
        }
        .encode();
        assert_eq!(lc.len(), 3);
        // Register/id pairs pack hi|lo; the const id splits hi, lo.
        assert_eq!(lc[1].0, 0x0006_ABCD);
        assert_eq!(lc[2].0, 0x1234);

        let env = WideOp::GetEnvSlotW {
            dst: 0x00AA,
            depth: 0x00BB,
            slot: 0x00CC,
        }
        .encode();
        assert_eq!(env.len(), 3);
        assert_eq!(env[1].0, 0x00AA_00BB);
        assert_eq!(env[2].0, 0x00CC);

        let call = WideOp::CallW {
            dst: 0x11,
            func: 0x22,
            argc: 0x3344,
        }
        .encode();
        assert_eq!(call.len(), 3);
        assert_eq!(call[1].0, 0x0011_0022);
        assert_eq!(call[2].0, 0x3344);

        let li = WideOp::LoadIntW { dst: 0, value: 1 }.encode();
        assert_eq!(li.len(), 4);
        assert_eq!((li[1].0, li[2].0, li[3].0), (1, 0, 0)); // low, high, dst

        // RegExt: mask rides in the header `a` slot, high bytes in payload 1.
        let rex = WideOp::RegExt {
            mask: 0b101,
            a_hi: 0x12,
            b_hi: 0x34,
            c_hi: 0x56,
        }
        .encode();
        assert_eq!(rex.len(), 2);
        assert_eq!(rex[0].a(), 0b101);
        assert_eq!(u32::from(rex[0].c()), WideOp::DISC_REG_EXT);
        assert_eq!(rex[1].0, 0x0012_3456);
    }

    #[test]
    fn wide_decode_rejects_truncated_and_unknown() {
        assert!(WideOp::try_decode(&[]).is_err());

        let hdr_only = [Instr::new_imm24(Opcode::Wide, WideOp::DISC_CALL_W)];
        assert!(WideOp::try_decode(&hdr_only).is_err());

        let not_wide = [Instr::new(Opcode::Add, 0, 0, 0)];
        assert!(WideOp::try_decode(&not_wide).is_err());

        let unknown = [Instr::new_imm24(Opcode::Wide, 99), Instr(0)];
        assert!(WideOp::try_decode(&unknown).is_err());
    }
}

// ---------------------------------------------------------------------------
// Stage 3: data structures
// ---------------------------------------------------------------------------

/// A pooled constant.
///
/// Strings and BigInts are referenced by interner id rather than stored
/// inline, which keeps the pool `Copy`-able per entry and its indices
/// fixed-width.
///
/// `Null` has no payload — a single discriminant byte suffices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Const {
    F64(f64),
    Str32(u32),
    BigIntId(u32),
    BigU64(u64),
    /// The `null` singleton (no payload).
    Null,
}

impl Const {
    /// Dedup key: discriminant tag plus the payload's exact bits, so
    /// `-0.0`/`0.0` stay distinct while identical NaN payloads dedup.
    /// `Null` has no payload — all instances share one key.
    fn key(self) -> (u8, u64) {
        match self {
            Self::F64(v) => (0, v.to_bits()),
            Self::Str32(id) => (1, u64::from(id)),
            Self::BigIntId(id) => (2, u64::from(id)),
            Self::BigU64(v) => (3, v),
            Self::Null => (4, 0),
        }
    }
}

impl fmt::Display for Const {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::F64(v) => write!(f, "f64({v})"),
            Self::Str32(id) => write!(f, "str#{id}"),
            Self::BigIntId(id) => write!(f, "bigint#{id}"),
            Self::BigU64(v) => write!(f, "big_u64({v})"),
            Self::Null => write!(f, "null"),
        }
    }
}

/// Upper bound on pool size: constant indices must fit the instructions
/// that reference them (`LoadConst` carries a u8 id, `LoadConstW` a u32 one,
/// but the pool itself is indexed by u16).
pub const MAX_CONSTANTS: usize = 65_535;

/// Interning constant pool: inserting an equal constant returns the existing
/// index instead of growing the pool.
#[derive(Debug, Default, Clone)]
pub struct ConstantPool {
    consts: Vec<Const>,
    index: HashMap<(u8, u64), u16>,
}

impl ConstantPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the stable index of `constant`, inserting it if unseen.
    /// Errors once [`MAX_CONSTANTS`] is reached.
    pub fn insert(&mut self, constant: Const) -> Result<u16, String> {
        let key = constant.key();
        if let Some(&existing) = self.index.get(&key) {
            return Ok(existing);
        }
        if self.consts.len() >= MAX_CONSTANTS {
            return Err("constant pool full".into());
        }
        let idx = self.consts.len() as u16;
        self.consts.push(constant);
        self.index.insert(key, idx);
        Ok(idx)
    }

    pub fn get(&self, idx: u16) -> Option<Const> {
        self.consts.get(idx as usize).copied()
    }

    pub fn len(&self) -> usize {
        self.consts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.consts.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Const> + '_ {
        self.consts.iter().copied()
    }
}

/// One exception handler: protects bytecode pcs `[start, end)` and unwinds
/// to `target` with the operand stack truncated to `stack_depth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandlerRange {
    pub start: u32,
    pub end: u32,
    pub target: u32,
    pub stack_depth: u32,
}

/// Maps JIT code offsets back to bytecode pcs for deoptimization and
/// profiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcMapEntry {
    pub jit_pc: u32,
    pub bc_pc: u32,
}

/// Fully assembled bytecode for one function.
///
/// Register conventions: binary ops write `r{a} <- r{b} op r{c}`; jumps
/// store absolute pc targets; `Call` is `r{a} <- call r{b}, argc={c}`.
#[derive(Debug, Clone, Default)]
pub struct FunctionBytecode {
    pub name_hint: Option<String>,
    pub max_regs: u16,
    pub instrs: Vec<Instr>,
    pub consts: ConstantPool,
    pub handlers: Vec<HandlerRange>,
    /// Index-aligned with `instrs`; `(0, 0)` marks unknown spans.
    pub spans: Vec<SpanPair>,
    pub pc_map: Vec<PcMapEntry>,
    pub is_strict: bool,
    /// Number of fixed (non-rest) parameters. `0` when no parameters.
    pub fixed_params: u16,
    /// `true` when the function has a rest parameter.
    pub has_rest: bool,
    /// Register index of the rest parameter (valid when `has_rest`).
    ///
    /// u16: rest parameters sit right after the fixed params, which can pass
    /// 255 in functions with many parameters (u16 register addressing).
    pub rest_reg: u16,
    pub is_generator: bool,
    pub is_async: bool,
    /// Arrow functions lack a `prototype` property (they are not
    /// constructible). Set from the compiler's unit plan.
    pub is_arrow: bool,
}

impl FunctionBytecode {
    /// Minimal constructor for tests and fuzz helpers: fills `spans` with
    /// `(0, 0)` placeholders and leaves all optional fields at defaults.
    pub fn with_instructions(instrs: Vec<Instr>, max_regs: u16) -> Self {
        let n = instrs.len();
        Self {
            name_hint: None,
            max_regs,
            spans: vec![(0, 0); n],
            instrs,
            consts: ConstantPool::new(),
            handlers: Vec::new(),
            pc_map: Vec::new(),
            is_strict: false,
            fixed_params: 0,
            has_rest: false,
            rest_reg: 0,
            is_generator: false,
            is_async: false,
            is_arrow: false,
        }
    }

    /// Checks the structural invariants the interpreter relies on:
    ///
    /// - `max_regs > 0` (registers index from 0),
    /// - every handler range is non-empty and handlers are sorted by `start`,
    /// - ranges may nest but never partially overlap, and a nested handler
    ///   runs at a strictly greater `stack_depth` so unwinding pops frames
    ///   in a well-defined order,
    /// - every handler target is a valid pc.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_regs == 0 {
            return Err("max_regs must be greater than zero".into());
        }
        // Stack of still-open ranges; sortedness makes this a nesting check.
        let mut open: Vec<&HandlerRange> = Vec::new();
        let mut prev_start = 0;
        for h in &self.handlers {
            if h.end <= h.start {
                return Err(format!(
                    "handler range [{}, {}) is empty or inverted",
                    h.start, h.end
                ));
            }
            if h.start < prev_start {
                return Err(format!(
                    "handler starting at {} is not sorted by start",
                    h.start
                ));
            }
            if h.target as usize >= self.instrs.len() {
                return Err(format!(
                    "handler target {} is out of bounds ({} instrs)",
                    h.target,
                    self.instrs.len()
                ));
            }
            while open.last().is_some_and(|top| top.end <= h.start) {
                open.pop();
            }
            if let Some(top) = open.last() {
                if h.end > top.end {
                    return Err(format!(
                        "handler [{}, {}) partially overlaps [{}, {}) without nesting",
                        h.start, h.end, top.start, top.end
                    ));
                }
                if h.stack_depth <= top.stack_depth {
                    return Err(format!(
                        "nested handler [{}, {}) has non-increasing stack depth ({} <= {})",
                        h.start, h.end, h.stack_depth, top.stack_depth
                    ));
                }
            }
            prev_start = h.start;
            open.push(h);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Stage 4: program-level structure
// ---------------------------------------------------------------------------

/// Compiled program: every function body plus the entry point.
///
/// `main` indexes the top-level script body in [`Program::functions`];
/// `Closure` instructions reference other entries of that same vector.
///
/// Lives in `v12-bytecode` so the interpreter and the embedding
/// facade can run pre-compiled programs without depending on the
/// front-end (`v12-bccompiler` / `oxc_*` / `lasso`). The compiler
/// re-exports this type for back-compat.
#[derive(Debug, Clone, Default)]
pub struct Program {
    pub functions: Vec<FunctionBytecode>,
    pub main: u32,
}

/// Names of standard intrinsics that are always present on the global object.
///
/// An unresolved `IdentifierReference` whose text is in this table is treated
/// as a global access (`GetGlobal`/`SetGlobal`) rather than a compile error.
/// The list is intentionally small for v1 — enough to cover `known-failures.md`
/// bucket 3 (`Object`, `Array`, `String`, `Number`, `Boolean`, `Math`, `JSON`,
/// `Error`, …) without claiming full spec coverage.
///
/// Lives in `v12-bytecode` so the compiler, the interpreter, and
/// the realm can all reference the same list without depending on
/// `v12-bccompiler`. The compiler's `model::GLOBAL_INTRINSICS` re-exports
/// this constant for back-compat.
pub const GLOBAL_INTRINSICS: &[&str] = &[
    "Object",
    "Array",
    "String",
    "Number",
    "Boolean",
    "Math",
    "JSON",
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "URIError",
    "EvalError",
    "Promise",
    "Symbol",
    "Map",
    "Set",
    "eval",
    "console",
    "globalThis",
];

/// Human-readable opcode name; exhaustive so a new variant fails to compile
/// until it gets a mnemonic.
pub fn mnemonic(op: Opcode) -> &'static str {
    match op {
        Opcode::Move => "move",
        Opcode::LoadConst => "load_const",
        Opcode::LoadInt => "load_int",
        Opcode::Wide => "wide",
        Opcode::Add => "add",
        Opcode::Sub => "sub",
        Opcode::Mul => "mul",
        Opcode::Div => "div",
        Opcode::Mod => "mod",
        Opcode::Pow => "pow",
        Opcode::Neg => "neg",
        Opcode::BitAnd => "bit_and",
        Opcode::BitOr => "bit_or",
        Opcode::BitXor => "bit_xor",
        Opcode::Shl => "shl",
        Opcode::Shr => "shr",
        Opcode::UShr => "ushr",
        Opcode::BitNot => "bit_not",
        Opcode::Eq => "eq",
        Opcode::Ne => "ne",
        Opcode::Lt => "lt",
        Opcode::Le => "le",
        Opcode::Gt => "gt",
        Opcode::Ge => "ge",
        Opcode::StrictEq => "strict_eq",
        Opcode::StrictNe => "strict_ne",
        Opcode::Not => "not",
        Opcode::TypeOf => "type_of",
        Opcode::Jump => "jump",
        Opcode::JumpIfFalse => "jump_if_false",
        Opcode::JumpIfTrue => "jump_if_true",
        Opcode::LoopHeader => "loop_header",
        Opcode::Call => "call",
        Opcode::Return => "return",
        Opcode::Throw => "throw",
        Opcode::GetProperty => "get_property",
        Opcode::SetProperty => "set_property",
        Opcode::DeleteProperty => "delete_property",
        Opcode::NewObject => "new_object",
        Opcode::NewArray => "new_array",
        Opcode::Closure => "closure",
        Opcode::NewEnvironment => "new_environment",
        Opcode::GetEnvSlot => "get_env_slot",
        Opcode::SetEnvSlot => "set_env_slot",
        Opcode::CreateGenerator => "create_generator",
        Opcode::SuspendYield => "suspend_yield",
        Opcode::Await => "await",
        Opcode::In => "in",
        Opcode::InstanceOf => "instance_of",
        Opcode::CopyArrayRest => "copy_array_rest",
        Opcode::CheckIsArray => "check_is_array",
        Opcode::CallApply => "call_apply",
        Opcode::CopyObjectRest => "copy_object_rest",
        Opcode::ArrayAppend => "array_append",
        Opcode::GetGlobal => "get_global",
        Opcode::SetGlobal => "set_global",
        Opcode::Construct => "construct",
        Opcode::ToNumber => "to_number",
        Opcode::MergeObject => "merge_object",
        Opcode::DefineAccessor => "define_accessor",
        Opcode::JumpIfNullish => "jump_if_nullish",
        Opcode::SetPrototype => "set_prototype",
    }
}

/// Formats one instruction's operands after the mnemonic; `Wide` is handled
/// by the caller because it consumes trailing words.
fn fmt_operands(f: &mut fmt::Formatter<'_>, op: Opcode, i: Instr) -> fmt::Result {
    let (a, b, c) = (i.a(), i.b(), i.c());
    match op {
        Opcode::Move => write!(f, " r{a}, r{b}"),
        Opcode::LoadConst => write!(f, " r{a}, k{}", i.imm16()),
        Opcode::LoadInt => write!(f, " r{a}, #{}", c as i8),
        Opcode::Wide => Ok(()), // caller decodes header + payload words
        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Mod
        | Opcode::Pow
        | Opcode::BitAnd
        | Opcode::BitOr
        | Opcode::BitXor
        | Opcode::Shl
        | Opcode::Shr
        | Opcode::UShr
        | Opcode::Eq
        | Opcode::Ne
        | Opcode::Lt
        | Opcode::Le
        | Opcode::Gt
        | Opcode::Ge
        | Opcode::StrictEq
        | Opcode::StrictNe => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::Neg | Opcode::BitNot | Opcode::Not | Opcode::TypeOf | Opcode::ToNumber => {
            write!(f, " r{a}, r{b}")
        }
        Opcode::MergeObject => write!(f, " r{b}, r{c}"),
        Opcode::DefineAccessor => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::SetPrototype => write!(f, " r{b}, r{c}"),
        Opcode::JumpIfNullish => write!(f, " r{a}, -> {}", i.imm16()),
        Opcode::Jump => write!(f, " -> {}", i.imm24()),
        Opcode::JumpIfFalse | Opcode::JumpIfTrue => write!(f, " r{a}, -> {}", i.imm16()),
        Opcode::LoopHeader => Ok(()),
        Opcode::Call | Opcode::Construct => write!(f, " r{a}, r{b}, argc={c}"),
        Opcode::Return | Opcode::Throw => write!(f, " r{a}"),
        Opcode::GetProperty | Opcode::DeleteProperty => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::SetProperty => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::NewObject => write!(f, " r{a}"),
        Opcode::NewArray => write!(f, " r{a}, r{b}, len={c}"),
        Opcode::Closure => write!(f, " r{a}, fn#{b}"),
        Opcode::NewEnvironment => write!(f, " depth={a}, slots={b}"),
        Opcode::GetEnvSlot => write!(f, " r{a}, depth={b}, slot={c}"),
        Opcode::SetEnvSlot => write!(f, " depth={a}, slot={b}, r{c}"),
        Opcode::CreateGenerator => write!(f, " r{a}, r{b}"),
        Opcode::SuspendYield => write!(f, " r{a}"),
        Opcode::Await => write!(f, " r{a}, r{b}"),
        Opcode::In | Opcode::InstanceOf => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::CopyArrayRest => write!(f, " r{a}, r{b}, start={c}"),
        Opcode::CheckIsArray => write!(f, " r{a}"),
        Opcode::CallApply => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::CopyObjectRest => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::ArrayAppend => write!(f, " r{a}, r{b}"),
        Opcode::GetGlobal => write!(f, " r{a}, k{}", i.imm16()),
        Opcode::SetGlobal => write!(f, " k{}, r{a}", i.imm16()),
    }
}

impl fmt::Display for FunctionBytecode {
    /// Disassembly listing: one instruction per line with pc prefix, plus
    /// the constant pool and handler tables. Malformed wide sequences and
    /// unknown opcode bytes render as diagnostics instead of panicking so a
    /// corrupt blob can always be inspected.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "function {}(max_regs={}){}",
            self.name_hint.as_deref().unwrap_or("<anon>"),
            self.max_regs,
            if self.is_strict { " [strict]" } else { "" }
        )?;
        for (idx, constant) in self.consts.iter().enumerate() {
            writeln!(f, "  k{idx} = {constant}")?;
        }
        let mut pc = 0;
        while pc < self.instrs.len() {
            let instr = self.instrs[pc];
            write!(f, "{pc:04}:")?;
            match instr.op() {
                Some(Opcode::Wide) => match WideOp::try_decode(&self.instrs[pc..]) {
                    Ok((wide_op, width)) => {
                        writeln!(f, " {wide_op}")?;
                        pc += width;
                        continue;
                    }
                    Err(reason) => write!(f, " wide <malformed: {reason}>")?,
                },
                Some(op) => {
                    write!(f, " {}", mnemonic(op))?;
                    fmt_operands(f, op, instr)?;
                }
                None => write!(f, " .word 0x{:08x}", instr.0)?,
            }
            writeln!(f)?;
            pc += 1;
        }
        if !self.handlers.is_empty() {
            writeln!(f, "handlers:")?;
            for h in &self.handlers {
                writeln!(
                    f,
                    "  [{}, {}) -> {} depth={}",
                    h.start, h.end, h.target, h.stack_depth
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod data_tests {
    use super::*;

    fn nop_fn(handlers: Vec<HandlerRange>) -> FunctionBytecode {
        let mut fb = FunctionBytecode::with_instructions(
            vec![Instr::new(Opcode::Move, 0, 0, 0); 4],
            2,
        );
        fb.handlers = handlers;
        fb
    }

    #[test]
    fn const_pool_dedups_equal_values() {
        let mut pool = ConstantPool::new();
        let first = pool.insert(Const::F64(1.5)).unwrap();
        let second = pool.insert(Const::F64(1.5)).unwrap();
        assert_eq!(first, second);
        assert_eq!(pool.len(), 1);

        let s1 = pool.insert(Const::Str32(42)).unwrap();
        let s2 = pool.insert(Const::Str32(42)).unwrap();
        assert_eq!(s1, s2);
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.get(first), Some(Const::F64(1.5)));
    }

    #[test]
    fn const_pool_keys_on_exact_bits() {
        let mut pool = ConstantPool::new();
        let pos_zero = pool.insert(Const::F64(0.0)).unwrap();
        let neg_zero = pool.insert(Const::F64(-0.0)).unwrap();
        assert_ne!(pos_zero, neg_zero, "-0.0 and 0.0 have different bits");

        let nan1 = pool.insert(Const::F64(f64::NAN)).unwrap();
        let nan2 = pool.insert(Const::F64(f64::NAN)).unwrap();
        assert_eq!(nan1, nan2, "identical NaN bit patterns dedup");
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn const_pool_caps_at_65535() {
        let mut pool = ConstantPool::new();
        for i in 0..MAX_CONSTANTS {
            pool.insert(Const::Str32(i as u32)).unwrap();
        }
        assert_eq!(pool.len(), MAX_CONSTANTS);
        assert!(pool.insert(Const::Str32(u32::MAX)).is_err());
        // Dedup still works at capacity.
        assert_eq!(pool.insert(Const::Str32(0)).unwrap(), 0);
    }

    #[test]
    fn validate_accepts_cleanly_nested_handlers() {
        let nested = vec![
            HandlerRange {
                start: 0,
                end: 20,
                target: 3,
                stack_depth: 2,
            },
            HandlerRange {
                start: 5,
                end: 10,
                target: 3,
                stack_depth: 3,
            },
            HandlerRange {
                start: 6,
                end: 8,
                target: 3,
                stack_depth: 4,
            },
        ];
        assert!(nop_fn(nested).validate().is_ok());
    }

    #[test]
    fn validate_rejects_unsorted_handlers() {
        let unsorted = vec![
            HandlerRange {
                start: 10,
                end: 20,
                target: 3,
                stack_depth: 1,
            },
            HandlerRange {
                start: 0,
                end: 5,
                target: 3,
                stack_depth: 2,
            },
        ];
        let err = nop_fn(unsorted).validate().unwrap_err();
        assert!(err.contains("not sorted"), "{err}");
    }

    #[test]
    fn validate_rejects_out_of_range_target() {
        let bad_target = vec![HandlerRange {
            start: 0,
            end: 4,
            target: 99,
            stack_depth: 1,
        }];
        let err = nop_fn(bad_target).validate().unwrap_err();
        assert!(err.contains("out of bounds"), "{err}");
    }

    #[test]
    fn validate_rejects_partial_overlap() {
        let partial = vec![
            HandlerRange {
                start: 0,
                end: 20,
                target: 3,
                stack_depth: 2,
            },
            HandlerRange {
                start: 5,
                end: 25,
                target: 3,
                stack_depth: 3,
            },
        ];
        let err = nop_fn(partial).validate().unwrap_err();
        assert!(err.contains("partially overlaps"), "{err}");
    }

    #[test]
    fn validate_rejects_non_increasing_nested_depth() {
        let flat = vec![
            HandlerRange {
                start: 0,
                end: 20,
                target: 3,
                stack_depth: 2,
            },
            HandlerRange {
                start: 5,
                end: 10,
                target: 3,
                stack_depth: 2,
            },
        ];
        let err = nop_fn(flat).validate().unwrap_err();
        assert!(err.contains("non-increasing stack depth"), "{err}");
    }

    #[test]
    fn validate_rejects_zero_max_regs() {
        let mut fb = nop_fn(Vec::new());
        fb.max_regs = 0;
        assert_eq!(
            fb.validate().unwrap_err(),
            "max_regs must be greater than zero"
        );
    }

    #[test]
    fn display_lists_mnemonics_consts_and_wide_ops() {
        let mut pool = ConstantPool::new();
        let k = pool.insert(Const::F64(1.5)).unwrap();
        let mut instrs = vec![
            Instr::new_imm16(Opcode::LoadConst, 0, k),
            Instr::new(Opcode::Add, 2, 0, 1),
        ];
        instrs.extend(
            WideOp::LoadConstW {
                dst: 3,
                const_id: 7,
            }
            .encode(),
        );
        instrs.push(Instr::new(Opcode::Return, 2, 0, 0));

        let mut fb = FunctionBytecode::with_instructions(instrs, 4);
        fb.name_hint = Some("smoke".into());
        fb.consts = pool;
        fb.handlers = vec![HandlerRange {
            start: 0,
            end: 4,
            target: 3,
            stack_depth: 1,
        }];
        fb.is_strict = true;
        let text = format!("{fb}");
        for needle in [
            "function smoke",
            "[strict]",
            "k0 = f64(1.5)",
            "load_const",
            "add",
            "load_const_w",
            "return",
            "handlers:",
            "-> 3",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 4: backpatching builder
// ---------------------------------------------------------------------------

/// A reference to a future instruction index.
///
/// Opaque by design: the builder owns pc assignment, so callers cannot hold
/// stale indices across emissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(u32);

/// One unresolved branch awaiting [`FunctionBuilder::bind`].
#[derive(Debug)]
struct Fixup {
    /// Index of the branch instruction to patch.
    at: u32,
    label: Label,
    /// `true` for `Jump` (24-bit target); conditional branches share their
    /// word with the condition register and carry a 16-bit target.
    wide_target: bool,
}

/// Incremental [`FunctionBytecode`] constructor with label backpatching.
///
/// Labels are allocated up front ([`FunctionBuilder::label`]), bound to "the
/// next emitted instruction" ([`FunctionBuilder::bind`]), and every branch
/// referencing them is rewritten in place during
/// [`FunctionBuilder::finish`] — the classic backpatching scheme, which lets
/// forward branches be emitted before their targets exist.
#[derive(Debug, Default)]
pub struct FunctionBuilder {
    name_hint: Option<String>,
    max_regs: u16,
    instrs: Vec<Instr>,
    spans: Vec<SpanPair>,
    consts: ConstantPool,
    handlers: Vec<HandlerRange>,
    labels: Vec<Option<u32>>,
    fixups: Vec<Fixup>,
    pub is_generator: bool,
    pub is_async: bool,
    pub is_arrow: bool,
}

impl FunctionBuilder {
    pub fn new(name_hint: Option<&str>) -> Self {
        Self {
            name_hint: name_hint.map(str::to_string),
            // Start at 1 so `validate`'s `max_regs > 0` holds for leaf thunks.
            max_regs: 1,
            ..Self::default()
        }
    }

    /// Grows the register budget; pass one more than the highest register
    /// index your emitted code touches.
    pub fn reserve_regs(&mut self, count: u16) {
        self.max_regs = self.max_regs.max(count);
    }

    /// Allocates an unbound label.
    pub fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() as u32 - 1)
    }

    /// Binds `label` to the index of the *next* emitted instruction.
    ///
    /// Panics on double binding: that is always a codegen bug, never a
    /// runtime condition.
    pub fn bind(&mut self, label: Label) {
        let slot = &mut self.labels[label.0 as usize];
        assert!(slot.is_none(), "label {label:?} bound more than once");
        *slot = Some(self.instrs.len() as u32);
    }

    /// Current emission pc; use it to compute [`HandlerRange`] bounds for
    /// [`FunctionBuilder::push_handler`].
    pub fn pc(&self) -> u32 {
        self.instrs.len() as u32
    }

    pub fn emit(&mut self, instr: Instr) {
        self.emit_spanned(instr, (0, 0));
    }

    /// Emits with a source span, keeping `spans` index-aligned with `instrs`.
    pub fn emit_spanned(&mut self, instr: Instr, span: SpanPair) {
        self.instrs.push(instr);
        self.spans.push(span);
    }

    /// Interns `constant`, deduplicating like [`ConstantPool::insert`].
    pub fn add_const(&mut self, constant: Const) -> Result<u16, String> {
        self.consts.insert(constant)
    }

    pub fn push_handler(&mut self, handler: HandlerRange) {
        self.handlers.push(handler);
    }

    /// Emits a branch to `target`. Unconditional `Jump` stores a 24-bit
    /// absolute target; conditional branches spend their remaining slots on
    /// `cond_reg` and carry a 16-bit absolute target instead.
    pub fn emit_jump(&mut self, op: Opcode, cond_reg: u8, target: Label) {
        debug_assert!(
            matches!(op, Opcode::Jump | Opcode::JumpIfFalse | Opcode::JumpIfTrue),
            "emit_jump expects a branching opcode"
        );
        let wide_target = op == Opcode::Jump;
        let instr = if wide_target {
            Instr::new_imm24(op, 0)
        } else {
            Instr::new_imm16(op, cond_reg, 0)
        };
        let at = self.instrs.len() as u32;
        self.emit(instr);
        self.fixups.push(Fixup {
            at,
            label: target,
            wide_target,
        });
    }

    /// Resolves every label and assembles the final bytecode.
    ///
    /// Panics if any label was never bound: an unresolved label means some
    /// branch points at nothing, which is always a codegen bug rather than
    /// a runtime condition worth recovering from.
    pub fn finish(mut self) -> FunctionBytecode {
        let unbound = self.labels.iter().filter(|slot| slot.is_none()).count();
        assert!(unbound == 0, "{unbound} label(s) created but never bound");

        let fixups = std::mem::take(&mut self.fixups);
        for fixup in fixups {
            // Every label is bound by the check above, so this lookup always
            // resolves; the `if let` merely avoids an unwrap.
            if let Some(target_pc) = self.labels[fixup.label.0 as usize] {
                let instr = &mut self.instrs[fixup.at as usize];
                if fixup.wide_target {
                    instr.set_imm24(target_pc);
                } else {
                    debug_assert!(
                        target_pc <= u32::from(u16::MAX),
                        "conditional branch target {target_pc} exceeds the 16-bit window"
                    );
                    instr.set_imm16(target_pc as u16);
                }
            }
        }
        debug_assert_eq!(self.spans.len(), self.instrs.len());

        FunctionBytecode {
            name_hint: self.name_hint,
            max_regs: self.max_regs,
            instrs: self.instrs,
            consts: self.consts,
            handlers: self.handlers,
            spans: self.spans,
            pc_map: Vec::new(),
            is_strict: false,
            fixed_params: 0,
            has_rest: false,
            rest_reg: 0,
            is_generator: self.is_generator,
            is_async: self.is_async,
            is_arrow: self.is_arrow,
        }
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    #[test]
    fn forward_jump_patches_to_bound_pc() {
        let mut b = FunctionBuilder::new(None);
        let done = b.label();
        b.emit_jump(Opcode::JumpIfFalse, 0, done);
        b.emit(Instr::new(Opcode::Move, 1, 2, 0));
        b.bind(done);
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));

        let fb = b.finish();
        assert_eq!(fb.instrs[0].op(), Some(Opcode::JumpIfFalse));
        assert_eq!(fb.instrs[0].a(), 0);
        assert_eq!(fb.instrs[0].imm16(), 2, "forward target is the Move's pc");
    }

    #[test]
    fn backward_jump_patches_to_loop_head() {
        let mut b = FunctionBuilder::new(None);
        let top = b.label();
        b.bind(top); // loop head at pc 0
        b.emit(Instr::new_imm24(Opcode::LoopHeader, 0));
        b.emit(Instr::new(Opcode::Add, 0, 0, 1));
        b.emit_jump(Opcode::JumpIfTrue, 2, top);

        let fb = b.finish();
        assert_eq!(fb.instrs[2].imm16(), 0, "backward target is the loop head");
    }

    #[test]
    fn unconditional_jump_uses_full_imm24_target() {
        let mut b = FunctionBuilder::new(None);
        let end = b.label();
        b.emit_jump(Opcode::Jump, 0, end);
        b.emit(Instr::new(Opcode::Move, 0, 0, 0));
        b.bind(end);

        let fb = b.finish();
        assert_eq!(fb.instrs[0].imm24(), 2, "end binds after the skipped Move");
    }

    #[test]
    fn diamond_control_flow_resolves_and_validates() {
        // if (r0) { r1 = 1 } else { r1 = 2 }; return r1
        let mut b = FunctionBuilder::new(Some("diamond"));
        b.reserve_regs(2);
        let else_l = b.label();
        let end_l = b.label();
        b.emit_jump(Opcode::JumpIfFalse, 0, else_l); // 0
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 1)); // 1: then
        b.emit_jump(Opcode::Jump, 0, end_l); // 2
        b.bind(else_l);
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 2)); // 3: else
        b.bind(end_l);
        b.emit(Instr::new(Opcode::Return, 1, 0, 0)); // 4

        let fb = b.finish();
        assert_eq!(fb.instrs[0].imm16(), 3, "false-branch lands on else");
        assert_eq!(fb.instrs[2].imm24(), 4, "then-branch skips the else");
        assert_eq!(fb.instrs.len(), 5);
        assert_eq!(fb.name_hint.as_deref(), Some("diamond"));
        assert!(fb.validate().is_ok());

        let text = format!("{fb}");
        for needle in [
            "jump_if_false r0, -> 3",
            "load_int r1, #1",
            "jump -> 4",
            "return r1",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    #[should_panic(expected = "never bound")]
    fn finish_panics_on_unbound_label() {
        let mut b = FunctionBuilder::new(None);
        let nowhere = b.label();
        b.emit_jump(Opcode::Jump, 0, nowhere);
        b.finish();
    }

    #[test]
    #[should_panic(expected = "bound more than once")]
    fn bind_twice_panics() {
        let mut b = FunctionBuilder::new(None);
        let l = b.label();
        b.bind(l);
        b.bind(l);
    }

    #[test]
    fn builder_consts_share_indices() {
        let mut b = FunctionBuilder::new(None);
        let k1 = b.add_const(Const::BigU64(7)).unwrap();
        let k2 = b.add_const(Const::BigU64(7)).unwrap();
        let k3 = b.add_const(Const::Str32(1)).unwrap();
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }

    #[test]
    fn emit_spanned_keeps_spans_parallel() {
        let mut b = FunctionBuilder::new(None);
        b.emit(Instr::new(Opcode::Move, 0, 1, 0));
        b.emit_spanned(Instr::new(Opcode::Return, 0, 0, 0), (10, 20));

        let fb = b.finish();
        assert_eq!(fb.spans, vec![(0, 0), (10, 20)]);
        assert_eq!(fb.spans.len(), fb.instrs.len());
    }

    #[test]
    fn handler_pushed_via_builder_validates() {
        let mut b = FunctionBuilder::new(Some("try"));
        b.reserve_regs(3);
        let start = b.pc(); // 0
        b.emit(Instr::new(Opcode::NewObject, 0, 0, 0)); // may throw
        let end = b.pc(); // 1
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));
        b.push_handler(HandlerRange {
            start,
            end,
            target: 2,
            stack_depth: 1,
        });
        b.emit(Instr::new(Opcode::Throw, 0, 0, 0)); // handler target at pc 2

        let fb = b.finish();
        assert_eq!(fb.handlers.len(), 1);
        assert!(fb.validate().is_ok());
    }
}

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
