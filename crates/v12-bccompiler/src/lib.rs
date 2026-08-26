//! Compiles the oxc AST (with oxc_semantic scope analysis) into v12 bytecode.
//!
//! Pipeline: `oxc_parser` → oxc AST → `oxc_semantic` scopes/symbols
//! → this crate walks the AST directly and emits [`v12_bytecode`] functions.
//! Identifier identity, hoisting, and shadowing all come from oxc's
//! resolution (`BindingIdentifier::symbol_id`,
//! `IdentifierReference::reference_id`); this crate adds only capture
//! analysis and register/environment layout.
//!
//! # Tier coverage
//!
//! - **Tier 1** — literals (numeric/string/bool), `let`/`const`/`var` with
//!   block scoping via registers, arithmetic / logical (`&&`, `||`, `??`) /
//!   bitwise / comparison / strict-equality / `in` / `instanceof` operators,
//!   `typeof`, unary `-`/`!`/`~`, `++`/`--` (prefix & postfix), assignment to
//!   locals, properties, and object members; expression statements; `if`/`else`;
//!   `while`/`do-while`/`for(;;)`; labeled `break`/`continue`; `return`;
//!   function declarations/expressions/arrows; plain + method calls;
//!   property get/set by identifier/string keys; object/array literals;
//!   string concatenation via `+`; `this`; `throw`/`try`/`catch`/`finally`
//!   via handler tables with duplicated-completion dispatch; nested
//!   closures through static capture analysis (`NewEnvironment`,
//!   `GetEnvSlot`, `SetEnvSlot`).
//! - **Tier 2** — conditional `?:`, basic flat destructuring declarations
//!   (`let {a, b} = …`, `let [a, b] = …` lowered to property loads).
//!
//! # Deviations / subset limits (all fail loudly as `CompileError`s)
//!
//! - `new Expr()` — no construct opcode exists in the ISA; rejected rather
//!   than mis-encoded (documented ISA gap).
//! - `null` literals — the constant pool has no null kind yet.
//! - Unary `+` (no ToNumber opcode), spread, optional
//!   chaining/calls, computed property keys, getters/setters/method
//!   shorthand in object literals, BigInt/RegExp literals, template literals
//!   with substitutions, classes, generators/async, `for-in`/`for-of`,
//!   switch/with, `arguments`, `eval`.
//! - Unbound identifier reads are errors (no global object yet), except
//!   `typeof undeclared` which correctly yields `"undefined"`.
//! - Calls use the documented ABI: callee at `callee_reg`, `this` at
//!   `callee_reg+1`, args from `callee_reg+2`; the callee window starts at
//!   `callee_reg+1` so no argument copying is needed.
//! - Captured variables live one Environment per function unit (block-level
//!   captures are hoisted into it): closures over a loop-body `let` observe
//!   the final value, not per-iteration bindings. TDZ is not modeled.
//! - `??` treats only `undefined` as nullish (`null` values cannot be
//!   produced in-subset yet).
//! - Function-declaration hoisting covers direct statement-list items only;
//!   function declarations bind lexically (strict-mode style) rather than
//!   Annex B web-compat semantics.
//!
//! # Stretch items not reached (TODO)
//!
//! Generator functions (`CreateGenerator`/`SuspendYield` lowering) and
//! async/await desugaring to `Await` remain unimplemented; both need the
//! interpreter's pausable-frame execution model to be testable.

#![forbid(unsafe_code)]

mod collect;
mod expr;
mod model;
mod peephole;
mod stmt;
#[cfg(test)]
mod tests;
mod unit;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::Scoping;
use oxc_span::{GetSpan, SourceType};
use v12_bytecode::FunctionBytecode;

pub use model::{
    CompileError, ExportEntry, ImportEntry, Interner, NATIVE_IMPORT_INDEX, NATIVE_IMPORT_INDEX_U32,
};

/// Compiled program: every function body plus the entry point.
///
/// `main` indexes the top-level script body in [`Program::functions`];
/// `Closure` instructions reference other entries of that same vector.
#[derive(Debug, Clone)]
pub struct Program {
    pub functions: Vec<FunctionBytecode>,
    pub main: u32,
}

