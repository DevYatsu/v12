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
    /// ES IteratorClose: `IteratorClose(r{a}, r{b})`. Calls the iterator's
    /// `"return"` method (if any) when the loop exits abruptly (break /
    /// throw). `r{b}` is the completion flag: 0 = handler (throw) path —
    /// best-effort close, all close errors swallowed so the original abrupt
    /// wins; nonzero = normal break/return path — close errors propagate
    /// and a non-Object result throws (spec 7.4.6). `r{c}` is unused.
    IteratorClose = 71,
    /// Defines an own method property: `r{a}[r{b}] = r{c}` with spec method
    /// attributes (writable + configurable, non-enumerable). Unlike
    /// `SetProperty` this never walks the prototype chain and never invokes a
    /// setter.
    DefineMethod = 72,
    /// Lenient global read: like `GetGlobal` but a missing binding yields
    /// `undefined` instead of a `ReferenceError`. Emitted only where the spec
    /// mandates it (`typeof undeclared_global`). Same operand layout as
    /// `GetGlobal`.
    GetGlobalLenient = 73,
    /// Loads the current frame's generator pending-mode slot: `0` = normal
    /// resume (the value is a `next()` payload), `1` = return completion
    /// (`gen.return(v)` — the resume value is the return value). Emitted
    /// right after `SuspendYield` in generator bodies so the compiled return
    /// path can run the active finalizer copies before returning.
    GenResumeMode = 74,
    /// ES ToPropertyKey: `r{a} = ToPropertyKey(r{b})`. Materialises the property key so a key object's `toString`/`valueOf` runs at a defined point.
    ToPropertyKey = 75,
    /// ES GetIterator with the async hint: `r{a} = GetIterator(r{b}, async)`.
    /// Reads `r{b}[@@asyncIterator]` and calls it; when that method is
    /// absent, falls back to the sync `@@iterator` (the for-await lowering
    /// awaits the results itself). Validates that the result is an object.
    /// `r{c}` is unused.
    GetAsyncIterator = 76,
    /// ES 10.5.1 `[[GetPrototypeOf]]`: `r{a} = r{b}`'s prototype (`null`
    /// when there is none). A proxy receiver dispatches its `getPrototypeOf`
    /// trap; a primitive receiver throws a TypeError. `r{c}` is unused.
    /// Emitted by the `super.x` lowering in an object-literal method, which
    /// must start the lookup one link above the receiver instead of on it.
    GetPrototype = 77,
    /// Retires the current frame's innermost environment, restoring its
    /// parent. The exact inverse of [`Opcode::NewEnvironment`], which
    /// installs the new head with no way to take it back.
    ///
    /// A class scope (`class T extends X`) needs a real environment to carry
    /// the parent constructor to the constructor and method closures — and
    /// `Closure` captures `frame.env`, so the scope has to be the frame's
    /// innermost environment while those closures are built. Left unretired it
    /// then shadows the enclosing function's own environment for the rest of
    /// the unit: every later `GetEnvSlot`/`SetEnvSlot` at `depth=0`, and every
    /// closure created after it, resolves one hop too shallow.
    ///
    /// No operands.
    PopEnvironment = 78,
}

/// Every assigned [`Opcode`], in ascending discriminant order.
///
/// The single statement of *which bytes decode and to what*: [`TryFrom<u8>`]
/// inverts this list into a byte-indexed lookup at compile time, so a new
/// variant is added in exactly one place and the decoder cannot drift from it.
/// A duplicate discriminant here would collapse two variants onto one byte and
/// make one unreachable, so `discriminants_are_frozen` pins the whole list
/// against a frozen copy of the wire format.
///
/// Ordering is part of the contract, not a style choice: [`crate::all_opcodes`]
/// returns this list verbatim.
pub const ALL_OPCODES: &[Opcode] = &[
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
    Opcode::DefineMethod,
    Opcode::GetGlobalLenient,
    Opcode::GenResumeMode,
    Opcode::ToPropertyKey,
    Opcode::GetAsyncIterator,
    Opcode::GetPrototype,
    Opcode::PopEnvironment,
];

/// Number of assigned [`Opcode`] variants.
pub const OPCODE_COUNT: usize = ALL_OPCODES.len();

