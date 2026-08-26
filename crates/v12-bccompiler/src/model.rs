//! Shared compiler state: variable plans, the per-function emission context
//! (`FnCtx`), and the v12 calling/environment ABI the emitter targets.
//!
//! Two passes share one traversal shape ([`crate::collect`] analyzes, the
//! `emit` modules produce code). All identifier resolution keys off oxc
//! `SymbolId`s: declaration sites read
//! `BindingIdentifier::symbol_id`, reference sites resolve
//! `IdentifierReference::reference_id` through `Scoping`, so shadowing and
//! hoisting follow oxc's resolution exactly instead of a hand-rolled name
//! lookup.

use std::collections::{HashMap, HashSet};

use lasso::Key;
use oxc_ast::ast::BlockStatement;
use oxc_semantic::{ReferenceId, Scoping, SymbolId};
use oxc_span::Span;
use v12_bytecode::{Const, FunctionBuilder, Instr, Label, Opcode, WideOp};

/// Register 0 of every frame holds `this` (main unit: `undefined`).
pub const REG_THIS: u8 = 0;

/// Calling convention (see `emit_call`): the callee's register window starts
/// at `callee_reg + 1`, so callee `r0` == caller `r{callee_reg + 1}` == the
/// `this` value, and callee `r{i}` == argument `i - 1`. Arguments therefore
/// occupy `callee_reg + 2 .. callee_reg + 2 + argc` on the caller side and no
/// copying is needed at either boundary.
pub const CALL_HEADER_REGS: u8 = 2; // callee + this

/// Hard cap on registers per function: instruction operand slots are u8 and
/// one slot is reserved as the never-written `undefined` source register.
pub const MAX_REGS: u8 = 255;

/// Upper bound for a single function's environment slot count (narrow u8
/// operand; larger programs should be split).
pub const MAX_ENV_SLOTS: u8 = 255;

/// Native index reserved for the synchronous module import helper.
///
/// See [`crate::unit::NATIVE_IMPORT_INDEX`] and the crate-level docs for
/// the calling convention. Kept here so callers that only depend on the
/// model (e.g., the engine) have a single source of truth.
pub const NATIVE_IMPORT_INDEX: u8 = 254;
pub const NATIVE_IMPORT_INDEX_U32: u32 = NATIVE_IMPORT_INDEX as u32;

// ---------------------------------------------------------------------------
// Interner
// ---------------------------------------------------------------------------

/// Program-global string table backing [`v12_bytecode::Const::Str32`] ids.
///
/// An alias for lasso's single-threaded [`lasso::Rodeo`]. Compilation holds
/// it mutably (see [`crate::compile_source_with_interner`]) so new
/// identifiers and literals intern into it and repeats reuse existing keys;
/// once compilation is done, [`crate::freeze_interner`] turns it into a
/// resolver for key→string lookups.
pub type Interner = lasso::Rodeo<lasso::Spur>;

/// The [`v12_bytecode::Const::Str32`] payload for `key`.
///
/// `Str32(u32)` and [`lasso::Spur`] encode the same sequential table index in
/// different representations (`Spur` wraps a `NonZeroU32`, offset by one).
/// These two functions are the single translation point; they delegate to
/// lasso's own `Key` codec rather than poking at the wrapped integer.
pub(crate) fn str_id_of(key: lasso::Spur) -> u32 {
    u32::try_from(key.into_usize()).expect("a Spur always names a 32-bit table index")
}

/// The [`lasso::Spur`] for `Str32(id)`, or `None` if no `Spur` can name that
/// id (only ids above `u32::MAX - 1` are unrepresentable). Inverse of
/// [`str_id_of`].
#[allow(dead_code)]
pub(crate) fn spur_of_str_id(id: u32) -> Option<lasso::Spur> {
    lasso::Key::try_from_usize(usize::try_from(id).ok()?)
}

// ---------------------------------------------------------------------------
// Module linkage (ESM import / export)
// ---------------------------------------------------------------------------