/// Compiled ES module: bytecode plus linkage metadata.
///
/// `program` is the executable bytecode (same shape as [`Program`] for a
/// script). `imports` and `exports` are the module linkage tables populated
/// from `ImportDeclaration` / `ExportDeclaration` nodes. Imports do not
/// produce runtime statements themselves; instead the compiler emits, at the
/// start of the main function, one synchronous call per distinct specifier to
/// the native import helper:
///
/// ```text
///   Closure rC, #NATIVE_IMPORT_INDEX   // rC = native function object
///   Move    r{C+1}, undef               // this = undefined
///   LoadConst r{C+2}, k{specifier}      // arg0 = specifier string
///   Call    rC, rC, argc=1              // rC = import(specifier)
///   // followed by GetProperty + Store for each `imported` binding from
///   // that specifier (`*` binds the whole namespace object).
/// ```
///
/// The helper is expected at `NativeRegistry` index [`NATIVE_IMPORT_INDEX`]
/// (254) and is implemented by `v12-engine` as a host hook that
/// synchronously loads the target module and returns its namespace object.
/// Reusing `Call` with a synthetic `Closure` callee keeps the lowering
/// portable without an `ImportCall` opcode.
#[derive(Debug, Clone)]
pub struct Module {
    pub program: Program,
    pub imports: Vec<ImportEntry>,
    pub exports: Vec<ExportEntry>,
}

/// Parses and compiles JavaScript source text (script grammar, strict mode
/// inherited from a `"use strict"` directive prologue).
///
/// Compatibility shim over [`compile_source_with_interner`]: compiles against
/// a throwaway interner and discards the string table.
pub fn compile_source(src: &str) -> Result<Program, CompileError> {
    compile_source_with_interner(src, &mut Interner::default())
}

/// The primary compilation entry point: parses and compiles JavaScript source
/// text (script grammar, strict mode inherited from a `"use strict"`
/// directive prologue) while interning every identifier / string literal into
/// `interner`.
///
/// Callers may share one interner across any number of compilations:
/// identifiers seen before reuse their existing keys, so the ids inside the
/// returned [`Program`]'s [`v12_bytecode::Const::Str32`] constants are stable
/// per string for the interner's lifetime. Once all compilation is done, use
/// [`freeze_interner`] to obtain a resolver that maps those keys back to
/// their text.
///
/// Scripts must not contain `import` or `export` declarations; such sources
/// are rejected with `"import statements only valid in modules"` (and the
/// analogous export message) so callers can surface a clear diagnostic
/// rather than a generic parse error.
pub fn compile_source_with_interner(
    src: &str,
    interner: &mut Interner,
) -> Result<Program, CompileError> {
    // Cheap pre-check: module syntax is a strict superset of script syntax
    // for this subset, and `SourceType::script()` panics on `import` rather
    // than producing a useful `Module`-aware AST. Probing as a module first
    // lets us surface the spec-mandated "only valid in modules" diagnostic
    // instead of a generic parse error.
    if let Some(err) = early_module_syntax_error(src) {
        return Err(err);
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, src, SourceType::script()).parse();
    if parsed.panicked {
        // Panicked parses carry at least one diagnostic; fall through to the
        // check below, which surfaces it.
        if let Some(d) = parsed.diagnostics.errors().next() {
            return Err(CompileError {
                message: format!("parse error: {d}"),
                span: d.labels.first().map(|l| {
                    let s = l.span();
                    (s.start, s.end)
                }),
            });
        }
        return Err(CompileError {
            message: "parse error".into(),
            span: None,
        });
    }
    let semantic = oxc_semantic::SemanticBuilder::new().build(&parsed.program);
    if let Some(d) = semantic.diagnostics.errors().next() {
        return Err(CompileError {
            message: format!("semantic error: {d}"),
            span: d.labels.first().map(|l| {
                let s = l.span();
                (s.start, s.end)
            }),
        });
    }
    let scoping = semantic.semantic.into_scoping();
    let program = compile_ast_inner(&parsed.program, &scoping, interner)?;
    // `collect` records imports/exports even for scripts; scripts must reject
    // them explicitly rather than silently ignoring.
    if let Some(err) = script_linkage_error(&program) {
        // Prefer the module-only diagnostic when present.
        return Err(err);
    }
    Ok(program)
}

