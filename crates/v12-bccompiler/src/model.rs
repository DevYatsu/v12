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
use oxc_semantic::{ReferenceId, ScopeId, Scoping, SymbolId};
use oxc_span::Span;
use v12_bytecode::{
    Const, EvalCallerFacts, EvalPhase, FunctionBuilder, Instr, Label, Opcode, WideOp,
};

/// Register 0 of every frame holds `this` (main unit: `undefined`).
pub const REG_THIS: u16 = 0;

/// Calling convention (see `emit_call`): the callee's register window starts
/// at `callee_reg + 1`, so callee `r0` == caller `r{callee_reg + 1}` == the
/// `this` value, and callee `r{i}` == argument `i - 1`. Arguments therefore
/// occupy `callee_reg + 2 .. callee_reg + 2 + argc` on the caller side and no
/// copying is needed at either boundary.
pub const CALL_HEADER_REGS: u16 = 2;

/// Hard cap on registers per function.
///
/// WHY u16: narrow instructions address registers with 8-bit slots, so
/// functions whose register window exceeds 255 escape through the
/// [`WideOp::RegExt`] prefix (16-bit register operands). 65 535 is the
/// largest value the escape can express (high byte + low byte); it also
/// bounds the interpreter frame window, which is indexed by
/// [`FunctionBytecode::max_regs`].
pub const MAX_REGS: u16 = 65_535;

/// Upper bound for a single function's environment slot count.
///
/// Mirrors [`MAX_REGS`]: narrow `GetEnvSlot`/`SetEnvSlot`/`NewEnvironment`
/// carry 8-bit slot operands; larger slot counts escape through
/// `WideOp::GetEnvSlotW`/`SetEnvSlotW`/`NewEnvironmentW` (16-bit operands).
pub const MAX_ENV_SLOTS: u16 = 65_535;

/// Native index reserved for the synchronous module import helper.
///
/// See [`crate::unit::NATIVE_IMPORT_INDEX`] and the crate-level docs for
/// the calling convention. Kept here so callers that only depend on the
/// model (e.g., the engine) have a single source of truth.
///
/// Caveat: the engine's `NativeRegistry` hardcodes this index, so it cannot
/// move to free the whole u16 function-index range. A module program whose
/// function table reaches index 254 would collide with it; such programs are
/// outside the supported envelope (see the `emit_closure_instr` limit).
pub const NATIVE_IMPORT_INDEX: u16 = 254;
pub const NATIVE_IMPORT_INDEX_U32: u32 = NATIVE_IMPORT_INDEX as u32;

/// Native index for `Object.enumerableOwnKeys` - returns array of own enumerable property keys.
/// Must match `v12_native::NativeId::ObjectEnumerableOwnKeys` (1003).
pub const NATIVE_OBJECT_ENUMERABLE_OWN_KEYS: u32 = 1003;

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
///
/// String identities are split across two bytecode namespaces that happen to
/// share the same underlying `Rodeo` storage but must never be conflated:
///
/// - `GetGlobal`/`SetGlobal` carry a `PropKey` via `lasso::Spur` directly: the
///   instruction's 16-bit immediate is the string table index (`Spur`'s
///   `into_usize`) for the global name (`"console"`).
/// - `LoadConst` carries a `Const::Str32(u32)` via the per-function
///   `ConstantPool`: the instruction's immediate is a pool index, and the
///   pooled `Str32` payload is the string table index for the literal
///   (`"log"`).
///
/// Both string ids originate from the same `Rodeo`, but the immediates live in
/// different tables (`strings` vs `consts`), so `k0` in `get_global r,a,k0`
/// and `k0` in `load_const r,k0` are unrelated. `str_id_of` / `spur_of_str_id`
/// are the only conversion points between the two representations.
pub type Interner = lasso::Rodeo<lasso::Spur>;

/// Emission result alias, shared by the emit modules (`expr.rs`, `stmt.rs`
/// declare the same shape locally).
pub type Res<T> = Result<T, CompileError>;

/// The [`v12_bytecode::Const::Str32`] payload for `key`.
///
/// `Str32(u32)` and [`lasso::Spur`] encode the same sequential table index in
/// different representations (`Spur` wraps a `NonZeroU32`, offset by one).
/// These two functions are the single translation point; they delegate to
/// lasso's own `Key` codec rather than poking at the wrapped integer.
///
/// See the `Interner` docs for why `Spur` (used for `GetGlobal`'s `PropKey`)
/// and `Str32` (used for `LoadConst`'s pooled string) share the `Rodeo` but
/// occupy distinct immediate namespaces.
// A `Spur` always names a 32-bit table index by construction; audited
// invariant of the lasso codec.
#[allow(clippy::expect_used)]
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
///
/// `deferred` is set for `import defer * as ns from` (the defer-import-eval
/// proposal): the dependency is linked but its evaluation is postponed until
/// an export is read through the namespace. The loader skips evaluating
/// deferred dependencies; ordinary imports keep `deferred == false`.
#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub specifier: String,
    pub imported: String,
    pub local: Option<SymbolId>,
    pub span: Option<(u32, u32)>,
    pub deferred: bool,
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
    /// Source export name for `export {x as y} from "./m.js"`.
    pub imported: Option<String>,
    pub local: Option<SymbolId>,
    pub exported: String,
    pub span: Option<(u32, u32)>,
}

/// Linkage role for one seeded module-main binding slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleBindingSource {
    /// A binding declared by this module.
    Local,
    /// `import ... from`; the slot aliases the source export cell.
    Import {
        specifier: String,
        imported: String,
        deferred: bool,
    },
    /// `export {x as y} from`; the slot aliases the source export cell.
    ReExport { specifier: String, imported: String },
    /// `export default <expression>` has no Oxc symbol and uses a synthetic slot.
    DefaultExpression,
}

/// Runtime seeding/linkage plan for a module-main environment slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleBindingPlan {
    pub symbol: Option<SymbolId>,
    pub name: String,
    pub env_slot: u16,
    pub source: ModuleBindingSource,
    /// Export names backed by this same cell. Empty for non-exported imports.
    pub exports: Vec<String>,
    /// Module `var` and function bindings are initialized during declaration
    /// instantiation; lexical bindings begin uninitialized (TDZ).
    pub initially_initialized: bool,
}

// ---------------------------------------------------------------------------
// Variable plans (output of the collect pass)
// ---------------------------------------------------------------------------

/// Where a declared variable lives at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarLoc {
    /// A frame register (plain local). u16: functions with more than 255
    /// registers address the upper range through `WideOp::RegExt`.
    Reg(u16),
    /// A slot in the home function unit's heap Environment (captured).
    /// u16: slot counts beyond 255 escape through the `…W` env-slot ops.
    Env(u16),
    /// A property on the global object (`GetGlobal`/`SetGlobal`).
    ///
    /// Used for `var` bindings that alias the global object as well as for
    /// unresolved references to well-known intrinsics (see
    /// [`v12_bytecode::GLOBAL_INTRINSICS`]). Keeping it distinct from
    /// `Reg`/`Env` makes the global path explicit in `VarAccess` lowering and
    /// satisfies the `collect.rs` → `model.rs` contract for
    /// `known-failures.md` bucket 3.
    #[allow(dead_code)]
    Global,
}

#[derive(Debug)]
pub(crate) struct EvalSitePlan {
    pub span: Span,
    pub phase: EvalPhase,
    pub scope_id: Option<ScopeId>,
    pub pc: Option<u32>,
    pub caller: EvalCallerFacts,
}

/// Per-function-unit layout decided by the collect pass.
/// Why a block-level function cannot update the function-scope Annex B name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFunctionSkip {
    Eligible,
    ParameterCollision,
    LexicalCollision,
}