/// One imported binding from an `import` declaration.
///
/// `specifier` is the module specifier string (`"./a.js"`). `imported` is
/// the name exported by that module (`"default"`, `"*"`, or the exported
/// identifier; empty for side-effect imports). `local` is the local binding
/// introduced by the import (`None` for `import "./side.js"`). `span`
/// records the declaration site for diagnostics.
///
/// Namespace imports (`import * as ns from`) are represented with
/// `imported == "*"`.
#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub specifier: String,
    pub imported: String,
    pub local: Option<SymbolId>,
    pub span: Option<(u32, u32)>,
}

/// One exported binding.
///
/// For `export const x = 1`, `exported == "x"` and `local == Some(symbol_of_x)`.
/// For `export {x as y}`, `exported == "y"` and `local == Some(symbol_of_x)`.
/// For `export default expr`, `exported == "default"`.
/// For `export * from "./m.js"`, `specifier == Some("./m.js")` and
/// `exported == "*"`.
/// `span` records the export site.
#[derive(Debug, Clone)]
pub struct ExportEntry {
    pub specifier: Option<String>,
    pub local: Option<SymbolId>,
    pub exported: String,
    pub span: Option<(u32, u32)>,
}

// ---------------------------------------------------------------------------
// Variable plans (output of the collect pass)
// ---------------------------------------------------------------------------

/// Where a declared variable lives at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarLoc {
    /// A frame register (plain local).
    Reg(u8),
    /// A slot in the home function unit's heap Environment (captured).
    Env(u8),
    /// A property on the global object (`GetGlobal`/`SetGlobal`).
    ///
    /// Used for `var` bindings that alias the global object as well as for
    /// unresolved references to well-known intrinsics (see
    /// [`GLOBAL_INTRINSICS`]). Keeping it distinct from `Reg`/`Env` makes the
    /// global path explicit in `VarAccess` lowering and satisfies the
    /// `collect.rs` → `model.rs` contract for `known-failures.md` bucket 3.
    #[allow(dead_code)]
    Global,
}

/// Names of standard intrinsics that are always present on the global object.
///
/// An unresolved `IdentifierReference` whose text is in this table is treated
/// as a global access (`GetGlobal`/`SetGlobal`) rather than a compile error.
/// The list is intentionally small for v1 — enough to cover `known-failures.md`
/// bucket 3 (`Object`, `Array`, `String`, `Number`, `Boolean`, `Math`, `JSON`,
/// `Error`, …) without claiming full spec coverage. The realm
/// (`v12-engine/src/realm.rs`) and interpreter (`v12-interp`) share the same
/// ordering via their own copies of this list (kept in sync manually for now).
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
    "console",
    "globalThis",
];

/// Per-function-unit layout decided by the collect pass.
#[derive(Debug)]
pub struct UnitPlan {
    /// Enclosing unit index (`None` for the main script body).
    pub parent: Option<usize>,
    /// Arrow functions inherit `this`; see `this_depth_to`.
    pub is_arrow: bool,
    pub name_hint: String,
    /// Every declared symbol in the unit (any nesting depth) → storage.
    pub vars: HashMap<SymbolId, VarLoc>,
    /// Symbols in declaration order (params first), driving register and env
    /// slot assignment deterministically.
    pub decl_order: Vec<SymbolId>,
    /// The unit owns a heap Environment iff some local escapes into an inner
    /// function or an arrow-descendant reads `this`.
    pub has_env: bool,
    /// An arrow-descendant reads `this`, forcing an Environment to thread it.
    pub needs_this: bool,
    pub env_slots: HashMap<SymbolId, u8>,
    pub this_slot: Option<u8>,
    pub env_slot_count: u8,
    /// Declarations at the head of `decl_order` that are function parameters.
    pub param_count: usize,
    /// `true` when the last parameter is a rest parameter.
    pub has_rest: bool,
    /// Strict mode for this unit (inherited + directive).
    pub is_strict: bool,
    /// First free register above params + non-captured locals.
    pub locals_end: u8,
}

