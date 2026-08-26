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
//!   bitwise / comparison / strict-equality operators, `typeof`, unary
//!   `-`/`!`/`~`, `++`/`--` (prefix & postfix), assignment to locals,
//!   properties, and object members; expression statements; `if`/`else`;
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
//! - Unary `+` (no ToNumber opcode), `in`/`instanceof`, spread, optional
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
use oxc_span::SourceType;
use v12_bytecode::FunctionBytecode;

pub use model::{CompileError, Interner};

/// Compiled program: every function body plus the entry point.
///
/// `main` indexes the top-level script body in [`Program::functions`];
/// `Closure` instructions reference other entries of that same vector.
#[derive(Debug, Clone)]
pub struct Program {
    pub functions: Vec<FunctionBytecode>,
    pub main: u32,
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
pub fn compile_source_with_interner(
    src: &str,
    interner: &mut Interner,
) -> Result<Program, CompileError> {
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
    compile_ast_inner(&parsed.program, &scoping, interner)
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
    let plans = collect::collect(program, scoping);
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

fn has_use_strict(program: &oxc_ast::ast::Program<'_>) -> bool {
    program
        .directives
        .iter()
        .any(|d| d.expression.value == "use strict")
}