/// Compiles `src` as an ES module.
///
/// Unlike [`compile_source`], `import` and `export` declarations are
/// accepted and recorded in [`Module::imports`] / [`Module::exports`]. The
/// resulting [`Module::program`] is still directly executable: the compiler
/// lowers each distinct import specifier to a synchronous `Call` to the
/// native helper at [`NATIVE_IMPORT_INDEX`] (see [`Module`] docs).
pub fn compile_source_as_module(src: &str) -> Result<Module, CompileError> {
    compile_source_as_module_with_interner(src, &mut Interner::default())
}

/// Module compilation with a caller-owned interner (see
/// [`compile_source_with_interner`]).
pub fn compile_source_as_module_with_interner(
    src: &str,
    interner: &mut Interner,
) -> Result<Module, CompileError> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, src, SourceType::mjs()).parse();
    if parsed.panicked {
        if let Some(d) = parsed.diagnostics.errors().next() {
            return Err(CompileError {
                message: format!("parse error: {d}"),
                span: d.labels.first().map(|l| {
                    let s = l.span();
                    (s.start, s.end)
                }),
            });
        }
        return Err(CompileError {
            message: "parse error".into(),
            span: None,
        });
    }
    let semantic = oxc_semantic::SemanticBuilder::new().build(&parsed.program);
    if let Some(d) = semantic.diagnostics.errors().next() {
        return Err(CompileError {
            message: format!("semantic error: {d}"),
            span: d.labels.first().map(|l| {
                let s = l.span();
                (s.start, s.end)
            }),
        });
    }
    let scoping = semantic.semantic.into_scoping();
    compile_ast_as_module_inner(&parsed.program, &scoping, interner)
}

/// Compiles an already-parsed module `Program` (module source type) into a
/// [`Module`]. A seam for embedders that own the parse pipeline.
pub fn compile_ast_as_module(
    program: &oxc_ast::ast::Program<'_>,
    scoping: &Scoping,
) -> Result<Module, CompileError> {
    compile_ast_as_module_inner(program, scoping, &mut Interner::default())
}

fn early_module_syntax_error(src: &str) -> Option<CompileError> {
    // Parse as a module in a throwaway arena; module declarations are the
    // only syntax that distinguishes scripts from modules for our subset.
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, src, SourceType::mjs()).parse();
    // If the file is empty or contains only script syntax, there will be no
    // module declarations.
    for stmt in &parsed.program.body {
        if stmt.as_module_declaration().is_some() {
            // Distinguish import vs export for the diagnostic wording.
            let is_import = stmt.as_module_declaration().is_some_and(|md| {
                matches!(md, oxc_ast::ast::ModuleDeclaration::ImportDeclaration(_))
            });
            let msg = if is_import {
                "import statements only valid in modules"
            } else {
                "export statements only valid in modules"
            };
            let span = stmt.span();
            return Some(CompileError {
                message: msg.into(),
                span: Some((span.start, span.end)),
            });
        }
    }
    // Some parse errors (e.g., `import` in a script parsed as script) are
    // reported as diagnostics without ever materialising a
    // `ModuleDeclaration` node. A textual check covers that fallback so
    // scripts containing the keywords still get the module-only diagnostic.
    let has_import_kw = src.contains("import");
    let has_export_kw = src.contains("export");
    if (has_import_kw || has_export_kw) && parsed.panicked {
        // Re-parse as script to see if diagnostics mention import/export;
        // if they do, surface the friendly message.
        let allocator2 = Allocator::default();
        let script_parsed = Parser::new(&allocator2, src, SourceType::script()).parse();
        let diag_text = script_parsed
            .diagnostics
            .errors()
            .map(|d| format!("{d}"))
            .collect::<Vec<_>>()
            .join("; ");
        if diag_text.contains("import") || diag_text.contains("export") {
            let msg = if has_import_kw {
                "import statements only valid in modules"
            } else {
                "export statements only valid in modules"
            };
            return Some(CompileError {
                message: msg.into(),
                span: None,
            });
        }
    }
    None
}