/// [`ALL_OPCODES`] inverted into a byte-indexed lookup; `None` at every
/// unassigned discriminant.
///
/// Built at compile time, so the decoder and the list it decodes are one
/// artefact. A `static` rather than a `const` would make this unreadable from
/// a `const fn` — and [`Opcode::from_byte`] is deliberately `const` so
/// compile-time consumers are not forced through `TryFrom` (which cannot be).
const OPCODE_BY_BYTE: [Option<Opcode>; 256] = invert_all_opcodes();

const fn invert_all_opcodes() -> [Option<Opcode>; 256] {
    let mut table = [None; 256];
    let mut i = 0;
    while i < ALL_OPCODES.len() {
        table[ALL_OPCODES[i] as usize] = Some(ALL_OPCODES[i]);
        i += 1;
    }
    table
}

impl Opcode {
    /// Decodes an opcode byte; `None` for an unassigned discriminant.
    ///
    /// `const`-callable, which [`TryFrom<u8>`] is not (a trait method cannot
    /// be). Every other decoder in the workspace should reach for this rather
    /// than rebuilding its own `match`.
    pub const fn from_byte(byte: u8) -> Option<Self> {
        OPCODE_BY_BYTE[byte as usize]
    }
}

impl TryFrom<u8> for Opcode {
    type Error = u8;

    /// Inverse of the discriminant mapping; `Err` carries the unassigned byte.
    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        Self::from_byte(byte).ok_or(byte)
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

    /// The wire format, frozen: every assigned opcode with its discriminant.
    ///
    /// A deliberate second copy of [`ALL_OPCODES`], because a stability test
    /// that read the live list would prove nothing — renumbering a variant
    /// updates the very list the decoder is built from. This one is a
    /// transcript of the *pre-W2.2* hand-written decoder's output, captured
    /// byte for byte over the whole `0..=255` range, so it detects a
    /// renumbering, a dropped arm, a newly-assigned gap, and a stale entry
    /// alike. It replaces the old `ALL_OPS` list, which was neither complete
    /// (it stopped at `ToPropertyKey` = 75) nor pinned to any byte.
    ///
    /// Adding an opcode means adding a row here, which is the point: the
    /// discriminants are the serialized format.
    const FROZEN_DISCRIMINANTS: &[(u8, Opcode)] = &[
        (1, Opcode::Move),
        (2, Opcode::LoadConst),
        (3, Opcode::LoadInt),
        (4, Opcode::Wide),
        (10, Opcode::Add),
        (11, Opcode::Sub),
        (12, Opcode::Mul),
        (13, Opcode::Div),
        (14, Opcode::Mod),
        (15, Opcode::Pow),
        (16, Opcode::Neg),
        (17, Opcode::BitAnd),
        (18, Opcode::BitOr),
        (19, Opcode::BitXor),
        (20, Opcode::Shl),
        (21, Opcode::Shr),
        (22, Opcode::UShr),
        (23, Opcode::BitNot),
        (24, Opcode::Eq),
        (25, Opcode::Ne),
        (26, Opcode::Lt),
        (27, Opcode::Le),
        (28, Opcode::Gt),
        (29, Opcode::Ge),
        (30, Opcode::StrictEq),
        (31, Opcode::StrictNe),
        (32, Opcode::Not),
        (33, Opcode::TypeOf),
        (34, Opcode::Jump),
        (35, Opcode::JumpIfFalse),
        (36, Opcode::JumpIfTrue),
        (37, Opcode::LoopHeader),
        (38, Opcode::Call),
        (39, Opcode::Return),
        (40, Opcode::Throw),
        (41, Opcode::GetProperty),
        (42, Opcode::SetProperty),
        (43, Opcode::DeleteProperty),
        (44, Opcode::NewObject),
        (45, Opcode::NewArray),
        (46, Opcode::Closure),
        (47, Opcode::NewEnvironment),
        (48, Opcode::GetEnvSlot),
        (49, Opcode::SetEnvSlot),
        (50, Opcode::CreateGenerator),
        (51, Opcode::SuspendYield),
        (52, Opcode::Await),
        (53, Opcode::In),
        (54, Opcode::InstanceOf),
        (55, Opcode::CopyArrayRest),
        (56, Opcode::CheckIsArray),
        (57, Opcode::CallApply),
        (58, Opcode::CopyObjectRest),
        (59, Opcode::ArrayAppend),
        (60, Opcode::GetGlobal),
        (61, Opcode::SetGlobal),
        (62, Opcode::Construct),
        (63, Opcode::GetNewTarget),
        (64, Opcode::ToNumber),
        (65, Opcode::MergeObject),
        (66, Opcode::DefineAccessor),
        (67, Opcode::JumpIfNullish),
        (68, Opcode::SetPrototype),
        (69, Opcode::GetIterator),
        (70, Opcode::IteratorNext),
        (71, Opcode::IteratorClose),
        (72, Opcode::DefineMethod),
        (73, Opcode::GetGlobalLenient),
        (74, Opcode::GenResumeMode),
        (75, Opcode::ToPropertyKey),
        (76, Opcode::GetAsyncIterator),
        (77, Opcode::GetPrototype),
        (78, Opcode::PopEnvironment),
    ];

