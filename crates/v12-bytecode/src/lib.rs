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

pub mod error;

pub use error::BytecodeError;

/// Source span as `(start, end)` byte offsets. Shape-compatible with
/// `oxc_span::Span`'s start/end pair so front-end spans forward without a
/// conversion layer.
pub type SpanPair = (u32, u32);


mod analysis;
mod builder;
mod opcode;
mod wide;

pub use analysis::{CountedLoop, MAX_INLINE_SIZE, find_counted_loop, is_inline_candidate,
    is_loop_header, logical_pcs, loop_headers, next_logical_pc};
pub use builder::{FunctionBuilder, Label};
pub use opcode::{Instr, Opcode, MAX_IMM24};
pub use wide::WideOp;

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
    pub fn insert(&mut self, constant: Const) -> Result<u16, BytecodeError> {
        let key = constant.key();
        if let Some(&existing) = self.index.get(&key) {
            return Ok(existing);
        }
        if self.consts.len() >= MAX_CONSTANTS {
            return Err(BytecodeError::ConstantPoolFull);
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
    /// The spec `SetFunctionName` value for this closure, when statically
    /// known (methods, class constructors). `None` for anonymous/arrow fns.
    pub function_name: Option<String>,
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
    /// `ExpectedArgumentCount` (ES 14.1.6): the number of formal parameters
    /// before the rest parameter or the first parameter with an initializer.
    /// Read as the function's own `length` property at closure creation.
    /// u16: bounded by the register file like `fixed_params`.
    pub expected_args: u16,
    /// The body references the `arguments` object (unbound identifier), so
    /// call paths materialize it into the frame's arguments slot.
    pub needs_arguments: bool,
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
            function_name: None,
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
            expected_args: 0,
            needs_arguments: false,
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
    pub fn validate(&self) -> Result<(), BytecodeError> {
        if self.max_regs == 0 {
            return Err(BytecodeError::ZeroMaxRegs);
        }
        // Stack of still-open ranges; sortedness makes this a nesting check.
        let mut open: Vec<&HandlerRange> = Vec::new();
        let mut prev_start = 0;
        for h in &self.handlers {
            if h.end <= h.start {
                return Err(BytecodeError::InvalidFunction {
                    reason: format!(
                        "handler range [{}, {}) is empty or inverted",
                        h.start, h.end
                    ),
                });
            }
            if h.start < prev_start {
                return Err(BytecodeError::InvalidFunction {
                    reason: format!("handler starting at {} is not sorted by start", h.start),
                });
            }
            if h.target as usize >= self.instrs.len() {
                return Err(BytecodeError::HandlerTargetOutOfBounds {
                    target: h.target,
                    instrs: self.instrs.len(),
                });
            }
            while open.last().is_some_and(|top| top.end <= h.start) {
                open.pop();
            }
            if let Some(top) = open.last() {
                if h.end > top.end {
                    return Err(BytecodeError::InvalidFunction {
                        reason: format!(
                            "handler [{}, {}) partially overlaps [{}, {}) without nesting",
                            h.start, h.end, top.start, top.end
                        ),
                    });
                }
                if h.stack_depth <= top.stack_depth {
                    return Err(BytecodeError::InvalidFunction {
                        reason: format!(
                            "nested handler [{}, {}) has non-increasing stack depth ({} <= {})",
                            h.start, h.end, h.stack_depth, top.stack_depth
                        ),
                    });
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

/// Names of the standard intrinsics the v1 realm installs, in slot order.
///
/// This is the canonical copy of the table: `v12-engine` (realm creation,
/// slots 0..len of the global object's `properties`) and `v12-interp`
/// (`GLOBAL_VAR_OFFSET` bias, intrinsic fast paths) both read it from here,
/// so the order/length contract is enforced by sharing instead of by
/// hand-synced duplicates. The list is intentionally small for v1 — enough
/// to cover `known-failures.md` bucket 3 (`Object`, `Array`, `String`,
/// `Number`, `Boolean`, `Math`, `JSON`, `Error`, …) without claiming full
/// spec coverage.
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
    "Promise",
    "Symbol",
    "Map",
    "Set",
    "RegExp",
    "eval",
    "console",
    "globalThis",
    // Appended (never mid-inserted): index 20. Appending only biases
    // `GLOBAL_VAR_OFFSET`, so no existing intrinsic index changes.
    "Proxy",
];

/// Offset of user-declared global slots in the global object's `properties`.
///
/// The realm pushes exactly [`GLOBAL_INTRINSICS`] intrinsic slots onto the
/// global's `properties` vector without shape descriptors, so any slot a
/// shape descriptor reports for the global must be biased by this constant
/// before indexing storage. Derived from the table itself, so it cannot
/// drift.
pub const GLOBAL_VAR_OFFSET: usize = GLOBAL_INTRINSICS.len();

/// Names the compiler treats as global references (`GetGlobal`/`SetGlobal`)
/// even when no binding exists — a superset of [`GLOBAL_INTRINSICS`] that
/// also covers error constructors the v1 realm does not install as intrinsic
/// slots. An unresolved `IdentifierReference` outside this table is a
/// compile error.
pub const GLOBAL_ACCESS_INTRINSICS: &[&str] = &[
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
    "RegExp",
    "eval",
    "console",
    "globalThis",
    // Kept in the same order as `GLOBAL_INTRINSICS` (this table is its
    // superset); the two extra error constructors above are the only pins
    // before it.
    "Proxy",
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
        Opcode::GetGlobalLenient => "get_global_lenient",
        Opcode::GenResumeMode => "gen_resume_mode",
        Opcode::SetGlobal => "set_global",
        Opcode::Construct => "construct",
        Opcode::ToNumber => "to_number",
        Opcode::MergeObject => "merge_object",
        Opcode::DefineAccessor => "define_accessor",
        Opcode::JumpIfNullish => "jump_if_nullish",
        Opcode::SetPrototype => "set_prototype",
        Opcode::GetIterator => "get_iterator",
        Opcode::IteratorNext => "iterator_next",
        Opcode::IteratorClose => "iterator_close",
        Opcode::DefineMethod => "define_method",
        Opcode::GetNewTarget => "get_new_target",
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
        Opcode::GenResumeMode => write!(f, " r{a}"),
        Opcode::MergeObject => write!(f, " r{b}, r{c}"),
        Opcode::DefineAccessor => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::SetPrototype => write!(f, " r{b}, r{c}"),
        Opcode::GetIterator => write!(f, " r{a}, r{b}"),
        Opcode::IteratorNext => write!(f, " r{a}, r{b}"),
        Opcode::IteratorClose => write!(f, " r{a}"),
        Opcode::JumpIfNullish => write!(f, " r{a}, -> {}", i.imm16()),
        Opcode::Jump => write!(f, " -> {}", i.imm24()),
        Opcode::JumpIfFalse | Opcode::JumpIfTrue => write!(f, " r{a}, -> {}", i.imm16()),
        Opcode::LoopHeader => Ok(()),
        Opcode::Call | Opcode::Construct => write!(f, " r{a}, r{b}, argc={c}"),
        Opcode::Return | Opcode::Throw => write!(f, " r{a}"),
        Opcode::GetProperty | Opcode::DeleteProperty => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::SetProperty => write!(f, " r{a}, r{b}, r{c}"),
        Opcode::DefineMethod => write!(f, " r{a}, r{b}, r{c}"),
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
        Opcode::GetGlobalLenient => write!(f, " r{a}, k{}", i.imm16()),
        Opcode::SetGlobal => write!(f, " k{}, r{a}", i.imm16()),
        Opcode::GetNewTarget => write!(f, " r{a}"),
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
        let mut fb =
            FunctionBytecode::with_instructions(vec![Instr::new(Opcode::Move, 0, 0, 0); 4], 2);
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
        assert!(err.to_string().contains("not sorted"), "{err}");
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
        assert!(err.to_string().contains("out of bounds"), "{err}");
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
        assert!(err.to_string().contains("partially overlaps"), "{err}");
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
        assert!(
            err.to_string().contains("non-increasing stack depth"),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_zero_max_regs() {
        let mut fb = nop_fn(Vec::new());
        fb.max_regs = 0;
        assert_eq!(fb.validate().unwrap_err(), BytecodeError::ZeroMaxRegs);
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