impl UnitPlan {
    pub(crate) fn new(parent: Option<usize>, is_arrow: bool, name_hint: String) -> Self {
        Self {
            parent,
            is_arrow,
            name_hint,
            vars: HashMap::new(),
            decl_order: Vec::new(),
            has_env: false,
            needs_this: false,
            env_slots: HashMap::new(),
            this_slot: None,
            env_slot_count: 0,
            param_count: 0,
            has_rest: false,
            is_strict: false,
            locals_end: 1, // r0 = this
        }
    }
}

/// Global layout tables produced by [`crate::collect`].
#[derive(Debug, Default)]
pub struct Plans {
    pub units: Vec<UnitPlan>,
    /// Symbol → declaring unit index.
    pub home_of: HashMap<SymbolId, usize>,
    /// Symbols referenced from a different unit than their home (escapes).
    pub captured: HashSet<SymbolId>,
    /// Function / arrow expression node span → program function index.
    pub fn_index: HashMap<Span, usize>,
    /// `(symbol, referencing unit)` pairs gathered during the walk; joined
    /// into `captured` by the finalize step.
    pub ref_sites: Vec<(SymbolId, usize)>,
    /// `const` bindings (for strict-mode reassignment check).
    pub const_bindings: HashSet<SymbolId>,
    /// Module linkage: imports recorded during collection (module mode only).
    pub imports: Vec<ImportEntry>,
    /// Module linkage: exports recorded during collection (module mode only).
    pub exports: Vec<ExportEntry>,
}

impl Plans {
    /// Number of Environment hops from `from_unit` up to the home unit of
    /// `sym` at an access point inside `from_unit`.
    ///
    /// Environments exist only at function granularity (block-captured vars
    /// are hoisted into the function env), so the hop count is purely
    /// structural: every env-bearing unit strictly between the accessing unit
    /// (inclusive) and the home unit (exclusive) contributes one parent-link
    /// step. This is what makes `GetEnvSlot` depths static per unit pair.
    pub fn env_depth(&self, from_unit: usize, sym: SymbolId) -> u8 {
        let Some(&home) = self.home_of.get(&sym) else {
            return 0;
        };
        if !self.is_descendant_or_self(from_unit, home) {
            return 0;
        }
        self.env_depth_between(from_unit, home)
    }

    /// Hop count from `from_unit`'s innermost environment up to (excluding)
    /// `to_unit`'s environment.
    pub fn env_depth_between(&self, from_unit: usize, to_unit: usize) -> u8 {
        let mut depth = 0u8;
        let mut cur = Some(from_unit);
        while let Some(u) = cur {
            if u == to_unit {
                return depth;
            }
            if self.units[u].has_env {
                depth += 1;
            }
            cur = self.units[u].parent;
        }
        // Not on ancestor chain — return 0 to avoid panic; the access will be treated as global.
        0
    }

    fn is_descendant_or_self(&self, from: usize, of: usize) -> bool {
        let mut cur = Some(from);
        while let Some(u) = cur {
            if u == of {
                return true;
            }
            cur = self.units[u].parent;
        }
        false
    }

    /// The nearest non-arrow ancestor unit (inclusive) — the unit whose `this`
    /// an arrow observes.
    pub fn this_home(&self, unit: usize) -> usize {
        let mut cur = unit;
        while self.units[cur].is_arrow {
            cur = self.units[cur].parent.expect("arrow below main unit");
        }
        cur
    }
}

// ---------------------------------------------------------------------------
// Compiler (program-level mutable state)
// ---------------------------------------------------------------------------

/// Program-global state threaded through both passes.
pub struct Compiler<'s, 'i> {
    pub scoping: &'s Scoping,
    /// Program-wide strict mode (directive prologue / source type); inherited
    /// by all nested units in this subset.
    #[allow(dead_code)]
    pub strict: bool,
    /// Shared string table; emission interns into it mutably so identifiers
    /// seen across compilations reuse their existing keys.
    pub strings: &'i mut Interner,
    pub plans: Plans,
    /// Assembled function bodies; index == program function index. Slots are
    /// placeholders between a nested unit's reservation and its completion.
    pub functions: Vec<v12_bytecode::FunctionBytecode>,
}