fn script_linkage_error(program: &Program) -> Option<CompileError> {
    // Placeholder: actual linkage check happens on `Plans` in
    // `compile_ast_inner`; this helper is kept for symmetry and future
    // `Program`-level string searches. For now scripts are rejected
    // earlier via `early_module_syntax_error`.
    let _ = program;
    None
}

/// Consumes a finished interner into a key→string table: resolving a
/// [`v12_bytecode::Const::Str32`] id's key yields the interned text, for
/// consumers like error messages and disassembly.
///
/// Freezing produces a [`lasso::RodeoResolver`] — deliberately not a
/// [`lasso::RodeoReader`] — because only key→string lookups happen after
/// compilation. String→key never does: runtime property names internalize
/// inside the GC heap's own table instead of this compiler-side one.
pub fn freeze_interner(interner: Interner) -> lasso::RodeoResolver<lasso::Spur> {
    interner.into_resolver()
}

/// Compatibility shim over [`compile_source_with_interner`] + [`freeze_interner`]:
/// compiles against a fresh interner and drains it into a `Vec<String>`
/// indexed by [`v12_bytecode::Const::Str32`] id. Prefer the interner-based API.
pub fn compile_source_with_strings(src: &str) -> Result<(Program, Vec<String>), CompileError> {
    let mut interner = Interner::default();
    let program = compile_source_with_interner(src, &mut interner)?;
    let strings = freeze_interner(interner)
        .iter()
        .map(|(_, s)| s.to_string())
        .collect();
    Ok((program, strings))
}

/// Compiles an already-parsed, already-analyzed program, reusing the caller's
/// oxc output (a seam for embedders that own the parse pipeline).
pub fn compile_ast(
    program: &oxc_ast::ast::Program<'_>,
    scoping: &Scoping,
) -> Result<Program, CompileError> {
    compile_ast_inner(program, scoping, &mut Interner::default())
}

fn compile_ast_inner(
    program: &oxc_ast::ast::Program<'_>,
    scoping: &Scoping,
    interner: &mut Interner,
) -> Result<Program, CompileError> {
    let plans = collect::collect(program, scoping)?;
    // Scripts must not carry module linkage.
    if !plans.imports.is_empty() {
        let span = plans.imports.first().and_then(|e| e.span);
        return Err(CompileError {
            message: "import statements only valid in modules".into(),
            span,
        });
    }
    if !plans.exports.is_empty() {
        let span = plans.exports.first().and_then(|e| e.span);
        return Err(CompileError {
            message: "export statements only valid in modules".into(),
            span,
        });
    }
    let strict = has_use_strict(program);
    let mut comp = model::Compiler {
        scoping,
        strict,
        strings: interner,
        plans,
        functions: Vec::new(),
    };
    unit::compile_unit(&mut comp, 0, unit::UnitNode::Main(program))?;
    for fb in &comp.functions {
        fb.validate().map_err(|e| CompileError {
            message: format!("validate: {e}"),
            span: None,
        })?;
    }
    Ok(Program {
        functions: comp.functions,
        main: 0,
    })
}

fn compile_ast_as_module_inner(
    program: &oxc_ast::ast::Program<'_>,
    scoping: &Scoping,
    interner: &mut Interner,
) -> Result<Module, CompileError> {
    let plans = collect::collect(program, scoping)?;
    // Modules are always strict, even without a directive.
    let strict = true;
    let imports = plans.imports.clone();
    let exports = plans.exports.clone();
    let mut comp = model::Compiler {
        scoping,
        strict,
        strings: interner,
        plans,
        functions: Vec::new(),
    };
    unit::compile_unit(&mut comp, 0, unit::UnitNode::Main(program))?;
    for fb in &comp.functions {
        fb.validate().map_err(|e| CompileError {
            message: format!("validate: {e}"),
            span: None,
        })?;
    }
    Ok(Module {
        program: Program {
            functions: comp.functions,
            main: 0,
        },
        imports,
        exports,
    })
}

fn has_use_strict(program: &oxc_ast::ast::Program<'_>) -> bool {
    program
        .directives
        .iter()
        .any(|d| d.expression.value == "use strict")
}
