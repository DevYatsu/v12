//! The operand contract — one [`OpSpec`] per [`Opcode`].
//!
//! # Why this exists
//!
//! The `a`/`b`/`c` slots of an [`Instr`] are three bare bytes. Nothing in the
//! encoder says what any of them *means*, so an opcode can be emitted and
//! decoded with the operands in the wrong order and no test fails: an
//! exhaustive `match` proves every variant has a dispatch arm, not that the arm
//! reads the slots the emitter wrote. `FunctionBytecode::validate` checks
//! handler nesting and operand bounds, not operand *roles*. This module is the
//! missing statement.
//!
//! It lives in `v12-bytecode` because that crate owns *both* ends of the
//! contract — the [`Opcode`] enum and the [`Instr`] encoding — while all four
//! dispatch implementations (`v12-interp/src/execute.rs`,
//! `test-support/src/mini.rs`, `v12-jit-baseline/src/compiler.rs`,
//! `v12-bccompiler/src/peephole.rs`) live in other crates and share no
//! definitions with each other.
//!
//! # How the roles below were derived
//!
//! Every entry was read off two independent sites, never guessed:
//!
//! * the **construction site** in `v12-bccompiler` — `model.rs`
//!   (`emit_reg1/2/3`, `emit_reg2_imm8`, `emit_regs`, and the
//!   `emit_get_env` / `emit_set_env` / `emit_new_env` / `emit_new_array` /
//!   `emit_call` / `emit_closure` families), plus the direct
//!   `emit_reg3` call sites in `expr.rs`, `stmt.rs`, `class.rs` and
//!   `unit.rs`;
//! * the **read site** in `v12-interp/src/execute.rs`, one dispatch arm per
//!   opcode (`self.stack[base + ra]` / `rb` / `rc`, `narrow.imm16()`,
//!   `narrow.imm24()`, or `WideOp::try_decode`).
//!
//! Where the two disagree, the comment says so. The per-slot evidence is
//! cited inline as `emit: <file:line>` / `read: <file:line>`.
//!
//! # The contract is *descriptive*, not prescriptive
//!
//! Each entry records what the operand **actually is today**. Several
//! neighbouring opcodes disagree about operand order — see
//! [`KNOWN_INCONSISTENT_OPERAND_ORDER`] — and those are recorded as-is with
//! the divergence named, because normalising them changes the emitted
//! bytecode, and therefore the wire format and every dispatcher at once. That
//! is a separate decision, not a cleanup.
//!
//! Nothing here changes the emitted bytecode. Task W2.1 phase A is the table
//! plus the cross-check test in `tests.rs`; W2.2 (phase B) wires `Width` into
//! `v12-bccompiler`'s `word_width` and derives the byte decoder from the
//! assigned-discriminant list.
//!
//! # W2.3: the table is enforced, and what that does *not* mean
//!
//! [`crate::FunctionBytecode::validate`] now walks every instruction against
//! this table (see the `validate` module), so the roles are load-bearing rather
//! than descriptive: an operand the table calls a register must be inside the
//! frame, an environment slot must fit the environment the function allocated,
//! a branch must land in the stream, and so on. Six of the eight
//! [`KNOWN_INCONSISTENT_OPERAND_ORDER`] entries are *checked in their current
//! order* rather than exempted, because the checks read roles per slot: a gate
//! keyed on the opcode list would have had to exempt the very opcodes whose
//! operands are most worth checking.
//!
//! What the enforcement does **not** reach is the role *kind* — nothing here
//! can tell a `Key` from an `Obj` in a slot both call a register, for the
//! reason the phase-A test `role_kinds_are_citation_backed_for_unlabelled_register_operands`
//! already states. So normalising the divergences (W2.4) is *not* made safe by
//! this gate alone: a forgotten transposition still validates. W2.4 keeps the
//! label cross-check in `tests.rs` as its real gate, and the emit-site
//! citations as its checklist.

use crate::{Instr, Opcode, WideOp};
#[cfg(test)]
use std::fmt;