/// One sloppy function-body Annex B block-function declaration.
///
/// The block binding and the optional function-scope target are deliberately
/// independent. A block function has one Oxc symbol, but its runtime name can
/// have two independent storage locations while the declaration is active.
#[derive(Debug, Clone)]
pub struct BlockFunctionPlan {
    pub order: usize,
    pub span: Span,
    pub declaration_scope: ScopeId,
    pub symbol: SymbolId,
    pub child_unit: usize,
    pub lexical_location: Option<VarLoc>,
    pub var_location: Option<VarLoc>,
    pub(crate) var_symbol: Option<SymbolId>,
    pub skip: BlockFunctionSkip,
}

#[derive(Debug)]
pub struct UnitPlan {
    /// Enclosing unit index (`None` for the main script body).
    pub parent: Option<usize>,
    /// Arrow functions inherit `this`; see `this_depth_to`.
    pub is_arrow: bool,
    pub name_hint: String,
    /// Statically-known `SetFunctionName` value for the unit's closure.
    pub function_name: Option<String>,
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
    pub env_slots: HashMap<SymbolId, u16>,
    pub this_slot: Option<u16>,
    pub env_slot_count: u16,
    /// TDZ ("temporal dead zone") flag locations for the unit's `let`/`const`
    /// bindings — see [`UnitPlan::tdz_flag_of`].
    ///
    /// A `let`/`const` binding exists from the moment its scope is entered but
    /// holds no value until its declaration *executes*; a reference in between
    /// is a `ReferenceError` (ES §9.1.1 step 2 / §8.5.2). There is no such
    /// state in `vars`, so each guarded binding gets one *parallel* cell that
    /// lives in the same storage class as its value (see
    /// [`VarLoc::Reg`]/[`VarLoc::Env`]). `undefined` is the uninitialized
    /// state, which is free: registers start `undefined` per the frame ABI
    /// and `JsObject::environment` fills fresh slots with `undefined`.
    pub tdz_flags: HashMap<SymbolId, VarLoc>,
    /// Number of top-level formal parameters (excluding a rest parameter).
    /// `r{i+1}` receives formal `i` by the call ABI (`r0` is `this`).
    pub arity: usize,
    /// For each formal (`len() == arity`): the top-level binding identifier,
    /// or `None` when the formal is a destructuring pattern whose incoming
    /// register is reserved as scratch for the prologue destructure.
    pub formal_idents: Vec<Option<SymbolId>>,
    /// The rest parameter's top-level binding identifier when it is a simple
    /// identifier (`...a`). `None` for a pattern rest or no rest.
    pub rest_ident: Option<SymbolId>,
    /// `true` when the last parameter is a rest parameter.
    pub has_rest: bool,
    /// `ExpectedArgumentCount`: formal parameters before the rest parameter
    /// or the first parameter with an initializer (the function's `length`).
    pub expected_args: usize,
    /// The body references the unbound `arguments` identifier, so call
    /// paths must materialize an arguments object for this unit.
    pub needs_arguments: bool,
    /// Strict mode for this unit (inherited + directive).
    pub is_strict: bool,
    /// The method/constructor body references `super`; its unit must resolve
    /// the class env captured by its closure.
    pub uses_super: bool,
    /// The unit is a class method with the `static` modifier: `super.x`
    /// resolves against the parent constructor directly instead of its
    /// `prototype`.
    pub static_method: bool,
    /// Object-literal method whose `super` fallback uses the call receiver.
    pub is_object_method: bool,
    /// The unit is lexically a class member (constructor, method, or field
    /// initializer), so `super` property/call references are legal in it.
    /// Nested ordinary functions reset this; nested arrows inherit it.
    pub allows_super: bool,
    /// First free register above params + non-captured locals.
    pub locals_end: u16,
    /// Syntactic `eval(...)` call sites in this unit.
    pub direct_eval_sites: Vec<EvalSitePlan>,
    /// Whether this unit contains a syntactic direct eval call.
    pub eval_reachable: bool,
    /// Runtime binding descriptors emitted with the function bytecode.
    pub binding_descriptors: Vec<(String, VarLoc)>,
    /// Formal parameter names in this unit.
    pub parameter_names: HashSet<String>,
    /// Function-scoped `var` names in this unit.
    pub var_names: HashSet<String>,
    /// Lexical names in this unit.
    pub lexical_names: HashSet<String>,
    /// Whether this ordinary function has an implicit arguments binding.
    pub implicit_arguments: bool,
    pub has_non_simple_parameters: bool,
    pub var_declared_names: Vec<String>,
    pub parameter_symbols: HashSet<SymbolId>,
    pub var_symbols: HashSet<SymbolId>,
    pub lexical_symbols: HashSet<SymbolId>,
    /// Function-body block-function candidates in source order.
    pub block_functions: Vec<BlockFunctionPlan>,
    /// Oxc function scope for this unit, when it has one.
    pub function_scope: Option<ScopeId>,
}

impl UnitPlan {
    pub(crate) fn new(parent: Option<usize>, is_arrow: bool, name_hint: String) -> Self {
        Self {
            parent,
            is_arrow,
            name_hint,
            function_name: None,
            vars: HashMap::new(),
            decl_order: Vec::new(),
            has_env: false,
            needs_this: false,
            env_slots: HashMap::new(),
            this_slot: None,
            env_slot_count: 0,
            tdz_flags: HashMap::new(),
            arity: 0,
            formal_idents: Vec::new(),
            rest_ident: None,
            has_rest: false,
            expected_args: 0,
            needs_arguments: false,
            is_strict: false,
            uses_super: false,
            static_method: false,
            allows_super: false,
            is_object_method: false,
            locals_end: 1, // r0 = this
            direct_eval_sites: Vec::new(),
            eval_reachable: false,
            binding_descriptors: Vec::new(),
            parameter_names: HashSet::new(),
            var_names: HashSet::new(),
            lexical_names: HashSet::new(),
            implicit_arguments: false,
            has_non_simple_parameters: false,
            var_declared_names: Vec::new(),
            parameter_symbols: HashSet::new(),
            var_symbols: HashSet::new(),
            lexical_symbols: HashSet::new(),
            block_functions: Vec::new(),
            function_scope: None,
        }
    }