impl<'s, 'i> Compiler<'s, 'i> {
    /// Resolves an identifier reference to its symbol, if bound locally.
    pub fn symbol_of(&self, rid: Option<ReferenceId>) -> Option<SymbolId> {
        rid.and_then(|rid| self.scoping.get_reference(rid).symbol_id())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Compilation failure with an optional `(start, end)` source span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub message: String,
    pub span: Option<(u32, u32)>,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.span {
            Some((start, end)) => write!(
                f,
                "{message} (bytes {start}..{end})",
                message = self.message
            ),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for CompileError {}

// ---------------------------------------------------------------------------
// Loop / label / finally bookkeeping
// ---------------------------------------------------------------------------

/// One entry per enclosing loop (or labeled statement) while emitting.
#[derive(Debug, Clone)]
pub struct LoopCtx {
    /// Where `break` lands.
    pub break_label: Label,
    /// Where `continue` lands (`None` for labeled non-loop statements).
    pub continue_label: Option<Label>,
    /// Bound label name, if any.
    pub name: Option<String>,
    /// Length of the finally stack when this loop was entered; `break` /
    /// `continue` leaving the loop runs inline copies of the finallies pushed
    /// since then.
    pub finally_base: usize,
}

/// One active `try … finally` region; intercepted exits (`return`, crossing
/// `break`/`continue`) duplicate its finalizer inline — completion-dispatch
/// duplication of its finalizer inline.
pub struct FinallyCtx<'a> {
    /// The finalizer block, duplicated on every intercepted exit path.
    pub body: &'a BlockStatement<'a>,
}

// ---------------------------------------------------------------------------
// FnCtx — the per-function emission context
// ---------------------------------------------------------------------------

/// Everything one function unit's emitter needs. Shared program state lives
/// in [`Compiler`] behind `comp`; nested units recurse through
/// [`crate::unit::compile_unit`] with a fresh `FnCtx`.
pub struct FnCtx<'c, 's, 'i, 'a> {
    pub comp: &'c mut Compiler<'s, 'i>,
    pub b: FunctionBuilder,
    pub unit: usize,
    /// Next temporary register (starts above `locals_end`; `locals_end`
    /// itself stays reserved as the undefined source register).
    temp_top: u8,
    /// One past the highest register any emission touched.
    high_water: u8,
    /// Maximum `stack_depth + 1` across all handlers in this unit.
    pub(crate) handler_max: u32,
    pub loops: Vec<LoopCtx>,
    pub finallies: Vec<FinallyCtx<'a>>,
    overflow: Option<CompileError>,
}

impl<'c, 's, 'i, 'a> FnCtx<'c, 's, 'i, 'a> {
    pub fn new(comp: &'c mut Compiler<'s, 'i>, unit: usize) -> Self {
        let locals_end = comp.plans.units[unit].locals_end;
        Self {
            comp,
            b: FunctionBuilder::new(None),
            unit,
            temp_top: locals_end + 1, // skip the reserved undefined register
            high_water: locals_end + 1,
            handler_max: 0,
            loops: Vec::new(),
            finallies: Vec::new(),
            overflow: None,
        }
    }

    /// Error with a source span, for use mid-emission.
    pub fn err(&self, span: oxc_span::Span, message: impl Into<String>) -> CompileError {
        CompileError {
            message: message.into(),
            span: Some((span.start, span.end)),
        }
    }

    // -- registers ----------------------------------------------------------

    pub fn new_temp(&mut self) -> u8 {
        if self.overflow.is_some() {
            return 0;
        }
        let r = self.temp_top;
        let Some(next) = self.temp_top.checked_add(1) else {
            self.overflow = Some(CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            });
            return 0;
        };
        self.temp_top = next;
        if let Err(e) = self.track(r) {
            self.overflow = Some(e);
            return 0;
        }
        r
    }

    /// Contiguous block for call layouts and array elements.
    pub fn new_temps(&mut self, n: u8) -> u8 {
        if self.overflow.is_some() {
            return 0;
        }
        let base = self.temp_top;
        let Some(next) = self.temp_top.checked_add(n) else {
            self.overflow = Some(CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            });
            return 0;
        };
        self.temp_top = next;
        let top = base.checked_add(n.saturating_sub(1)).unwrap_or(base);
        if let Err(e) = self.track(top) {
            self.overflow = Some(e);
            return 0;
        }
        base
    }