/// What one [`Instr`] operand slot holds.
///
/// The vocabulary is deliberately narrow: a role names the *kind* of operand,
/// and the comment at the assignment site carries the detail. Roles that span
/// two slots ([`Role::Imm16`], [`Role::Imm24`]) appear in *both* slots they
/// occupy, so a consumer inspecting one slot still sees the truth; reassemble
/// with [`Instr::imm16`] / [`Instr::imm24`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The emitter writes this slot but nothing reads it, or it is written
    /// with a constant zero. Naming these is the point: dead operands are
    /// otherwise invisible.
    Unused,
    /// A register the instruction writes (its destination).
    Dst,
    /// A register the instruction reads (a value operand).
    Src,
    /// A register the instruction reads *and* may overwrite — the operand is
    /// both an input and the destination. Only [`Opcode::SuspendYield`] slot
    /// `a` and [`Opcode::GenResumeMode`] slot `b`.
    InOut,
    /// A register a branch tests for truthiness (`JumpIfFalse` /
    /// `JumpIfTrue` / `JumpIfNullish` slot `a`). Read only, but not a value
    /// operand — it is never passed anywhere.
    Cond,
    /// The base object of a property access or membership test.
    Obj,
    /// A property-key register (normalised by
    /// [`Opcode::ToPropertyKey`] only where that opcode is emitted).
    Key,
    /// The value being stored.
    Val,
    /// Base register of a call header window `[callee][this][args…]` for
    /// [`Opcode::Call`] / [`Opcode::Construct`] / [`Opcode::CallApply`] slot
    /// `b`. `this` sits at `callee + 1` and the first argument at
    /// `callee + CALL_HEADER_REGS`; those are positional, not stored operands.
    Callee,
    /// An immediate count: a call's argument count (`Call` / `Construct` slot
    /// `c`) or an array literal's element count (`NewArray` slot `c`).
    Count,
    /// A register holding an *array* of arguments (`CallApply` slot `c`).
    ArgsArr,
    /// Base register of a contiguous element window (`NewArray` slot `b`).
    ElemBase,
    /// A register holding one excluded key, or `0` for "none"
    /// (`CopyObjectRest` slot `c`).
    Excl,
    /// Base register of a packed `(getter, setter)` pair
    /// (`DefineAccessor` slot `c`).
    Pair,
    /// Environment chain depth: how many hops up the frame's environment
    /// chain. An immediate, not a register.
    Depth,
    /// Index into the addressed environment's slot vector. An immediate, not
    /// a register.
    Slot,
    /// Index into `Program::functions` (`Closure` / `CreateGenerator` slot
    /// `b`). An immediate, not a register.
    FnIndex,
    /// First element index of an array-rest slice (`CopyArrayRest` slot `c`).
    /// An immediate, not a register.
    Start,
    /// A `0`/`1` behavioural switch: `IteratorClose`'s completion flag,
    /// `Await`'s async-from-sync-iterator-result flag.
    Flag,
    /// 16-bit index into the function's `ConstantPool`, big-endian across
    /// `b` (high) and `c` (low) — [`Opcode::LoadConst`].
    ConstId,
    /// 16-bit index into the program's string table naming a global, likewise
    /// big-endian across `b`/`c` — [`Opcode::GetGlobal`],
    /// [`Opcode::GetGlobalLenient`], [`Opcode::SetGlobal`].
    NameId,
    /// Sign-extended 8-bit immediate in `c` ([`Opcode::LoadInt`]).
    Imm8,
    /// Half of a big-endian 16-bit immediate: `b` high, `c` low
    /// ([`Opcode::JumpIfFalse`] and friends). Reassemble with
    /// [`Instr::imm16`].
    Imm16,
    /// One third of a big-endian 24-bit immediate spanning `a`, `b` and `c`
    /// ([`Opcode::Jump`]). Reassemble with [`Instr::imm24`].
    Imm24,
    /// [`WideOp::RegExt`]'s slot mask: which of the *following* narrow
    /// instruction's `a`/`b`/`c` bytes are register high halves. `0` for every
    /// other [`Opcode::Wide`] variant.
    Mask,
    /// [`WideOp`] variant discriminant, in the header's low immediate byte.
    Disc,
}

impl Role {
    /// The role's name, for diagnostics.
    ///
    /// A validation failure has to say *which* operand was wrong, and the
    /// alternatives — the slot letter, or the disassembler's own rendering —
    /// are both less precise: `b` does not distinguish a key from an object,
    /// and `fmt_operands` is the third statement of the contract rather than
    /// the one being enforced.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unused => "unused",
            Self::Dst => "dst",
            Self::Src => "src",
            Self::InOut => "in/out",
            Self::Cond => "condition",
            Self::Obj => "object",
            Self::Key => "key",
            Self::Val => "value",
            Self::Callee => "callee",
            Self::Count => "count",
            Self::ArgsArr => "argument array",
            Self::ElemBase => "element base",
            Self::Excl => "excluded key",
            Self::Pair => "accessor pair base",
            Self::Depth => "environment depth",
            Self::Slot => "environment slot",
            Self::FnIndex => "function index",
            Self::Start => "start index",
            Self::Flag => "flag",
            Self::ConstId => "constant id",
            Self::NameId => "name id",
            Self::Imm8 => "immediate",
            Self::Imm16 => "jump target (high)",
            Self::Imm24 => "jump target",
            Self::Mask => "register-extension mask",
            Self::Disc => "wide discriminant",
        }
    }
}

/// How many [`Instr`] words one instruction occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    /// Exactly one word.
    One,
    /// A [`Opcode::Wide`] header plus a variant-specific number of raw payload
    /// words. Only decoding the header reveals the count, so a layout walk
    /// must decode rather than assume.
    WidePayload,
}

/// Which of an [`Instr`]'s three operand slots a role occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
    C,
}

impl Slot {
    /// All three slots, in encoding order. Useful for exhaustive walks.
    pub const ALL: [Self; 3] = [Self::A, Self::B, Self::C];

    /// This slot's index into `a`/`b`/`c` within the 32-bit word.
    const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
        }
    }
}

/// The operand contract for one [`Opcode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpSpec {
    pub a: Role,
    pub b: Role,
    pub c: Role,
    pub width: Width,
}

impl OpSpec {
    /// A single-word opcode with roles given in full.
    pub const fn new(a: Role, b: Role, c: Role) -> Self {
        Self {
            a,
            b,
            c,
            width: Width::One,
        }
    }

    /// A [`Opcode::Wide`]-header opcode: its own word count is only knowable by
    /// decoding the header.
    pub const fn wide(a: Role, b: Role, c: Role) -> Self {
        Self {
            a,
            b,
            c,
            width: Width::WidePayload,
        }
    }

    /// The role occupying `slot`.
    pub const fn role_at(self, slot: Slot) -> Role {
        match slot {
            Slot::A => self.a,
            Slot::B => self.b,
            Slot::C => self.c,
        }
    }

    /// The slots this opcode actually uses — i.e. every slot whose role is not
    /// [`Role::Unused`].
    ///
    /// This is what the cross-check in `tests.rs` compares against the slots
    /// the disassembler prints: an operand the table marks unused but the
    /// disassembler shows (or vice versa) means one of the two is lying.
    pub const fn used_slots(self) -> [bool; 3] {
        [
            !matches!(self.a, Role::Unused),
            !matches!(self.b, Role::Unused),
            !matches!(self.c, Role::Unused),
        ]
    }