    /// Where `sym`'s TDZ flag lives, or `None` when `sym` is never in a TDZ.
    ///
    /// Only `let`/`const` bindings get a flag. Function declarations hoist and
    /// are initialized at scope entry (ES §9.2.12), `var` bindings are created
    /// with the function's environment record and initialized to `undefined`
    /// there (ES §9.1.1 step 11), and formal parameters are bound before the
    /// body runs — so none of them can be observed uninitialized, and adding a
    /// flag for them would only cost a check.
    pub(crate) fn tdz_flag_of(&self, sym: SymbolId) -> Option<VarLoc> {
        self.tdz_flags.get(&sym).copied()
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
    /// Source names for declared symbols, used to emit eval binding metadata.
    pub symbol_names: HashMap<SymbolId, String>,
    /// Function / arrow expression node span → program function index.
    pub fn_index: HashMap<Span, usize>,
    /// `(symbol, referencing unit)` pairs gathered during the walk; joined
    /// into `captured` by the finalize step.
    pub ref_sites: Vec<(SymbolId, usize)>,
    /// `const` bindings (for strict-mode reassignment check).
    pub const_bindings: HashSet<SymbolId>,
    /// `var` bindings declared at the top level (unit 0) that alias the
    /// global object (`VarLoc::Global` → `GetGlobal`/`SetGlobal`).
    pub global_vars: HashSet<SymbolId>,
    /// `true` when the source was parsed as a module (top-level `var` stays
    /// module-scoped, not global). Scripts have `false`.
    pub is_module: bool,
    /// Module linkage: imports recorded during collection (module mode only).
    pub imports: Vec<ImportEntry>,
    /// Module linkage: exports recorded during collection (module mode only).
    pub exports: Vec<ExportEntry>,
    /// Seeded module-main environment slots and their cell-link sources.
    pub module_bindings: Vec<ModuleBindingPlan>,
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
    // Arrow units always have a parent (a non-arrow main unit exists);
    // audited invariant.
    #[allow(clippy::expect_used)]
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
    /// Shared string table; emission interns into it mutably so identifiers
    /// seen across compilations reuse their existing keys.
    pub strings: &'i mut Interner,
    pub plans: Plans,
    /// Assembled function bodies; index == program function index. Slots are
    /// placeholders between a nested unit's reservation and its completion.
    pub functions: Vec<v12_bytecode::FunctionBytecode>,
    /// Emit binding metadata for every unit in a compiled eval payload.
    pub emit_eval_metadata: bool,
    pub is_eval_payload: bool,
    pub eval_var_target: v12_bytecode::EvalVarTarget,
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
    /// Register holding the loop's iterator (`for-of` only): an abrupt exit
    /// (`break`/`return`) emits `IteratorClose` on it, innermost loop first.
    pub close_iter: Option<u16>,
    /// Register holding a boolean "close already in progress" guard for the
    /// same loop. The break/return path sets it before calling `return()`:
    /// that call happens *inside* the loop's exception range, so if it
    /// throws, the loop's own handler must not close a second time (spec
    /// 7.4.6 — the close error is itself the completion). The handler tests
    /// the guard and rethrows the original abrupt without re-closing.
    pub close_guard: Option<u16>,
    /// This loop body runs inside a per-iteration Environment pushed by
    /// [`crate::FnCtx::emit_per_iteration_env`] (spec
    /// `CreatePerIterationEnvironment`).
    ///
    /// The push is *not* balanced by the body's own fallthrough alone: every
    /// abrupt exit out of the body has to retire it too, and the runtime chain
    /// grows one hop per iteration if it does not. That is why the flag lives
    /// on the loop context — it is what lets
    /// [`crate::FnCtx::emit_per_iteration_pops`] find the environments a
    /// `break`/`continue` is leaving behind. The environment's live range is
    /// the whole body, so `return` (which discards the frame) and a `throw`
    /// that escapes the frame are the only exits that may skip the pop.
    pub per_iter_env: bool,
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
    temp_top: u16,
    /// One past the highest register any emission touched.
    high_water: u16,
    /// Maximum `stack_depth + 1` across all handlers in this unit.
    pub(crate) handler_max: u32,
    pub loops: Vec<LoopCtx>,
    pub finallies: Vec<FinallyCtx<'a>>,
    /// Nesting depth of non-straight-line regions (branch bodies, loop bodies,
    /// `switch`, `try`/`catch`/`finally`, `with`) in the unit being emitted.
    ///
    /// A declarator emitted while this is non-zero cannot be *proven* to have
    /// run before a later reference: control may have reached that reference
    /// without passing the declarator at all (`switch` clauses share one block
    /// scope, and a `try` body may resume after a throw). Inside such a region
    /// [`FnCtx::tdz_init`] therefore still writes the flag but does not record
    /// the binding as definitely initialized, so the guard stays.
    tdz_region_depth: u32,
    /// `let`/`const` bindings whose declarator was emitted in straight-line
    /// code, and which therefore dominate every later reference in this unit.
    ///
    /// This is what keeps the guard off the hot path: a reference that cannot
    /// be in a TDZ costs nothing, so only genuinely order-sensitive reads pay
    /// for the check. It is also what keeps ordinary functions compilable by
    /// the baseline JIT — a guard expands to `JumpIfNullish` plus a
    /// `ReferenceError` construction, none of which that template supports, so
    /// a function that referenced *every* `let` unconditionally could not be
    /// JIT-compiled at all.
    tdz_initialized: HashSet<SymbolId>,
    /// Registers holding the scope objects of the enclosing `with` statements,
    /// innermost last (spec `ObjectEnvironmentRecord` chain). Identifier
    /// *reads* are dynamically probed against these before static resolution.
    ///
    /// The register is reserved for the whole `with` body: `new_temp()` is a
    /// monotonic bump with no free list, so nothing else can clobber it. The
    /// value stack is already a GC root (`gc_protect` extends from `self.stack`),
    /// so no `Frame` field or extra rooting is required.
    pub with_objs: Vec<u16>,
    /// Register holding the value of the last expression statement compiled
    /// so far, when there is one. The main unit emits a `Return` of it so
    /// `eval` gets the spec-compliant script completion value.
    pub last_expr_reg: Option<u16>,
    /// Transient environments currently pushed *on top of* this unit's own
    /// base environment, which the structural model in [`Plans::env_depth`]
    /// cannot see.
    ///
    /// A class scope needs a real environment to hold the parent constructor
    /// for `super`, and `Closure` captures `frame.env`, so that environment
    /// must be the frame's innermost one while the constructor and method
    /// closures are built. While it is current, every statically resolved
    /// access in this unit is physically one hop deeper than `env_depth`
    /// reports, so `access`/`access_location` add this counter.
    ///
    /// [`Self::emit_scoped_env`] and [`Self::emit_pop_env`] are the only
    /// mutators, and they are balanced: the push emits `NewEnvironment` and
    /// increments, the pop emits `PopEnvironment` and decrements, so code
    /// after the construct is back to the structural depths.
    ///
    /// Only *statically resolved* depths are biased. Emissions that pass a
    /// literal depth (the class scope's own super-constructor write, the
    /// per-iteration copy-forward) are relative to the current head and stay
    /// unbiased, which is why they call the raw `emit_get_env`/`emit_set_env`
    /// rather than going through `access`.
    env_bias: u8,
    scope_stack: Vec<ScopeId>,
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
            with_objs: Vec::new(),
            last_expr_reg: None,
            env_bias: 0,
            scope_stack: Vec::new(),
            tdz_region_depth: 0,
            tdz_initialized: HashSet::new(),
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

    pub fn new_temp(&mut self) -> u16 {
        if self.overflow.is_some() {
            return 0;
        }
        let r = self.temp_top;
        let Some(next) = self.temp_top.checked_add(1) else {
            self.overflow = Some(self.overflow_err());
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
    pub fn new_temps(&mut self, n: u16) -> u16 {
        if self.overflow.is_some() {
            return 0;
        }
        let base = self.temp_top;
        let Some(next) = self.temp_top.checked_add(n) else {
            self.overflow = Some(self.overflow_err());
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

    pub fn temp_mark(&self) -> u16 {
        self.temp_top
    }

    pub fn temp_release(&mut self, mark: u16) {
        self.temp_top = mark.min(self.temp_top);
    }

    /// The single capacity error message. Named for the historical bucket:
    /// Test262 negative handling keys off a clean compile failure, and every
    /// saturating counter (registers, env slots, temps) shares it.
    fn overflow_err(&self) -> CompileError {
        CompileError {
            message: "too many functions/constants".into(),
            span: Some((0, 0)),
        }
    }

    fn track(&mut self, reg: u16) -> Result<(), CompileError> {
        #[allow(clippy::absurd_extreme_comparisons)]
        if reg >= MAX_REGS {
            return Err(self.overflow_err());
        }
        self.high_water = self.high_water.max(reg + 1);
        Ok(())
    }

    /// The reserved never-written register; reading it yields `undefined`
    /// (all registers initialize to `undefined` — frame ABI).
    pub fn undef_reg(&self) -> u16 {
        self.comp.plans.units[self.unit].locals_end
    }

    // -- variable access ----------------------------------------------------

    /// Resolve a symbol to ordinary function/global storage.
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
                depth: self.biased_depth(plans.env_depth(self.unit, sym)),
                slot,
            },
            VarLoc::Global => VarAccess::Global { sym },
        }
    }

    /// Resolve a reference at the current lexical scope, preferring the
    /// nearest block-function lexical location and falling back to the
    /// ordinary function-scope route.
    pub fn access_in_scope(&self, sym: SymbolId) -> VarAccess {
        let plans = &self.comp.plans;
        if plans.units[self.unit].block_functions.iter().any(|plan| {
            plan.symbol == sym
                && self.comp.scoping.symbol_name(plan.symbol) == "arguments"
                && plan.skip == BlockFunctionSkip::ParameterCollision
        }) {
            return VarAccess::Global { sym };
        }
        if let Some(scope) = self.scope_stack.last().copied()
            && let Some(location) = plans.units[self.unit]
                .block_functions
                .iter()
                .rev()
                .find(|plan| {
                    plan.symbol == sym
                        && plan.declaration_scope
                            != plans.units[self.unit]
                                .function_scope
                                .unwrap_or(plan.declaration_scope)
                        && self
                            .comp
                            .scoping
                            .scope_is_descendant_of(scope, plan.declaration_scope)
                })
                .and_then(|plan| plan.lexical_location)
        {
            return self.access_location(location);
        }
        self.access(sym)
    }

    pub fn access_location(&self, loc: VarLoc) -> VarAccess {
        match loc {
            VarLoc::Reg(reg) => VarAccess::Reg(reg),
            // A block-function lexical binding lives in the current unit's own
            // environment, which sits `env_bias` hops below the current head
            // while a class scope is pushed in this unit.
            VarLoc::Env(slot) => VarAccess::Env {
                depth: self.biased_depth(0),
                slot,
            },
            VarLoc::Global => {
                let plans = &self.comp.plans;
                let unit = &plans.units[self.unit];
                // Iterate `decl_order` — a `Vec`, in declaration order — rather
                // than `symbol_names`, a `HashMap`. One identifier text has one
                // SymbolId per DECLARING SCOPE, so a hash-order scan can pick a
                // different symbol on each run and stamp the global access with
                // the wrong variable name. Same defect class as the
                // eval-metadata lookup fixed in `unit.rs`: make the choice a
                // function of the program, never of the hash seed.
                let by_decl_order = unit
                    .decl_order
                    .iter()
                    .copied()
                    .find(|sym| unit.vars.get(sym) == Some(&VarLoc::Global));
                // `collect.rs` assigns locations by walking `decl_order`, so it
                // covers every symbol that has a `vars` entry. The minimum-id
                // scan is a completeness backstop for anything added by another
                // pass, and is deterministic by construction.
                let sym = by_decl_order.or_else(|| {
                    plans
                        .symbol_names
                        .keys()
                        .copied()
                        .filter(|sym| unit.vars.get(sym) == Some(&VarLoc::Global))
                        .min()
                });
                VarAccess::Global {
                    sym: sym.expect("global block-function location has a symbol"),
                }
            }
        }
    }

    // -- TDZ (temporal dead zone) ------------------------------------------

    /// Resolves a binding's TDZ flag to a runtime access, or `None` when the
    /// binding is never in a TDZ.
    ///
    /// The flag lives in the *home* unit, exactly like the value it guards, so
    /// the hop count is the one [`FnCtx::access`] already computes. A
    /// register-local flag is only ever reachable from its own unit — which is
    /// correct, because a register-local binding is by definition never
    /// captured by an inner function.
    fn tdz_flag_access(&self, sym: SymbolId) -> Option<VarAccess> {
        let plans = &self.comp.plans;
        let home = *plans.home_of.get(&sym)?;
        let loc = plans.units[home].tdz_flag_of(sym)?;
        Some(match loc {
            VarLoc::Reg(r) => VarAccess::Reg(r),
            VarLoc::Env(slot) => VarAccess::Env {
                depth: self.biased_depth(plans.env_depth(self.unit, sym)),
                slot,
            },
            // No `let`/`const` binding aliases the global object: top-level
            // `var`/`function` do, and those are never TDZ-guarded.
            VarLoc::Global => return None,
        })
    }

    /// Emits the `ReferenceError` throw for a reference to `sym` made while
    /// its binding is still uninitialized. `JumpIfNullish` on the flag is the
    /// whole test: `undefined` means "not yet initialized", anything else
    /// means initialized (the marker written by [`FnCtx::tdz_init`] is `1`).
    fn emit_tdz_throw(&mut self, sym: SymbolId, span: oxc_span::Span) -> Res<()> {
        let name = self.comp.scoping.symbol_name(sym);
        // `new ReferenceError(msg)`: [callee][this][msg] per the `Call` ABI.
        let block = self.new_temps(3);
        // The intrinsic table is consulted before the global object's own
        // properties (`resolve_global`), so a user-shadowed `ReferenceError`
        // global cannot disarm the check.
        let ctor = self.global_name_id("ReferenceError");
        self.emit_get_global(block, ctor, span);
        self.move_reg(block + 1, self.undef_reg(), span);
        let msg = format!("Cannot access '{name}' before initialization");
        self.load_str(block + 2, &msg, span)?;
        let err = self.new_temp();
        self.emit_call(err, block, 1, span);
        self.emit_reg3(Opcode::Throw, err, 0, 0, span);
        Ok(())
    }

    /// Emits the TDZ *test* for a reference to `sym`, returning the label the
    /// uninitialized case lands on. `None` when the binding can never be in a
    /// TDZ and no check is needed.
    ///
    /// Split in two halves because the throw cannot sit between the test and
    /// the reference itself: the guarded code (a load or a store) has to fall
    /// through from the test and *then* jump past the `ReferenceError`, which
    /// is only expressible if the throw is emitted after that code. Callers
    /// emit their one-or-two instructions and then call
    /// [`FnCtx::tdz_guard_end`]:
    ///
    /// ```text
    ///     read flag -> f
    ///     JumpIfNullish f -> uninit      ; still undefined => dead
    ///     <the reference>                ; fallthrough: binding is initialized
    ///     Jump cont
    /// uninit:
    ///     new ReferenceError(...); Throw
    /// cont:
    /// ```
    pub(crate) fn tdz_test(&mut self, sym: SymbolId, span: oxc_span::Span) -> Res<Option<Label>> {
        // Dominance shortcut: straight-line code that already initialized this
        // binding cannot reach a TDZ, whatever the reference looks like.
        if self.tdz_initialized.contains(&sym) {
            return Ok(None);
        }
        let Some(access) = self.tdz_flag_access(sym) else {
            return Ok(None);
        };
        // `undefined` *is* the uninitialized state: registers start
        // `undefined` per the frame ABI and `JsObject::environment` fills fresh
        // slots with `undefined`, so no sentinel needs installing.
        let flag = self.new_temp();
        self.read_access(access, flag, span);
        let uninit = self.label();
        self.emit_jump(Opcode::JumpIfNullish, flag, uninit);
        Ok(Some(uninit))
    }

    /// Closes a [`FnCtx::tdz_test`]: jumps past the throw, emits it on the
    /// uninitialized path, and leaves `pc` on the continuation.
    pub(crate) fn tdz_guard_end(
        &mut self,
        sym: SymbolId,
        uninit: Label,
        span: oxc_span::Span,
    ) -> Res<()> {
        let cont = self.label();
        self.emit_jump(Opcode::Jump, 0, cont);
        self.bind(uninit);
        self.emit_tdz_throw(sym, span)?;
        self.bind(cont);
        Ok(())
    }

    /// Marks `sym`'s binding initialized, ending its TDZ.
    ///
    /// Called once per declarator *after* the whole initializer expression has
    /// evaluated — that ordering is the spec's (ES §8.5.2 `BindingInitialization`
    /// evaluates the initializer first, then creates the binding), and it is
    /// what makes `let x = x` a TDZ *read* of the new binding rather than a
    /// self-assignment.
    pub(crate) fn tdz_init(&mut self, sym: SymbolId, span: oxc_span::Span) -> Res<()> {
        let Some(access) = self.tdz_flag_access(sym) else {
            return Ok(());
        };
        let one = self.new_temp();
        self.load_int(one, 1, span);
        self.store_access(access, one, span);
        // The flag write is unconditional — a closure created here may run the
        // read long after this unit finished. Only the dominance *record* is
        // conditional, and only for straight-line declarators (see
        // `FnCtx::tdz_region_depth`).
        if self.tdz_region_depth == 0 {
            self.tdz_initialized.insert(sym);
        }
        Ok(())
    }

    /// Marks the start of a region whose statements are not guaranteed to run
    /// (or not guaranteed to run before everything after them). Pairs with
    /// [`FnCtx::exit_tdz_region`].
    pub fn enter_tdz_region(&mut self) {
        self.tdz_region_depth += 1;
    }

    /// Runs `body` inside a non-straight-line region: nothing declared while
    /// it runs is recorded as dominating anything after it (see
    /// [`FnCtx::tdz_region_depth`]).
    pub fn tdz_region<T>(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<T, CompileError>,
    ) -> Result<T, CompileError> {
        self.enter_tdz_region();
        let out = body(self);
        self.exit_tdz_region();
        out
    }

    /// Marks the end of a region opened by [`FnCtx::enter_tdz_region`].
    ///
    /// A `CompileError` raised while a region is open aborts the whole unit,
    /// so the depth is only unwound on the success path.
    pub fn exit_tdz_region(&mut self) {
        debug_assert!(self.tdz_region_depth > 0, "unbalanced TDZ region");
        self.tdz_region_depth = self.tdz_region_depth.saturating_sub(1);
    }

    /// Reads a lexical binding by symbol into a caller-chosen register: TDZ
    /// guard first, then the load.
    ///
    /// Every plain identifier read routes through here (and therefore through
    /// `typeof`, `o.x` receivers, destructuring defaults, …), which is what
    /// makes the guard a property of *references* rather than of the
    /// syntactic form that contains them.
    pub fn read_binding_into(&mut self, sym: SymbolId, dst: u16, span: oxc_span::Span) -> Res<()> {
        let uninit = self.tdz_test(sym, span)?;
        self.read_access(self.access_in_scope(sym), dst, span);
        if let Some(uninit) = uninit {
            self.tdz_guard_end(sym, uninit, span)?;
        }
        Ok(())
    }

    /// Stores into a lexical binding that is being *reassigned* (`x = v`).
    ///
    /// ES §9.1.1 step 2 throws on a reference to an uninitialized binding for
    /// `PutValue` as well as `GetValue`, so the guard precedes the store.
    /// Callers that already emitted [`FnCtx::read_binding`] for the same
    /// symbol (compound assignment `x += v`, `x++`) must use
    /// [`FnCtx::store_access`] directly: reaching the store already proves
    /// the binding was initialized.
    pub fn store_binding_reassign(
        &mut self,
        sym: SymbolId,
        src: u16,
        span: oxc_span::Span,
    ) -> Res<()> {
        let uninit = self.tdz_test(sym, span)?;
        self.store_access(self.access_in_scope(sym), src, span);
        if let Some(uninit) = uninit {
            self.tdz_guard_end(sym, uninit, span)?;
        }
        Ok(())
    }

    pub fn push_scope(&mut self, scope: Option<ScopeId>) {
        if let Some(scope) = scope {
            self.scope_stack.push(scope);
        }
    }

    pub fn current_scope(&self) -> Option<ScopeId> {
        self.scope_stack.last().copied()
    }

    pub fn pop_scope(&mut self, scope: Option<ScopeId>) {
        if scope.is_some() {
            self.scope_stack.pop();
        }
    }

    // -- primitive emissions -------------------------------------------------

    /// Emits a pre-built instruction word. Narrow-only: registers above u8
    /// must go through the `emit_reg*` helpers, which add the `RegExt`
    /// prefix. Kept for direct single-word emissions that carry no register
    /// operands (e.g. `LoopHeader`).
    #[allow(dead_code)] // narrow escape hatch; most emission routes through helpers now
    pub fn emit(&mut self, instr: Instr) {
        self.b.emit(instr);
    }

    /// Emits `op a, b, c` where **all three slots are registers**; falls back
    /// to a `WideOp::RegExt` prefix when a register exceeds the narrow 8-bit
    /// slot. Never use for ops whose slots carry immediates.
    pub fn emit_reg3(&mut self, op: Opcode, a: u16, b: u16, c: u16, span: oxc_span::Span) {
        self.emit_regs(op, a, b, c, 0b0111, span);
    }

    /// Two register slots (`c` stays 0 and unextended).
    pub fn emit_reg2(&mut self, op: Opcode, a: u16, b: u16, span: oxc_span::Span) {
        self.emit_regs(op, a, b, 0, 0b0011, span);
    }

    /// One register slot in `a`.
    pub fn emit_reg1(&mut self, op: Opcode, a: u16, span: oxc_span::Span) {
        self.emit_regs(op, a, 0, 0, 0b0001, span);
    }

    /// Two register slots plus an 8-bit immediate in `c` (never extended).
    pub fn emit_reg2_imm8(&mut self, op: Opcode, a: u16, b: u16, imm8: u8, span: oxc_span::Span) {
        self.emit_regs(op, a, b, u16::from(imm8), 0b0011, span);
    }

    /// The narrow/wide funnel for register operands.
    ///
    /// `reg_mask` bit 0/1/2 marks slots `a`/`b`/`c` as registers, so only
    /// those may receive a `RegExt` high byte; immediate slots (e.g. `c`
    /// holding an argc or array length) must fit u8 and stay unmasked.
    fn emit_regs(
        &mut self,
        op: Opcode,
        a: u16,
        b: u16,
        c: u16,
        reg_mask: u8,
        span: oxc_span::Span,
    ) {
        let fits_narrow = |v: u16| v <= u16::from(u8::MAX);
        if fits_narrow(a) && fits_narrow(b) && fits_narrow(c) {
            self.emit_spanned(Instr::new(op, a as u8, b as u8, c as u8), span);
            return;
        }
        debug_assert!(
            (fits_narrow(a) || reg_mask & 1 != 0)
                && (fits_narrow(b) || reg_mask & 2 != 0)
                && (fits_narrow(c) || reg_mask & 4 != 0),
            "only register slots may be RegExt-extended"
        );
        let words = WideOp::RegExt {
            mask: reg_mask,
            a_hi: (a >> 8) as u8,
            b_hi: (b >> 8) as u8,
            c_hi: (c >> 8) as u8,
        }
        .encode();
        self.emit_words(words, span);
        self.emit_spanned(Instr::new(op, a as u8, b as u8, c as u8), span);
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

    /// Pushes a [`LoopCtx`] (or labeled-statement break target) for the
    /// scope of `f`, popping it afterwards — including on the error path,
    /// where a `CompileError` aborts the whole unit anyway. `break` lands at
    /// `break_label`; `continue`, when the construct has a continuation
    /// point, at `continue_label`.
    ///
    /// `per_iter_env` records that the body runs inside a per-iteration
    /// environment (see [`LoopCtx::per_iter_env`]); it must be the same value
    /// the loop's push was emitted under, and `f` is responsible for emitting
    /// the matching `emit_env_pop` on the body's fallthrough.
    pub fn with_loop<R>(
        &mut self,
        break_label: Label,
        continue_label: Option<Label>,
        name: Option<String>,
        close_iter: Option<u16>,
        per_iter_env: bool,
        f: impl FnOnce(&mut Self) -> Result<R, CompileError>,
    ) -> Result<R, CompileError> {
        self.loops.push(LoopCtx {
            break_label,
            continue_label,
            name,
            finally_base: self.finallies.len(),
            close_iter,
            close_guard: None,
            per_iter_env,
        });
        // The body is the one non-straight-line region every loop shares, and
        // it is entered *after* the head: a `for (let i = 0; ...)` declarator
        // runs on every path into the body, so it still dominates references
        // there (which is why the region starts here rather than around the
        // whole loop lowering in the statement dispatcher).
        self.enter_tdz_region();
        let out = f(self);
        self.exit_tdz_region();
        self.loops.pop();
        out
    }

    /// Like [`Self::with_loop`] but for a `for-of` loop that carries an
    /// iterator-close guard register (see [`LoopCtx::close_guard`]).
    // The for-of extras (iterator, guard, per-iteration flag) have already
    // taken this past clippy's argument budget; splitting them into a context
    // struct would be churn for one caller.
    #[allow(clippy::too_many_arguments)]
    pub fn with_for_of_loop<R>(
        &mut self,
        break_label: Label,
        continue_label: Option<Label>,
        name: Option<String>,
        close_iter: u16,
        close_guard: u16,
        per_iter_env: bool,
        f: impl FnOnce(&mut Self) -> Result<R, CompileError>,
    ) -> Result<R, CompileError> {
        self.loops.push(LoopCtx {
            break_label,
            continue_label,
            name,
            finally_base: self.finallies.len(),
            close_iter: Some(close_iter),
            close_guard: Some(close_guard),
            per_iter_env,
        });
        // Same body region as `with_loop` (see the note there).
        self.enter_tdz_region();
        let out = f(self);
        self.exit_tdz_region();
        self.loops.pop();
        out
    }

    /// Emits one `PopEnvironment` per per-iteration environment belonging to a
    /// loop being exited (`self.loops[until..]`, innermost first) — the
    /// abrupt-exit half of the balance that the body's fallthrough pop (see
    /// [`Self::emit_env_pop`]) provides for normal completion.
    ///
    /// Without this the runtime chain grows one hop per *runtime* iteration,
    /// which no compile-time depth bias can model: the loop environment's
    /// parent is the previous iteration's loop environment, so the chain depth
    /// is only known while running. `break`/`continue` jumping with the
    /// environment still live would leave it dangling for the rest of the
    /// frame.
    ///
    /// The pops deliberately do **not** touch `env_bias`: this runs at the
    /// `break`/`continue` site, where the code emitted afterwards is still
    /// (unreachable) lexically inside the body and must keep resolving against
    /// the loop environment.
    pub fn emit_per_iteration_pops(&mut self, until: usize) {
        let pops = self.loops[until..]
            .iter()
            .rev()
            .filter(|ctx| ctx.per_iter_env)
            .count();
        for _ in 0..pops {
            self.emit_env_pop(oxc_span::Span::default());
        }
    }

    /// Emits `IteratorClose r{iter}` for every loop being exited, innermost
    /// first (`for-of` abrupt completion; spec 14.7.4.9). `until` is the
    /// loop-stack index of the break target: loops above it are exited.
    /// `rb` is 1 (normal completion): the `return()` result is validated
    /// as an Object (spec 7.4.6). The throw-path handler in `stmt.rs`
    /// emits `rb` 0 instead (no validation — the original error wins).
    pub fn emit_iterator_closes(&mut self, until: usize) {
        let iters: Vec<(u16, Option<u16>)> = self.loops[until..]
            .iter()
            .rev()
            .filter_map(|ctx| ctx.close_iter.map(|it| (it, ctx.close_guard)))
            .collect();
        for (iter, guard) in iters {
            // Arm the loop's close guard *before* the `return()` call: that
            // call sits inside the loop's exception range, so a throwing
            // close re-enters the handler, which must not close a second time
            // (spec 7.4.6 — the close error is the completion).
            if let Some(g) = guard {
                self.load_bool(g, true, oxc_span::Span::default());
            }
            self.emit_reg3(Opcode::IteratorClose, iter, 1, 0, oxc_span::Span::default());
        }
    }

    /// Emits inline copies of every active finalizer (innermost first) while
    /// *keeping* the compile-time finally stack: the generator return path
    /// after a yield may also be fallen through by a normal resume, which
    /// must still see those finallies as active. Mirrors the emission shape
    /// of [`Self::run_finally_copies`] with a restored stack.
    pub fn emit_yield_return_finally_copies(&mut self) -> Result<(), CompileError> {
        let bodies: Vec<&'a BlockStatement<'a>> = self.finallies.iter().map(|c| c.body).collect();
        for body in bodies.into_iter().rev() {
            let saved = std::mem::take(&mut self.finallies);
            let r = self.stmt_list(&body.body);
            self.finallies = saved;
            r?;
        }
        Ok(())
    }

    /// `src === undefined ? default : src` — the ES destructuring-default
    /// semantics. Evaluates `default` into a fresh temp only when `src` is
    /// `undefined`; returns the register holding the chosen value.
    ///
    /// `name` carries the spec `NamedEvaluation` binding identifier for a
    /// destructuring default (`[f = function(){}]`); when present and
    /// `default` is a syntactically anonymous function definition, the
    /// planned unit is named after it.
    pub fn lower_default(
        &mut self,
        src: u16,
        default: &oxc_ast::ast::Expression<'_>,
        span: Span,
        name: Option<&str>,
    ) -> Result<u16, CompileError> {
        let chosen = self.new_temp();
        let undef = self.new_temp();
        self.load_undefined(undef, span);
        let cond = self.new_temp();
        self.emit_reg3(Opcode::StrictEq, cond, src, undef, span);
        let use_src = self.label();
        let end = self.label();
        self.emit_jump(Opcode::JumpIfFalse, cond, use_src);
        if let Some(n) = name {
            crate::class::apply_function_name(self, n, default);
        }
        let def = self.expr(default)?;
        self.move_reg(chosen, def, span);
        self.emit_jump(Opcode::Jump, 0, end);
        self.bind(use_src);
        self.move_reg(chosen, src, span);
        self.bind(end);
        Ok(chosen)
    }

    pub fn pc(&self) -> u32 {
        self.b.pc()
    }

    /// Emits a branch to `target`, with a `RegExt` prefix when the condition
    /// register exceeds the narrow 8-bit slot. `Jump` ignores `cond`.
    pub fn emit_jump(&mut self, op: Opcode, cond: u16, target: Label) {
        if cond <= u16::from(u8::MAX) {
            self.b.emit_jump(op, cond as u8, target);
            return;
        }
        // The prefix is emitted first so the builder's fixup still points at
        // the branch word itself.
        self.emit_words(
            WideOp::RegExt {
                mask: 0b0001,
                a_hi: (cond >> 8) as u8,
                b_hi: 0,
                c_hi: 0,
            }
            .encode(),
            oxc_span::Span::default(),
        );
        self.b.emit_jump(op, cond as u8, target);
    }

    pub fn add_const(&mut self, c: Const) -> Result<u16, CompileError> {
        self.b.add_const(c).map_err(|e| CompileError {
            message: e.to_string(),
            span: None,
        })
    }

    /// Loads a pooled constant into `dst` (16-bit const ids always fit).
    pub fn load_const(
        &mut self,
        dst: u16,
        konst: Const,
        span: oxc_span::Span,
    ) -> Result<(), CompileError> {
        let k = self.add_const(konst)?;
        self.emit_regs(Opcode::LoadConst, dst, k >> 8, k & 0xFF, 0b0001, span);
        Ok(())
    }

    /// Loads a string literal (interned) into `dst` via `ConstantPool`.
    ///
    /// The `Rodeo` interns `s` to a `Spur`; `str_id_of` converts it to a
    /// `Str32` payload, which is pooled and whose pool index becomes the
    /// `LoadConst` immediate. This is distinct from `GetGlobal`'s
    /// `PropKey` (Spur) immediate — see `Interner` docs.
    pub fn load_str(
        &mut self,
        dst: u16,
        s: &str,
        span: oxc_span::Span,
    ) -> Result<(), CompileError> {
        let id = str_id_of(self.comp.strings.get_or_intern(s));
        self.load_const(dst, Const::Str32(id), span)
    }

    /// Loads the interned text `s` into a fresh register, returning it.
    pub fn load_str_key(&mut self, s: &str, span: oxc_span::Span) -> Result<u16, CompileError> {
        let d = self.new_temp();
        self.load_str(d, s, span)?;
        Ok(d)
    }

    /// Loads an i64 as narrowly as the encoding allows.
    pub fn load_int(&mut self, dst: u16, v: i64, span: oxc_span::Span) {
        if (-128..=127).contains(&v) && dst <= u16::from(u8::MAX) {
            self.emit_spanned(
                Instr::new(Opcode::LoadInt, dst as u8, 0, v as i8 as u8),
                span,
            );
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
    pub fn load_undefined(&mut self, dst: u16, span: oxc_span::Span) {
        let src = self.undef_reg();
        self.move_reg(dst, src, span);
    }

    /// Boolean literals: comparisons yield booleans, so `0 == 0` / `0 != 0`
    /// materialize `true` / `false` without a boolean constant kind.
    pub fn load_bool(&mut self, dst: u16, v: bool, span: oxc_span::Span) {
        let t = self.new_temp();
        self.load_int(t, 0, span);
        let op = if v { Opcode::Eq } else { Opcode::Ne };
        self.emit_reg3(op, dst, t, t, span);
    }

    pub fn move_reg(&mut self, dst: u16, src: u16, span: oxc_span::Span) {
        self.emit_reg2(Opcode::Move, dst, src, span);
    }

    /// `GetEnvSlot` with wide fallback once the slot (or destination)
    /// exceeds the narrow 8-bit operand.
    pub fn emit_get_env(&mut self, dst: u16, depth: u8, slot: u16, span: oxc_span::Span) {
        if dst <= u16::from(u8::MAX) && slot <= u16::from(u8::MAX) {
            self.emit_spanned(
                Instr::new(Opcode::GetEnvSlot, dst as u8, depth, slot as u8),
                span,
            );
        } else {
            let words = WideOp::GetEnvSlotW {
                dst,
                depth: u16::from(depth),
                slot,
            }
            .encode();
            self.emit_words(words, span);
        }
    }

    pub fn emit_set_env(&mut self, depth: u8, slot: u16, src: u16, span: oxc_span::Span) {
        if src <= u16::from(u8::MAX) && slot <= u16::from(u8::MAX) {
            self.emit_spanned(
                Instr::new(Opcode::SetEnvSlot, depth, slot as u8, src as u8),
                span,
            );
        } else {
            let words = WideOp::SetEnvSlotW {
                src,
                depth: u16::from(depth),
                slot,
            }
            .encode();
            self.emit_words(words, span);
        }
    }

    /// `GetGlobal dst, name_id` where `name_id` is the `Spur`'s
    /// Interns a global name and returns its string-table id — the immediate
    /// accepted by [`Self::emit_get_global`]/[`Self::emit_set_global`]. Same
    /// id namespace as [`str_id_of`] (a direct string table index, not a
    /// `LoadConst` pool index); see `Interner` docs.
    pub fn global_name_id(&mut self, name: &str) -> u32 {
        str_id_of(self.comp.strings.get_or_intern(name))
    }

    /// `GetGlobal dst, name_id` — the `Spur`-derived string table id, not a
    /// pool index — distinct from `LoadConst`'s `Str32` pool immediate. See
    /// `Interner` docs.
    // Global-name table ids fit u16 in this subset; audited invariant.
    #[allow(clippy::expect_used)]
    pub fn emit_get_global(&mut self, dst: u16, name_id: u32, span: oxc_span::Span) {
        let k = u16::try_from(name_id).expect("global name id fits u16");
        self.emit_regs(Opcode::GetGlobal, dst, k >> 8, k & 0xFF, 0b0001, span);
    }

    /// `GetGlobalLenient dst, name_id` — a global read that never throws:
    /// missing bindings yield `undefined` (spec `typeof` semantics).
    pub fn emit_get_global_lenient(&mut self, dst: u16, name_id: u32, span: oxc_span::Span) {
        let k = u16::try_from(name_id).expect("global name id fits u16");
        self.emit_regs(
            Opcode::GetGlobalLenient,
            dst,
            k >> 8,
            k & 0xFF,
            0b0001,
            span,
        );
    }

    /// `RequireObjectCoercible` guard for destructuring: a dummy
    /// `GetProperty` read that throws `TypeError` on `null`/`undefined`
    /// sources (via the null guard in the interpreter's `get_property`)
    /// and is otherwise a harmless miss discarded into a fresh temp. Only
    /// needed when a pattern would emit zero reads (empty `{}`/`[]` with
    /// no rest); non-empty patterns throw through their own first read.
    pub fn emit_require_object_coercible(
        &mut self,
        src: u16,
        span: oxc_span::Span,
    ) -> Result<(), CompileError> {
        let key = self.new_temp();
        self.load_str(key, "__coercible__", span)?;
        let tmp = self.new_temp();
        self.emit_reg3(Opcode::GetProperty, tmp, src, key, span);
        Ok(())
    }

    /// Emits the dynamic `with`-scope probe for the identifier `name`.
    ///
    /// For each enclosing `with` object (innermost first — the spec
    /// `ObjectEnvironmentRecord` binding chain) emits
    /// `HasProperty(obj, name) ? obj[name] : <next>`, branching to the returned
    /// label on the first hit. When no enclosing object has the binding the
    /// caller emits static resolution and then binds that label, so the probe
    /// costs zero instructions outside any `with`.
    ///
    /// `HasProperty` is the `In` operator, which walks the prototype chain.
    /// `store` selects `SetProperty` over `GetProperty`; `dst` is the read
    /// destination.
    ///
    /// Limitations (documented, outside the target tests): `In` throws
    /// `TypeError` on a primitive right-hand side, so `with` over a primitive
    /// throws instead of boxing it; and a function *declared* inside a `with`
    /// body compiles in a fresh `FnCtx` that sees no `with` objects, so its
    /// identifiers resolve statically.
    pub fn emit_with_probe(
        &mut self,
        name: &str,
        dst: u16,
        store: Option<u16>,
        span: Span,
    ) -> Result<Option<Label>, CompileError> {
        if self.with_objs.is_empty() {
            return Ok(None);
        }
        let key = self.load_str_key(name, span)?;
        let end = self.label();
        for idx in (0..self.with_objs.len()).rev() {
            let obj = self.with_objs[idx];
            let hit = self.new_temp();
            self.emit_reg3(Opcode::In, hit, key, obj, span);
            let miss = self.label();
            self.emit_jump(Opcode::JumpIfFalse, hit, miss);
            match store {
                Some(v) => self.emit_reg3(Opcode::SetProperty, obj, key, v, span),
                None => self.emit_reg3(Opcode::GetProperty, dst, obj, key, span),
            }
            self.emit_jump(Opcode::Jump, 0, end);
            self.bind(miss);
        }
        Ok(Some(end))
    }

    /// Observable binding-resolution probe: emits one `In` per enclosing `with`
    /// scope object, innermost first, so a proxy `has` trap can observe
    /// `ResolveBinding`. The boolean result is deliberately DISCARDED — lane 4's
    /// documented YAGNI boundary still applies, i.e. a *hit* still resolves
    /// statically. Mirrors spec 9.1.2.1 GetIdentifierReference step 2.
    pub fn emit_with_binding_probe(&mut self, name: &str, span: Span) -> Result<(), CompileError> {
        if self.with_objs.is_empty() {
            return Ok(());
        }
        let key = self.load_str_key(name, span)?;
        for idx in (0..self.with_objs.len()).rev() {
            let obj = self.with_objs[idx];
            let probe = self.new_temp();
            self.emit_reg3(Opcode::In, probe, key, obj, span);
        }
        Ok(())
    }

    /// `SetGlobal name_id, src` — same `Spur`-derived string table id as
    /// `GetGlobal`.
    // Global-name table ids fit u16 in this subset; audited invariant.
    #[allow(clippy::expect_used)]
    pub fn emit_set_global(&mut self, name_id: u32, src: u16, span: oxc_span::Span) {
        let k = u16::try_from(name_id).expect("global name id fits u16");
        self.emit_regs(Opcode::SetGlobal, src, k >> 8, k & 0xFF, 0b0001, span);
    }

    /// Strict-mode `SetGlobal`: a throwing `GetGlobal` existence check
    /// (`ReferenceError` on an unresolvable identifier) before the store, so
    /// strict assignment to an undeclared name throws instead of creating a
    /// global. Sloppy mode keeps the bare `SetGlobal`. Callers evaluate the
    /// RHS first, so the check runs after it, matching spec order.
    pub fn emit_set_global_strict(&mut self, name_id: u32, src: u16, span: oxc_span::Span) {
        let probe = self.new_temp();
        self.emit_get_global(probe, name_id, span);
        self.emit_set_global(name_id, src, span);
    }

    /// `Call` with the documented layout; wide encodings for large arities
    /// (`CallW`) and wide registers (`RegExt`).
    pub fn emit_call(&mut self, dst: u16, callee: u16, argc: u16, span: oxc_span::Span) {
        if argc > u16::from(u8::MAX) {
            let words = WideOp::CallW {
                dst,
                func: callee,
                argc,
            }
            .encode();
            self.emit_words(words, span);
            return;
        }
        // argc rides in slot `c`, which stays unmasked.
        self.emit_regs(
            Opcode::Call,
            dst,
            callee,
            u16::from(argc as u8),
            0b0011,
            span,
        );
    }

    /// `Construct` with the same layout as `emit_call`; wide escapes for
    /// large arities (`ConstructW`) and wide registers (`RegExt`).
    pub fn emit_construct(&mut self, dst: u16, callee: u16, argc: u16, span: oxc_span::Span) {
        if argc > u16::from(u8::MAX) {
            let words = WideOp::ConstructW {
                dst,
                func: callee,
                argc,
            }
            .encode();
            self.emit_words(words, span);
            return;
        }
        self.emit_regs(
            Opcode::Construct,
            dst,
            callee,
            u16::from(argc as u8),
            0b0011,
            span,
        );
    }

    /// `NewArray dst, base, len` — `len` is an immediate and must fit the
    /// narrow slot (array literals above 255 elements are rejected by the
    /// caller); `dst`/`base` may escape through `RegExt`.
    pub fn emit_new_array(&mut self, dst: u16, base: u16, len: u8, span: oxc_span::Span) {
        self.emit_regs(Opcode::NewArray, dst, base, u16::from(len), 0b0011, span);
    }

    /// `NewEnvironment depth, slots` with a `NewEnvironmentW` escape once
    /// the slot count exceeds the narrow 8-bit operand.
    pub fn emit_new_env(&mut self, depth: u8, slots: u16, span: oxc_span::Span) {
        if slots <= u16::from(u8::MAX) {
            self.emit_spanned(
                Instr::new(Opcode::NewEnvironment, depth, slots as u8, 0),
                span,
            );
        } else {
            let words = WideOp::NewEnvironmentW {
                depth: u16::from(depth),
                slots,
            }
            .encode();
            self.emit_words(words, span);
        }
    }

    /// A *scoped* environment: pushed on top of this unit's own environment for
    /// the duration of a construct, and retired again by
    /// [`Self::emit_pop_env`].
    ///
    /// The unit's base environment (the prologue) is not scoped and does not go
    /// through here — it *is* the origin every structural depth counts from.
    pub fn emit_scoped_env(&mut self, depth: u8, slots: u16, span: oxc_span::Span) {
        self.emit_new_env(depth, slots, span);
        self.env_bias += 1;
    }

    /// Retires the environment [`Self::emit_scoped_env`] pushed, so the unit is
    /// back to its structural env depths. Balanced by construction: emitting
    /// this without a matching `emit_scoped_env` would underflow the bias.
    pub fn emit_pop_env(&mut self, span: oxc_span::Span) {
        self.env_bias = self.env_bias.saturating_sub(1);
        self.emit_spanned(Instr::new(Opcode::PopEnvironment, 0, 0, 0), span);
    }

    /// Retires an environment that is *not* a biased scope: the frame's
    /// innermost environment is retired without touching `env_bias`.
    ///
    /// This is the pop half of a per-iteration environment (see
    /// [`crate::FnCtx::emit_per_iteration_env`]), which unlike a class scope
    /// replaces the unit's *own* environment — same slot count, same slot
    /// indices — rather than shadowing it, so both the push and the pop leave
    /// `env_bias` alone.
    pub fn emit_env_pop(&mut self, span: oxc_span::Span) {
        self.emit_spanned(Instr::new(Opcode::PopEnvironment, 0, 0, 0), span);
    }

    /// The depth a statically resolved access must be emitted with: the
    /// structural hop count plus every scoped environment currently on the
    /// chain. Saturating because the structural count is already a `u8` budget.
    fn biased_depth(&self, depth: u8) -> u8 {
        depth.saturating_add(self.env_bias)
    }

    /// `Closure dst, fn#idx` — escapes to `ClosureW` once the function
    /// index or destination exceeds the narrow 8-bit operands.
    pub fn emit_closure(&mut self, dst: u16, function_index: u16, span: oxc_span::Span) {
        if dst <= u16::from(u8::MAX) && function_index <= u16::from(u8::MAX) {
            self.emit_spanned(
                Instr::new(Opcode::Closure, dst as u8, function_index as u8, 0),
                span,
            );
        } else {
            let words = WideOp::ClosureW {
                dst,
                function_index,
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
            regs = u16::try_from(self.handler_max).unwrap_or(MAX_REGS);
        }
        self.b.reserve_regs(regs);
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
    Reg(u16),
    Env {
        depth: u8,
        slot: u16,
    },
    /// A global property (`GetGlobal`/`SetGlobal`) — the symbol's name is
    /// interned at emission time.
    Global {
        sym: SymbolId,
    },
}
