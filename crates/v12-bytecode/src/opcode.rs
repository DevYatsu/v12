//! Stage 1: core instruction encoding — the `Opcode` enum and the
//! 32-bit `Instr` word (op + three operand slots).

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
    /// ES `new.target`: `r{a} = new.target`. Returns the constructor function
    /// that was invoked with `new`, or `undefined` when not in a constructor
    /// call (e.g., when the function was called directly). Arrow functions
    /// inherit `new.target` from their enclosing non-arrow function.
    GetNewTarget = 63,
    /// ES ToNumber: `r{a} = ToNumber(r{b})`. Supports the unary `+`
    /// operator; boxes the numeric result (Smi or double).
    ToNumber = 64,
    /// Copies every enumerable own property of `r{c}` onto the object `r{b}`
    /// (object spread merge; later writes win). `r{a}` is unused.
    MergeObject = 65,
    /// Defines an accessor property: `r{a}` = object, `r{b}` = key,
    /// `r{c}` = packed `(getter_fn, setter_fn)` pair register base. The
    /// getter/setter are function objects (or `undefined` for absent).
    DefineAccessor = 66,
    /// Jumps to `target` (imm16) when `r{a}` is `null` or `undefined`.
    /// Supports optional chaining (`a?.b`) short-circuiting.
    JumpIfNullish = 67,
    /// Sets `r{b}`'s `[[Prototype]]` to `r{c}` (the class `extends` wiring;
    /// also used by `Object.setPrototypeOf`). `r{a}` is unused. Rejects
    /// primitive targets with a TypeError.
    SetPrototype = 68,
    /// ES GetIterator: `r{a} = GetIterator(r{b})`. Reads the `@@iterator`
    /// method off `r{b}` (the realm's `Symbol.iterator` well-known symbol),
    /// calls it with `r{b}` as receiver, and validates that the result is an
    /// object. Throws TypeError when the method is missing or the result is
    /// not an object. Supports the `for-of` statement, spread, and `yield*`.
    GetIterator = 69,
    /// ES IteratorNext: `r{a} = IteratorNext(r{b})` — calls the iterator
    /// object's `next` method (property `"next"`) with the iterator as
    /// receiver and stores the result. `r{c}` is unused (kept for a future
    /// `IteratorNextValue` fused op).
    IteratorNext = 70,
    /// ES IteratorClose: `IteratorClose(r{a})`. Calls the iterator's
    /// `"return"` method (if any) when the loop exits abruptly (break /
    /// throw). `r{b}` and `r{c}` are unused.
    IteratorClose = 71,
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
            63 => Ok(Self::GetNewTarget),
            64 => Ok(Self::ToNumber),
            65 => Ok(Self::MergeObject),
            66 => Ok(Self::DefineAccessor),
            67 => Ok(Self::JumpIfNullish),
            68 => Ok(Self::SetPrototype),
            69 => Ok(Self::GetIterator),
            70 => Ok(Self::IteratorNext),
            71 => Ok(Self::IteratorClose),
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
        Opcode::GetNewTarget,
        Opcode::ToNumber,
        Opcode::MergeObject,
        Opcode::DefineAccessor,
        Opcode::JumpIfNullish,
        Opcode::SetPrototype,
        Opcode::GetIterator,
        Opcode::IteratorNext,
        Opcode::IteratorClose,
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
        assert_eq!(Opcode::GetNewTarget as u8, 63);
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