    /// Whether `slot` holds a *register* (and so may be widened past 8 bits by
    /// a [`WideOp::RegExt`] prefix) rather than an immediate that must fit a
    /// byte.
    ///
    /// This is the distinction `v12-bccompiler`'s `emit_regs` encodes as its
    /// `reg_mask`, and it is the property a layout or peephole pass needs
    /// before it can reason about a narrow instruction's operands.
    ///
    /// Derived from [`Role`], and therefore *only as trustworthy as the table*.
    /// No test in this crate can check it: the emitter's `reg_mask` lives in
    /// `v12-bccompiler` and the mask a real prefix carries is only knowable by
    /// decoding one. The cross-check in `tests.rs` does bound the adjacent
    /// question — the *set* of live slots — against the disassembler.
    pub const fn slot_is_register(self, slot: Slot) -> bool {
        matches!(
            self.role_at(slot),
            Role::Dst
                | Role::Src
                | Role::InOut
                | Role::Cond
                | Role::Obj
                | Role::Key
                | Role::Val
                | Role::Callee
                | Role::ElemBase
                | Role::ArgsArr
                | Role::Excl
                | Role::Pair
        )
    }

    /// Whether `slot` may be widened past 8 bits at all, as a `const fn` over
    /// the byte index `0`/`1`/`2` rather than a [`Slot`].
    pub const fn index_is_register(self, index: usize) -> bool {
        self.slot_is_register(match index {
            0 => Slot::A,
            1 => Slot::B,
            _ => Slot::C,
        })
    }

    /// Words this instruction spans in `words`, given the slice starting at its
    /// own pc.
    ///
    /// The layout walk's primitive: a peephole pass that needs `pc`-to-`pc`
    /// adjacency cannot assume one word per instruction.
    pub fn logical_width(self, words: &[Instr]) -> usize {
        match self.width {
            Width::One => 1,
            Width::WidePayload => WideOp::try_decode(words).map_or(1, |(_, width)| width),
        }
    }

    /// Role of the operand at byte `slot_index` within a word laid out
    /// `a`, `b`, `c` — the slice-shaped accessor for consumers that walk raw
    /// words rather than [`Instr`] accessors.
    pub const fn role_at_index(self, slot_index: usize) -> Role {
        self.role_at(match slot_index {
            0 => Slot::A,
            1 => Slot::B,
            _ => Slot::C,
        })
    }

    /// Index of `slot` within the word, exposed for callers that pack.
    pub const fn index_of(slot: Slot) -> usize {
        slot.index()
    }
}

/// The contract table, indexed by `Opcode as usize`.
///
/// `None` at an unassigned discriminant; `Some` at every assigned one. Built
/// entirely at compile time, so the table *is* the single source — there is no
/// second decoder that can fall out of step with it.
static OP_SPECS: [Option<OpSpec>; 256] = build_specs();

/// Every assigned [`Opcode`], in discriminant order.
///
/// A `Vec` view of [`crate::ALL_OPCODES`] — the same list the byte decoder inverts, so
/// a variant cannot be decoded but absent here, or present here but not
/// decodable. The list it replaced (`ALL_OPS` in `opcode.rs`) was hand-written
/// and went stale: it stopped at `ToPropertyKey` = 75 and never gained
/// `GetAsyncIterator` = 76, `GetPrototype` = 77 or `PopEnvironment` = 78, and
/// the tests in `opcode.rs` iterated it, so those three variants had no
/// encoding coverage at all.
///
/// Returns `Vec` rather than the slice itself because callers clone per sweep
/// and the crate exposes the const list directly for the cases that do not.
pub fn all_opcodes() -> Vec<Opcode> {
    crate::ALL_OPCODES.to_vec()
}

/// The operand contract for this opcode.
///
/// `None` cannot happen for an `Opcode` value — the table has an entry for
/// every assigned discriminant — but the signature keeps the shape honest
/// rather than relying on an unchecked index.
pub fn spec(op: Opcode) -> Option<OpSpec> {
    OP_SPECS[op as usize]
}

/// The table entry for a raw opcode byte, whether or not it is assigned.
///
/// The accessor a layout walk needs: `peephole`-style passes hold `Instr`
/// words, not `Opcode` values, and must be able to ask "is this byte an opcode
/// at all, and what are its operand roles" without decoding twice.
pub fn spec_at_byte(byte: u8) -> Option<OpSpec> {
    OP_SPECS[usize::from(byte)]
}

/// Renders one instruction's operands exactly as the disassembler does.
///
/// Test-only. The cross-check needs the disassembler's *own* output rather
/// than a second copy of its formatting rules — a re-implementation would
/// agree with a wrong table by construction.
#[cfg(test)]
pub(crate) fn render_operands(op: Opcode, i: Instr) -> String {
    /// Carries one instruction through `fmt_operands` unchanged, so
    /// `to_string` builds a real `Formatter` over a `String` buffer — the
    /// same path `FunctionBytecode`'s `Display` takes.
    struct Operands {
        op: Opcode,
        i: Instr,
    }

    impl fmt::Display for Operands {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            crate::fmt_operands(f, self.op, self.i)
        }
    }

    Operands { op, i }.to_string()
}