    /// Every one of the 73 discriminants is pinned, not 13 of them.
    ///
    /// Three things at once, and each catches a different failure the old
    /// 13-assert test could not:
    ///
    /// * `op as u8 == byte` — a frozen row that no longer matches its own
    ///   variant means the *enum* was renumbered;
    /// * frozen list == [`ALL_OPCODES`] — the live list gained, lost or
    ///   reordered a variant (or moved one to a different byte);
    /// * strict ascent + equal length — a duplicate discriminant in
    ///   `ALL_OPCODES`, which `invert_all_opcodes` would silently collapse,
    ///   making one of the two variants undecodable.
    #[test]
    fn discriminants_are_frozen() {
        let mut previous: Option<u8> = None;
        for &(byte, op) in FROZEN_DISCRIMINANTS {
            assert_eq!(
                op as u8, byte,
                "frozen row ({byte}, {op:?}) contradicts the enum"
            );
            if let Some(prev) = previous {
                assert!(
                    prev < byte,
                    "frozen discriminants not ascending: {prev} then {byte}"
                );
            }
            previous = Some(byte);
        }
        assert_eq!(FROZEN_DISCRIMINANTS.len(), OPCODE_COUNT);

        let live: Vec<(u8, Opcode)> = ALL_OPCODES.iter().map(|&op| (op as u8, op)).collect();
        assert_eq!(
            live, FROZEN_DISCRIMINANTS,
            "the assigned-discriminant list moved; if that is intended, this is a \
             wire-format change and the frozen copy must be updated deliberately"
        );
    }

    /// Byte-for-byte decode equivalence with the pre-W2.2 hand-written decoder.
    ///
    /// The decoder used to be a 73-arm `match`; it is now a compile-time
    /// inversion of [`ALL_OPCODES`]. This walks the *entire* `0..=255` byte
    /// range and asserts the resulting `(byte, opcode)` sequence is exactly
    /// the frozen transcript of the old `match` — every assigned byte and,
    /// just as importantly, every byte still rejected.
    #[test]
    fn every_byte_decodes_exactly_as_the_frozen_decoder_did() {
        let decoded: Vec<(u8, Opcode)> = (0u16..=255)
            .filter_map(|byte| Opcode::try_from(byte as u8).ok().map(|op| (byte as u8, op)))
            .collect();
        assert_eq!(decoded, FROZEN_DISCRIMINANTS);

        // The complement matters as much as the mapping: 183 of 256 bytes must
        // still be rejected, so a decoder that accepted everything, or an
        // `Opcode::from_byte` that returned a default for a gap, fails here.
        let rejected = (0u16..=255)
            .filter(|byte| Opcode::try_from(*byte as u8).is_err())
            .count();
        assert_eq!(rejected, 256 - OPCODE_COUNT);

        // `from_byte` is what `TryFrom` delegates to; pin that they agree,
        // since only one of them is `const` and a future edit could split them.
        for byte in 0u16..=255 {
            let byte = byte as u8;
            assert_eq!(
                Opcode::from_byte(byte).ok_or(byte),
                Opcode::try_from(byte),
                "{byte}"
            );
        }
    }

    #[test]
    fn all_opcodes_roundtrip_through_new() {
        for (idx, &op) in ALL_OPCODES.iter().enumerate() {
            let i = Instr::new(op, idx as u8, 0xAB, 0xCD);
            assert_eq!(i.op(), Some(op), "{op:?}");
            assert_eq!(i.a(), idx as u8);
            assert_eq!(i.b(), 0xAB);
            assert_eq!(i.c(), 0xCD);
        }
    }

    #[test]
    fn all_opcodes_roundtrip_through_imm16_big_endian() {
        for &op in ALL_OPCODES {
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
        for &op in ALL_OPCODES {
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