    pub fn temp_mark(&self) -> u8 {
        self.temp_top
    }

    pub fn temp_release(&mut self, mark: u8) {
        self.temp_top = mark.min(self.temp_top);
    }

    fn track(&mut self, reg: u8) -> Result<(), CompileError> {
        if u16::from(reg) >= u16::from(MAX_REGS) {
            return Err(CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            });
        }
        self.high_water = self.high_water.max(reg + 1);
        Ok(())
    }

    /// The reserved never-written register; reading it yields `undefined`
    /// (all registers initialize to `undefined` — frame ABI).
    pub fn undef_reg(&self) -> u8 {
        self.comp.plans.units[self.unit].locals_end
    }

    // -- variable access ----------------------------------------------------

    /// Resolve a symbol to concrete storage relative to the current unit.
    pub fn access(&self, sym: SymbolId) -> VarAccess {
        let plans = &self.comp.plans;
        let Some(&home) = plans.home_of.get(&sym) else {
            return VarAccess::Reg(0);
        };
        let Some(&loc) = plans.units[home].vars.get(&sym) else {
            return VarAccess::Reg(0);
        };
        match loc {
            VarLoc::Reg(r) => VarAccess::Reg(r),
            VarLoc::Env(slot) => VarAccess::Env {
                depth: plans.env_depth(self.unit, sym),
                slot,
            },
            VarLoc::Global => VarAccess::Global { sym },
        }
    }

    // -- primitive emissions -------------------------------------------------

    pub fn emit(&mut self, instr: Instr) {
        self.b.emit(instr);
    }

    pub fn emit_op(&mut self, op: Opcode, a: u8, bb: u8, c: u8) {
        self.b.emit(Instr::new(op, a, bb, c));
    }

    pub fn emit_spanned(&mut self, instr: Instr, span: oxc_span::Span) {
        self.b.emit_spanned(instr, (span.start, span.end));
    }

    pub fn label(&mut self) -> Label {
        self.b.label()
    }

    pub fn bind(&mut self, l: Label) {
        self.b.bind(l);
    }

    pub fn pc(&self) -> u32 {
        self.b.pc()
    }

    pub fn emit_jump(&mut self, op: Opcode, cond: u8, target: Label) {
        self.b.emit_jump(op, cond, target);
    }

    pub fn add_const(&mut self, c: Const) -> Result<u16, CompileError> {
        self.b.add_const(c).map_err(|e| CompileError {
            message: e,
            span: None,
        })
    }

    /// Loads a pooled constant into `dst` (16-bit const ids always fit).
    pub fn load_const(
        &mut self,
        dst: u8,
        konst: Const,
        span: oxc_span::Span,
    ) -> Result<(), CompileError> {
        let k = self.add_const(konst)?;
        self.emit_spanned(Instr::new_imm16(Opcode::LoadConst, dst, k), span);
        Ok(())
    }

    /// Loads a string literal (interned) into `dst`.
    pub fn load_str(&mut self, dst: u8, s: &str, span: oxc_span::Span) -> Result<(), CompileError> {
        let id = str_id_of(self.comp.strings.get_or_intern(s));
        self.load_const(dst, Const::Str32(id), span)
    }

    /// Loads an i64 as narrowly as the encoding allows.
    pub fn load_int(&mut self, dst: u8, v: i64, span: oxc_span::Span) {
        if (-128..=127).contains(&v) {
            self.emit_spanned(Instr::new(Opcode::LoadInt, dst, 0, v as i8 as u8), span);
        } else {
            let words = WideOp::LoadIntW { dst, value: v }.encode();
            self.emit_words(words, span);
        }
    }

    /// Emits a multi-word (wide) sequence with every word carrying `span`, so
    /// the spans vector stays index-aligned with instructions.
    pub fn emit_words(&mut self, words: Vec<Instr>, span: oxc_span::Span) {
        for w in words {
            self.emit_spanned(w, span);
        }
    }

    /// `undefined` materialization: copy from the reserved register.
    pub fn load_undefined(&mut self, dst: u8, span: oxc_span::Span) {
        let src = self.undef_reg();
        self.emit_spanned(Instr::new(Opcode::Move, dst, src, 0), span);
    }

    /// Boolean literals: comparisons yield booleans, so `0 == 0` / `0 != 0`
    /// materialize `true` / `false` without a boolean constant kind.
    pub fn load_bool(&mut self, dst: u8, v: bool, span: oxc_span::Span) {
        let t = self.new_temp();
        self.load_int(t, 0, span);
        let op = if v { Opcode::Eq } else { Opcode::Ne };
        self.emit_spanned(Instr::new(op, dst, t, t), span);
    }

    pub fn move_reg(&mut self, dst: u8, src: u8, span: oxc_span::Span) {
        self.emit_spanned(Instr::new(Opcode::Move, dst, src, 0), span);
    }

    /// `GetEnvSlot` with wide fallback once depth/slot exceed u8.
    pub fn emit_get_env(&mut self, dst: u8, depth: u8, slot: u8, span: oxc_span::Span) {
        self.emit_spanned(Instr::new(Opcode::GetEnvSlot, dst, depth, slot), span);
    }

    pub fn emit_set_env(&mut self, depth: u8, slot: u8, src: u8, span: oxc_span::Span) {
        self.emit_spanned(Instr::new(Opcode::SetEnvSlot, depth, slot, src), span);
    }

    pub fn emit_get_global(&mut self, dst: u8, name_id: u32, span: oxc_span::Span) {
        let k = u16::try_from(name_id).expect("global name id fits u16");
        self.emit_spanned(Instr::new_imm16(Opcode::GetGlobal, dst, k), span);
    }

    pub fn emit_set_global(&mut self, name_id: u32, src: u8, span: oxc_span::Span) {
        let k = u16::try_from(name_id).expect("global name id fits u16");
        self.emit_spanned(Instr::new_imm16(Opcode::SetGlobal, src, k), span);
    }

    /// `Call` with the documented layout; wide encoding for large arities.
    pub fn emit_call(&mut self, dst: u8, callee: u8, argc: u16, span: oxc_span::Span) {
        if argc <= u16::from(u8::MAX) {
            self.emit_spanned(Instr::new(Opcode::Call, dst, callee, argc as u8), span);
        } else {
            let words = WideOp::CallW {
                dst,
                func: callee,
                argc,
            }
            .encode();
            self.emit_words(words, span);
        }
    }

    // -- finish ---------------------------------------------------------------

    /// Assembles, optimizes, and validates the unit's bytecode.
    pub fn finish(mut self) -> Result<v12_bytecode::FunctionBytecode, CompileError> {
        if let Some(err) = self.overflow.take() {
            return Err(err);
        }
        let locals_end = self.comp.plans.units[self.unit].locals_end;
        let mut regs = self.high_water.max(locals_end + 1);
        // Handlers deliver the exception into register `stack_depth`, so the
        // register window must include that slot. `high_water` already
        // accounts for the delivery temporary, but an explicit max over the
        // tracked handler depth guards against future emission paths that
        // might set `stack_depth` without going through `new_temp`.
        if self.handler_max > u32::from(regs) {
            regs = u8::try_from(self.handler_max).unwrap_or(MAX_REGS);
        }
        self.b.reserve_regs(u16::from(regs));
        let mut fb = self.b.finish();
        // Handler ranges are pushed as regions close (inner regions first),
        // so sort by start pc; equal starts mean shared entry points, where
        // the OUTER (longer) range must come first for the nesting check.
        fb.handlers
            .sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        crate::peephole::optimize(&mut fb);
        fb.validate().map_err(|e| CompileError {
            message: format!("validate: {e}"),
            span: None,
        })?;
        Ok(fb)
    }
}

/// Concrete storage for one symbol relative to the emitting unit.
#[derive(Debug, Clone, Copy)]
pub enum VarAccess {
    Reg(u8),
    Env {
        depth: u8,
        slot: u8,
    },
    /// A global property (`GetGlobal`/`SetGlobal`) — the symbol's name is
    /// interned at emission time.
    Global {
        sym: SymbolId,
    },
}