const fn build_specs() -> [Option<OpSpec>; 256] {
    // `None` is both "unassigned discriminant" and the starting value; every
    // assigned entry below overwrites one, and
    // `every_assigned_opcode_has_a_spec` fails if a new variant is not listed.
    let mut table = [None; 256];

    // --- data movement and constants ------------------------------------
    // emit: `move_reg` -> `emit_reg2(Move, dst, src)` (model.rs:1566).
    // read: `stack[ra] = stack[rb]` (execute.rs:227).
    table[Opcode::Move as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    // emit: `emit_regs(LoadConst, dst, k >> 8, k & 0xFF, mask 0b0001)`
    // (model.rs:1503) — `b`/`c` are immediate halves, never RegExt-extended.
    // read: `const_value(fn_idx, narrow.imm16(), …)` (execute.rs:237).
    table[Opcode::LoadConst as usize] = Some(OpSpec::new(Role::Dst, Role::ConstId, Role::ConstId));
    // emit: `Instr::new(LoadInt, dst, 0, v as i8 as u8)` (model.rs:1534).
    // read: `i8::from_be_bytes([narrow.c()])` (execute.rs:231).
    table[Opcode::LoadInt as usize] = Some(OpSpec::new(Role::Dst, Role::Unused, Role::Imm8));
    // emit: `WideOp::encode`'s `hdr(a, disc)` = `Instr::new(Wide, a,
    // disc >> 8, disc)` (wide.rs:193). `a` is the variant-specific byte —
    // `RegExt`'s slot mask, `0` for all 16 other variants — and `c` is the
    // discriminant. `b` is written as `disc >> 8`, which is `0` for every
    // discriminant defined today (max `DISC_GET_SUPER_W` = 16) and is never
    // read: `try_decode` matches on `header.c()` alone (wide.rs:352).
    table[Opcode::Wide as usize] = Some(OpSpec::wide(Role::Mask, Role::Unused, Role::Disc));

    // --- arithmetic, comparison ------------------------------------------
    // Every one of the 20 below is emitted `emit_reg3(op, dst, lhs, rhs)`
    // with mask `0b0111` — expr.rs:1112 for the binary operators, and
    // `load_bool`'s `emit_reg3(Eq|Ne, dst, t, t)` (model.rs:1563) for the
    // boolean-materialising path — and read as
    // `stack[rb]` / `stack[rc]` into `stack[ra]` (execute.rs:427-535).
    //
    // TERNARY: three operands, not two. `Add` disassembles as
    // "r17, r22, r33" and a table entry that collapses it to a binary shape
    // would silently drop a slot.
    table[Opcode::Add as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Sub as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Mul as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Div as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Mod as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Pow as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::BitAnd as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::BitOr as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::BitXor as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Shl as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Shr as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::UShr as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Eq as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Ne as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Lt as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Le as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Gt as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::Ge as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::StrictEq as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));
    table[Opcode::StrictNe as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));

    // --- unary ------------------------------------------------------------
    // emit: `emit_reg2(op, dst, v)` (expr.rs:882 for `Neg`/`Not`/`BitNot`;
    // expr.rs:874 for `ToNumber`; expr.rs:1017 for `TypeOf`; stmt.rs:436 for
    // `ToPropertyKey`). read: `stack[rb]` into `stack[ra]`
    // (execute.rs:536-573).
    table[Opcode::Neg as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    table[Opcode::BitNot as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    table[Opcode::Not as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    table[Opcode::TypeOf as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    table[Opcode::ToNumber as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    table[Opcode::ToPropertyKey as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));

    // --- control flow ------------------------------------------------------
    // emit: `Instr::new_imm24(Jump, …)` via `emit_jump` (model.rs:1468).
    // read: `narrow.imm24()` (execute.rs:595) — all three slots are the
    // target.
    table[Opcode::Jump as usize] = Some(OpSpec::new(Role::Imm24, Role::Imm24, Role::Imm24));
    // emit: `emit_jump(JumpIfFalse | JumpIfTrue, cond, label)` -> `a` = cond
    // register, `b`/`c` = `imm16` target (model.rs:1468).
    // read: `ops::to_boolean(stack[ra])` then `set_pc(narrow.imm16())`
    // (execute.rs:597-605).
    table[Opcode::JumpIfFalse as usize] = Some(OpSpec::new(Role::Cond, Role::Imm16, Role::Imm16));
    table[Opcode::JumpIfTrue as usize] = Some(OpSpec::new(Role::Cond, Role::Imm16, Role::Imm16));
    // emit: `emit_jump(JumpIfNullish, flag, label)` (model.rs:1079).
    // read: `stack[ra].is_null() || is_undefined()`, target `narrow.imm16()`
    // (execute.rs:606).
    table[Opcode::JumpIfNullish as usize] = Some(OpSpec::new(Role::Cond, Role::Imm16, Role::Imm16));
    // emit: `Instr::new_imm24(LoopHeader, 0)` — a zero payload. read: the arm
    // touches no operand, only `note_loop` (execute.rs:617).
    table[Opcode::LoopHeader as usize] =
        Some(OpSpec::new(Role::Unused, Role::Unused, Role::Unused));

    // --- calls, returns, throws --------------------------------------------
    // emit: `emit_call(dst, callee, argc)` (model.rs:1741) — `a` = destination,
    // `b` = callee register, `c` = immediate argc (unmasked, mask `0b0011`).
    // read: `prepare_call(…, rb, rc)`; result into `stack[ra]`
    // (execute.rs:625-649). `a` is *also* the window base for
    // `decode_parked_call` on the resume path (execute.rs:103-104).
    table[Opcode::Call as usize] = Some(OpSpec::new(Role::Dst, Role::Callee, Role::Count));
    // emit: `emit_construct(dst, callee, argc)` (model.rs:1765) — identical
    // layout to `Call`. read: `prepare_construct(…, rb, rc)`
    // (execute.rs:650).
    table[Opcode::Construct as usize] = Some(OpSpec::new(Role::Dst, Role::Callee, Role::Count));
    // emit: `emit_reg3(Return, v, 0, 0)` / `emit_reg1(Return, v)` (expr.rs:897,
    // unit.rs:310). read: `complete_frame(stack[ra])` (execute.rs:666).
    table[Opcode::Return as usize] = Some(OpSpec::new(Role::Src, Role::Unused, Role::Unused));
    // emit: `emit_reg3(Throw, err, 0, 0)` (model.rs:1040).
    // read: `throw_js!(stack[ra])` (execute.rs:673).
    table[Opcode::Throw as usize] = Some(OpSpec::new(Role::Src, Role::Unused, Role::Unused));

    // --- property access ------------------------------------------------------
    // emit: `emit_reg3(GetProperty, dst, obj, key)` (expr.rs:1539 and ~40
    // others). read: `get_property(fn_idx, pc, stack[rb], stack[rc])` into
    // `stack[ra]` (execute.rs:680).
    table[Opcode::GetProperty as usize] = Some(OpSpec::new(Role::Dst, Role::Obj, Role::Key));
    // DIVERGENCE: no destination. emit: `emit_reg3(SetProperty, obj, key, val)`
    // (expr.rs:2314). read: `set_property(stack[ra], stack[rb], stack[rc], …)`
    // (execute.rs:693) — the base object moved from `b` to `a` and the value
    // took the slot `GetProperty` spends on the key.
    table[Opcode::SetProperty as usize] = Some(OpSpec::new(Role::Obj, Role::Key, Role::Val));
    // emit: `emit_reg3(DeleteProperty, dst, obj, key)` (expr.rs:1057).
    // read: `delete_property(stack[rb], stack[rc])`, result into `stack[ra]`
    // (execute.rs:723). Same shape as `GetProperty`, unlike `SetProperty`.
    table[Opcode::DeleteProperty as usize] = Some(OpSpec::new(Role::Dst, Role::Obj, Role::Key));
    // emit: `emit_reg3(DefineMethod, obj, key, fn)` (class.rs:85/87/187).
    // read: `op_define_method(stack[ra], stack[rb], stack[rc])` (execute.rs:715).
    // Agrees with `SetProperty`, not with `GetProperty`.
    table[Opcode::DefineMethod as usize] = Some(OpSpec::new(Role::Obj, Role::Key, Role::Val));
    // emit: `emit_reg3(DefineAccessor, obj, key, pair)` (class.rs:173,
    // expr.rs:771). read: `getter = stack[rc]`, `setter = stack[rc + 1]`
    // (execute.rs:985-999) — `c` names a packed pair base, not one value.
    table[Opcode::DefineAccessor as usize] = Some(OpSpec::new(Role::Obj, Role::Key, Role::Pair));
    // emit: `emit_reg3(SetPrototype, 0, obj, proto)` (class.rs:99/104).
    // read: `op_set_prototype(stack[rb], stack[rc])` (execute.rs:840) — `a` is
    // written as a literal `0`, never read.
    table[Opcode::SetPrototype as usize] = Some(OpSpec::new(Role::Unused, Role::Obj, Role::Src));
    // emit: `emit_reg3(GetPrototype, dst, this, 0)` (expr.rs:2414).
    // read: `op_get_prototype(stack[rb])` into `stack[ra]` (execute.rs:846).
    table[Opcode::GetPrototype as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));

    // --- allocation and environments -------------------------------------------
    // emit: `emit_reg3(NewObject, dst, 0, 0)` (expr.rs:704, class.rs:81).
    // read: `stack[ra]` (execute.rs:740).
    table[Opcode::NewObject as usize] = Some(OpSpec::new(Role::Dst, Role::Unused, Role::Unused));
    // emit: `emit_new_array(dst, base, len)` (model.rs:1789) — mask `0b0011`,
    // so `c` (an immediate length) is never RegExt-extended.
    // read: `stack[rb .. rb + rc]` into `stack[ra]` (execute.rs:750).
    table[Opcode::NewArray as usize] = Some(OpSpec::new(Role::Dst, Role::ElemBase, Role::Count));
    // emit: `emit_closure(dst, function_index)` -> `Instr::new(Closure, dst,
    // function_index, 0)` (model.rs:1873). read: `alloc_closure(rb.into(), …)`
    // (execute.rs:769) — `b` is a `Program::functions` index immediate, and
    // the wide escape `ClosureW` exists precisely because 8 bits is too few.
    table[Opcode::Closure as usize] = Some(OpSpec::new(Role::Dst, Role::FnIndex, Role::Unused));
    // emit: `Instr::new(NewEnvironment, depth, slots, 0)` (model.rs:1798).
    // read: `let slots = usize::from(rb)` (execute.rs:774) and
    // `WideOp::NewEnvironmentW { depth: _, slots }` (execute.rs:296) — BOTH
    // forms discard `depth`. The static depth still drives the compiler's own
    // env model, but at runtime the parent chain is the truth, so the operand
    // is recorded as unused rather than as a `Depth`.
    table[Opcode::NewEnvironment as usize] =
        Some(OpSpec::new(Role::Unused, Role::Slot, Role::Unused));
    // emit: `Instr::new(PopEnvironment, 0, 0, 0)` (model.rs:1823/1835).
    // read: no operand (execute.rs:803) — the retired head comes from
    // `frame.env`.
    table[Opcode::PopEnvironment as usize] =
        Some(OpSpec::new(Role::Unused, Role::Unused, Role::Unused));
    // DIVERGENCE: emit: `emit_get_env(dst, depth, slot)` (model.rs:1579) ->
    // `Instr::new(GetEnvSlot, dst, depth, slot)`. read: `env_read(rb, rc)`
    // into `stack[ra]` (execute.rs:830). The destination is FIRST.
    table[Opcode::GetEnvSlot as usize] = Some(OpSpec::new(Role::Dst, Role::Depth, Role::Slot));
    // DIVERGENCE: emit: `emit_set_env(depth, slot, src)` (model.rs:1592) ->
    // `Instr::new(SetEnvSlot, depth, slot, src)`. read:
    // `env_write(ra, rb, stack[rc])` (execute.rs:835). The destination moved
    // from `a` to `c` and the immediates slid down — the exact transpose of
    // the getter above.
    table[Opcode::SetEnvSlot as usize] = Some(OpSpec::new(Role::Depth, Role::Slot, Role::Src));

    // --- generators and async ---------------------------------------------------
    // emit: `emit_reg3(CreateGenerator, dst, func_idx, 0)` (unit.rs:231).
    // read: the live path takes `frames.last().generator` and never touches
    // `rb`; the dormant fallback reads `stack[base + rb]` as a *register*
    // (execute.rs:1079-1084). Emitter and interpreter disagree here — see
    // `CREATE_GENERATOR_B_FALLBACK` below — so `b` is recorded as the
    // `FnIndex` immediate the compiler actually writes.
    table[Opcode::CreateGenerator as usize] =
        Some(OpSpec::new(Role::Dst, Role::FnIndex, Role::Unused));
    // emit: `emit_reg3(SuspendYield, ydst, 0, 0)` (expr.rs:366/891).
    // read: `let yielded = stack[base + dst]` then `suspend(dst, yielded, …)`
    // (execute.rs:1120-1146) — the same register carries the value out and the
    // resume value back in.
    table[Opcode::SuspendYield as usize] =
        Some(OpSpec::new(Role::InOut, Role::Unused, Role::Unused));
    // emit: `emit_reg3(Await, dst, arg, flag)` — flag `0` everywhere except
    // the async-from-sync `yield*` path, which passes `1` (expr.rs:358; `0` at
    // expr.rs:369/383/407, stmt.rs:970/990).
    // read: `stack[rb]` awaited, result into `stack[ra]`, `rc == 1` selects
    // the extra AsyncFromSync value await (execute.rs:1149-1199).
    table[Opcode::Await as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Flag));
    // emit: `emit_reg3(GenResumeMode, mode, ydst, 0)` (expr.rs:893) and
    // `emit_reg2(GenResumeMode, mode, 0)` when there is no yield register
    // (expr.rs:914). read: mode into `stack[ra]`; `rb` is the yield register,
    // read only to test it for zero and then OVERWRITTEN with the saved
    // `gen.return(v)` value (execute.rs:874-905).
    table[Opcode::GenResumeMode as usize] = Some(OpSpec::new(Role::Dst, Role::InOut, Role::Unused));

    // --- membership --------------------------------------------------------------
    // DIVERGENCE: emit: `emit_reg3(In, dst, lhs, rhs)` (expr.rs:1106) where
    // `lhs` is the KEY and `rhs` the OBJECT. read: `op_in(stack[rb],
    // stack[rc])` (execute.rs:574) — `key_v` from `b`, `obj_v` from `c`.
    // Every other binary opcode above is `(dst, lhs, rhs)`, so the object sits
    // in the slot the right operand would occupy, reversed.
    table[Opcode::In as usize] = Some(OpSpec::new(Role::Dst, Role::Key, Role::Obj));
    // emit: `emit_reg3(InstanceOf, dst, lhs, rhs)` (expr.rs:1107).
    // read: `op_instanceof(stack[rb], stack[rc])` (execute.rs:582) — the
    // conventional `(dst, lhs, rhs)`.
    table[Opcode::InstanceOf as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Src));

    // --- destructuring, spread, iteration ------------------------------------------
    // emit: `emit_reg2_imm8(CopyArrayRest, dst, src, start)` (stmt.rs:523,
    // stmt.rs:1164) — but ALSO `emit_reg3(CopyArrayRest, dst, src, index)`
    // (expr.rs:1445), whose mask `0b0111` lets a `start` above 255 escape
    // through `RegExt`. Either way it is an immediate index, never a register.
    // read: `op_copy_array_rest(stack[rb], rc)` into `stack[ra]`
    // (execute.rs:927).
    table[Opcode::CopyArrayRest as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Start));
    // emit: `emit_reg3(CheckIsArray, src, 0, 0)` (expr.rs:666, expr.rs:2107).
    // read: `op_check_is_array(stack[ra])` (execute.rs:934) — it only throws,
    // it stores nothing, so the slot is a source.
    table[Opcode::CheckIsArray as usize] = Some(OpSpec::new(Role::Src, Role::Unused, Role::Unused));
    // DIVERGENCE from `Call`: emit: `emit_reg3(CallApply, dst, block, args_arr)`
    // (expr.rs:2212, expr.rs:1894) — same `(a, b)` prefix, but `c` is a
    // register holding an argument ARRAY, not an immediate argc.
    // read: `prepare_call_apply(…, callee_v, this_v, args_v)` with
    // `this_v = stack[rb + 1]` and `args_v = stack[rc]` (execute.rs:939-950).
    table[Opcode::CallApply as usize] = Some(OpSpec::new(Role::Dst, Role::Callee, Role::ArgsArr));
    // emit: NEVER. Every object-rest site emits the wide
    // `WideOp::CopyObjectRestW { dst, src, excl_base, excl_count }`
    // (stmt.rs:460/469/1271/1280). The narrow form survives only in the
    // interpreter's arm, which reads `stack[rb]` as the source and takes `c`
    // as a single excluded-key register (`0` = none) (execute.rs:960-971).
    table[Opcode::CopyObjectRest as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Excl));
    // emit: `emit_reg3(ArrayAppend, dst, src, 0)` (expr.rs:667, expr.rs:2108).
    // read: `op_array_append(stack[ra], stack[rb])` (execute.rs:973) — `a` is
    // the destination ARRAY being appended to.
    table[Opcode::ArrayAppend as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    // DIVERGENCE: emit: `emit_reg3(MergeObject, 0, dst, src)` (expr.rs:715) —
    // `a` is a literal `0`. read: `op_merge_object(stack[rb], stack[rc])`
    // (execute.rs:979). The only opcode whose destination is not in `a`.
    table[Opcode::MergeObject as usize] = Some(OpSpec::new(Role::Unused, Role::Dst, Role::Src));
    // emit: `emit_reg3(GetIterator, dst, src, 0)` (stmt.rs:549).
    // read: `op_get_iterator(stack[rb])` into `stack[ra]` (execute.rs:852).
    table[Opcode::GetIterator as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    // emit: `emit_reg3(GetAsyncIterator, dst, src, 0)` (stmt.rs:942, expr.rs:258).
    // read: `get_iterator_with_mode(stack[rb], true)` into `stack[ra]`
    // (execute.rs:860).
    table[Opcode::GetAsyncIterator as usize] =
        Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    // emit: `emit_reg3(IteratorNext, dst, iter, 0)` (stmt.rs:572/628/967).
    // read: `op_iterator_next(stack[rb])` into `stack[ra]` (execute.rs:907).
    table[Opcode::IteratorNext as usize] = Some(OpSpec::new(Role::Dst, Role::Src, Role::Unused));
    // emit: `emit_reg3(IteratorClose, iter, flag, 0)` — flag `1` on the
    // break/return path (stmt.rs:618, model.rs:1407) and `0` on the throw path
    // (stmt.rs:651, stmt.rs:1083). read: `iter_v = stack[ra]`,
    // `throw_path = rb == 0` (execute.rs:915-924). `c` is a literal `0`.
    table[Opcode::IteratorClose as usize] = Some(OpSpec::new(Role::Src, Role::Flag, Role::Unused));

    // --- globals and misc ------------------------------------------------------------
    // emit: `emit_get_global(dst, name_id)` -> `emit_regs(GetGlobal, dst,
    // k >> 8, k & 0xFF, mask 0b0001)` (model.rs:1620). read:
    // `strings_for_program(...)[narrow.imm16()]` into `stack[ra]`
    // (execute.rs:1002).
    table[Opcode::GetGlobal as usize] = Some(OpSpec::new(Role::Dst, Role::NameId, Role::NameId));
    // emit: `emit_get_global_lenient` (model.rs:1628) — identical layout.
    // read: execute.rs:1019.
    table[Opcode::GetGlobalLenient as usize] =
        Some(OpSpec::new(Role::Dst, Role::NameId, Role::NameId));
    // DIVERGENCE from `GetGlobal`: emit: `emit_set_global(name_id, src)` ->
    // `emit_regs(SetGlobal, src, k >> 8, k & 0xFF, mask 0b0001)`
    // (model.rs:1725). read: `val = stack[ra]`, name from `narrow.imm16()`
    // (execute.rs:1034). Same slot order, but slot `a` is a SOURCE here and a
    // DESTINATION there.
    table[Opcode::SetGlobal as usize] = Some(OpSpec::new(Role::Src, Role::NameId, Role::NameId));
    // emit: `emit_reg2(GetNewTarget, dst, 0)` (expr.rs:469).
    // read: `frames.last().new_target` into `stack[ra]` (execute.rs:661) — no
    // operand is read at all.
    table[Opcode::GetNewTarget as usize] = Some(OpSpec::new(Role::Dst, Role::Unused, Role::Unused));

    table
}

/// Operand-order divergences between opcodes, **recorded, not fixed**.
///
/// Each entry is documentation. Normalising any of them changes the emitted
/// bytecode — which changes the serialized format and forces every dispatcher
/// and every `Instr` in flight to change together — so it is a format decision
/// with its own migration, not a refactor. The point of listing them is that
/// the table has to say something about each, and "inconsistent on purpose
/// (today)" is the honest answer.
/// Registers reserved before a call's arguments: `[callee][this][args...]`.
///
/// The callee's register window starts at `callee_reg + 1`, so callee `r0` is
/// the caller's `this`, and callee `r{i}` is argument `i - 1`. Arguments
/// therefore occupy `callee_reg + CALL_HEADER_REGS ..
/// callee_reg + CALL_HEADER_REGS + argc` on the caller side, and no copying is
/// needed at either boundary.
///
/// **Why this lives here rather than in the compiler.** The emitter
/// (`v12-bccompiler`) sizes call windows from it, and the interpreter
/// (`v12-interp`) reads them back at six `args_start` sites. `v12-bccompiler` is
/// an OPTIONAL dependency of `v12-interp` — deliberately, so the front-end is
/// not a runtime dependency of the interpreter — so the interpreter could not
/// name the compiler's copy without reintroducing that edge. `v12-bytecode` is
/// the one crate both already depend on, and it already owns the calling
/// convention's operand table (`OpSpec`), so the layout constant belongs beside
/// it. `v12-bccompiler::model::CALL_HEADER_REGS` re-exports this for its own
/// callers.
pub const CALL_HEADER_REGS: u16 = 2;

pub const KNOWN_INCONSISTENT_OPERAND_ORDER: &[(&str, &str)] = &[
    (
        "GetEnvSlot / SetEnvSlot",
        "get is (dst, depth, slot); set is (depth, slot, src) — the destination \
         moves from `a` to `c` between a getter and its own setter",
    ),
    (
        "GetProperty, DeleteProperty / SetProperty, DefineMethod",
        "get is (dst, obj, key); set is (obj, key, val) — the base object moves \
         from `b` to `a`, the key from `c` to `b`, and there is no destination",
    ),
    (
        "binary operators / In",
        "the 20 arithmetic and comparison opcodes are (dst, lhs, rhs); `In` is \
         (dst, key, obj), so the object sits in `c` where the right operand would \
         go, i.e. the two operands are reversed",
    ),
    (
        "MergeObject",
        "(unused, dst, src) — the only opcode whose destination is not in `a`; \
         the emitter writes a literal 0 there",
    ),
    (
        "SetPrototype",
        "(unused, obj, proto) — same dead `a` as `MergeObject`, but the base \
         object stays in `b`",
    ),
    (
        "GetGlobal / SetGlobal",
        "get is (dst, name); set is (src, name) — identical slot order, but \
         slot `a` is a destination for one and a source for the other",
    ),
    (
        "Call / CallApply",
        "identical (dst, callee, …) prefix, but `c` is an immediate argc for \
         one and an argument-array register for the other",
    ),
    (
        "SetProperty, DefineMethod / DefineAccessor",
        "`c` is a single value register for the first two and a packed \
         (getter, setter) pair base for the third",
    ),
];

/// Where the **disassembler** and this table disagree about *which slots are
/// live*.
///
/// `fmt_operands` in `lib.rs` is a third, independent statement of the
/// contract, and it disagrees with reality in five places. These are not
/// operand-order divergences: the slot ORDER agrees, only the *set* of
/// rendered slots differs. Recorded rather than fixed because changing what
/// `--disasm` prints is a user-visible behaviour change, out of scope for a
/// table-only task.
///
/// Each entry is `(opcode, divergent-slot mask, why)`, where the mask has bit
/// `n` set for slot `n` (`0` = `a`, `1` = `b`, `2` = `c`). Pinning the mask
/// rather than the opcode matters: an opcode-level exemption would silently
/// tolerate a *second*, unrelated mistake in the same entry, because the
/// recomputation would still classify the opcode as documented.
pub const KNOWN_DISASSEMBLER_SLOT_DIVERGENCE: &[(Opcode, u8, &str)] = &[
    (
        Opcode::Wide,
        0b101,
        "table: {a: Mask, c: Disc}; `fmt_operands` renders NO slots for a \
         `Wide` header, because `FunctionBytecode`'s `Display` handles `Wide` \
         in a separate branch that decodes the header and prints the decoded \
         `WideOp` instead. Structural, not an oversight — but it means the \
         header's own operands are never visible through `fmt_operands`.",
    ),
    (
        Opcode::GenResumeMode,
        0b010,
        "table: {a: Dst, b: InOut}; disassembler prints only `r{a}` — slot `b`, \
         the yield register the instruction conditionally overwrites, is not \
         rendered",
    ),
    (
        Opcode::IteratorClose,
        0b010,
        "table: {a: Src, b: Flag}; disassembler prints only `r{a}` — the \
         completion flag that selects the throw path over the break path is \
         not rendered",
    ),
    (
        Opcode::Await,
        0b100,
        "table: {a: Dst, b: Src, c: Flag}; disassembler prints `r{a}, r{b}` — \
         the async-from-sync flag in `c` is not rendered, so the two \
         async-generator await lowerings are indistinguishable in a listing",
    ),
    (
        Opcode::NewEnvironment,
        0b001,
        "table: {b: Slot}, a unused; disassembler prints `depth={a}, slots={b}` \
         — it renders the static depth the interpreter discards",
    ),
];

/// Where the disassembler's *labels* name a kind of operand the table does not
/// put in that slot.
///
/// A second, sharper class than [`KNOWN_DISASSEMBLER_SLOT_DIVERGENCE`]: here
/// both slots are live in both statements, but the disassembler's label says
/// "this slot holds X" and the table says it holds something else. Found by
/// the label cross-check while writing this table, not by reading it.
///
/// Same `(opcode, mismatching-slot mask, why)` shape, for the same reason.
pub const KNOWN_DISASSEMBLER_LABEL_DIVERGENCE: &[(Opcode, u8, &str)] = &[
    (
        // Also a slot divergence: it renders a slot the table calls dead.
        Opcode::NewEnvironment,
        0b001,
        "table: a is unused; the disassembler's `depth={a}` label names an \
         environment depth there. Same defect as the slot-count entry above, \
         seen through the label check. (`slots={b}` agrees — that is the one \
         live slot.)",
    ),
    (
        Opcode::CreateGenerator,
        0b010,
        "table: {b: FnIndex} — the emitter writes a `Program::functions` index \
         (unit.rs:231); the disassembler renders a bare `r{b}`, i.e. it claims \
         `b` is a register. `Closure`, which has the identical operand, prints \
         `fn#{b}`. The listing therefore shows a register index where the \
         bytecode carries an immediate.",
    ),
];

/// `CreateGenerator` slot `b`: the emitter writes a `Program::functions` index
/// immediate, the interpreter's dormant-generator fallback reads it as a
/// register.
///
/// `unit.rs:231` emits `emit_reg3(CreateGenerator, dst, func_idx, 0)`, and the
/// live dispatch path ignores `rb` entirely (it reuses the generator
/// `prepare_call` already parked on the frame). Only the fallback for
/// hand-built bytecode with no generator on the frame dereferences it, and it
/// dereferences it as `self.stack[base + rb]` (execute.rs:1079-1084). The two
/// readings are indistinguishable in compiled code — the live path always
/// wins — so this is latent, not live. `b` is recorded as
/// [`Role::FnIndex`] because that is what the compiler writes; the fallback's
/// register read is what makes it unsafe to trust.
pub const CREATE_GENERATOR_B_FALLBACK: &str = "emitter writes a function-index immediate; the interpreter's dormant-generator \
     fallback reads the same slot as a register (execute.rs:1079-1084). Only \
     reachable for hand-built bytecode, never for compiler output.";
