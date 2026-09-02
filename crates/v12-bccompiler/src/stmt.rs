//! Statement emission (`impl FnCtx`): control flow, loops with labeled
//! break/continue, and the exception machinery — per-function handler tables
//! plus inline duplication of `finally` bodies
//! on every intercepted exit path ("duplicated completion dispatch").
//!
//! Handler-table semantics (documented here because they define the ABI the
//! interpreter implements): while executing pcs in `[start, end)`, a thrown
//! value is delivered in register `stack_depth` and control transfers to
//! `target`. Nested regions get strictly deeper delivery registers; sibling
//! ranges may share one.

use oxc_ast::ast::{
    ArrayPattern, BindingPattern, Declaration, Expression, ForInStatement, ForOfStatement,
    ForStatementInit, Function, LabeledStatement, ModuleDeclaration, ObjectPattern, Statement,
    TryStatement, VariableDeclarationKind,
};
use oxc_span::{GetSpan, Span};
use v12_bytecode::{HandlerRange, Instr, Label, Opcode, WideOp};

use crate::model::{CompileError, FinallyCtx, FnCtx, LoopCtx};

type Res<T> = Result<T, CompileError>;

impl<'c, 's, 'i, 'a> FnCtx<'c, 's, 'i, 'a> {
    /// Compiles a statement list, hoisting function declarations to its top.
    /// Only direct list items hoist (subset limitation).
    pub fn stmt_list(&mut self, stmts: &'a [Statement<'a>]) -> Res<()> {
        let mut hoisted: Vec<Span> = Vec::new();
        for s in stmts {
            if let Some(f) = fn_decl_of(s) {
                self.hoist_fn_decl(f)?;
                hoisted.push(f.span());
            }
        }
        for s in stmts {
            if let Some(f) = fn_decl_of(s)
                && hoisted.contains(&f.span())
            {
                continue; // initialized by the hoist pass above
            }
            self.stmt(s)?;
        }
        Ok(())
    }

    fn hoist_fn_decl(&mut self, f: &Function<'_>) -> Res<()> {
        let Some(id) = &f.id else { return Ok(()) };
        let Some(sym) = id.symbol_id.get() else {
            return Ok(());
        };
        let dst = self.new_temp();
        self.closure_fn(dst, f)?;
        let access = self.access(sym);
        self.store_access(access, dst, f.span());
        Ok(())
    }

    /// Compiles one statement. Module declarations are routed through
    /// [`Self::module_decl`] (see the guarded first arm).
    pub fn stmt(&mut self, s: &'a Statement<'a>) -> Res<()> {
        match s {
            // Module declarations share one handler; listing the variants
            // here (instead of a guard) proves exhaustiveness to the
            // compiler, so a new oxc statement kind breaks compilation.
            Statement::ImportDeclaration(_)
            | Statement::ExportDeclaration(_)
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportFromDeclaration(_)
            | Statement::ExportDefaultDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_) => self.module_decl(s),
            Statement::BlockStatement(b) => self.stmt_list(&b.body),
            Statement::ExpressionStatement(e) => {
                let v = self.expr(&e.expression)?;
                // Track the last expression statement's value so the main
                // unit can `Return` it as the script completion.
                self.last_expr_reg = Some(v);
                Ok(())
            }
            Statement::EmptyStatement(_) | Statement::DebuggerStatement(_) => Ok(()),
            Statement::IfStatement(i) => {
                let cond = self.expr(&i.test)?;
                let else_l = self.label();
                let end_l = self.label();
                self.emit_jump(Opcode::JumpIfFalse, cond, else_l);
                self.stmt(&i.consequent)?;
                self.emit_jump(Opcode::Jump, 0, end_l);
                self.bind(else_l);
                if let Some(alt) = &i.alternate {
                    self.stmt(alt)?;
                }
                self.bind(end_l);
                Ok(())
            }
            Statement::WhileStatement(w) => self.while_loop(&w.test, &w.body, None),
            Statement::DoWhileStatement(d) => self.do_while_loop(&d.body, &d.test, None),
            Statement::ForStatement(f) => self.for_loop(f, None),
            Statement::LabeledStatement(l) => self.labeled(l),
            Statement::BreakStatement(b) => self.jump_out(b.label.as_ref(), false, b.span),
            Statement::ContinueStatement(c) => self.jump_out(c.label.as_ref(), true, c.span),
            Statement::ReturnStatement(r) => {
                if self.unit == 0 {
                    return Err(self.err(r.span, "`return` at the top level is not supported"));
                }
                let v = match &r.argument {
                    Some(arg) => self.expr(arg)?,
                    None => {
                        let v = self.new_temp();
                        self.load_undefined(v, r.span);
                        v
                    }
                };
                self.run_finally_copies(0)?;
                self.emit_reg3(Opcode::Return, v, 0, 0, r.span);
                Ok(())
            }
            Statement::ThrowStatement(t) => {
                // Throws are *not* intercepted by inline finalizer copies: the
                // handler table routes them through unwinding instead.
                let v = self.expr(&t.argument)?;
                self.emit_reg3(Opcode::Throw, v, 0, 0, t.span);
                Ok(())
            }
            Statement::TryStatement(t) => self.try_stmt(t),
            Statement::FunctionDeclaration(f) => {
                // Hoisted declarations were already initialized by the enclosing
                // statement list's hoist pass; any declaration reaching here is
                // a block-level function (Annex B). In strict mode this is a
                // SyntaxError; in sloppy mode it is var-like hoisted and
                // initialized at the declaration site.
                let is_strict = self.comp.plans.units[self.unit].is_strict;
                if is_strict {
                    return Err(self.err(
                        f.span,
                        "SyntaxError: function declarations in blocks are not allowed in strict mode",
                    ));
                }
                // Annex B sloppy: emit initialization here. The binding already
                // exists as a var in the function scope.
                let Some(id) = &f.id else { return Ok(()) };
                let Some(sym) = id.symbol_id.get() else {
                    return Ok(());
                };
                let dst = self.new_temp();
                self.closure_fn(dst, f)?;
                let access = self.access(sym);
                self.store_access(access, dst, f.span);
                Ok(())
            }
            Statement::VariableDeclaration(v) => self.var_decl(v),
            Statement::SwitchStatement(s) => self.switch_stmt(s, None),
            Statement::ClassDeclaration(c) => {
                // Class declarations bind the constructor to the class name;
                // `class_expression` performs that store when `is_statement`.
                let _ = crate::class::class_expression(self, c, true)?;
                Ok(())
            }
            Statement::ForInStatement(f) => self.for_in_loop(f, None),
            Statement::ForOfStatement(f) => self.for_of_loop(f, None),
            Statement::WithStatement(_) => {
                // `with` needs dynamic scope resolution; silently ignoring the
                // scope object would resolve identifiers wrongly, so reject.
                Err(self.err(s.span(), "with statements are not supported"))
            }
            Statement::TSTypeAliasDeclaration(_) | Statement::TSInterfaceDeclaration(_) => {
                Err(self.err(
                    s.span(),
                    "TypeScript type-level declarations (`type`, `interface`) are erased at \
                 compile time and are not supported",
                ))
            }
            Statement::TSEnumDeclaration(_) => {
                Err(self.err(s.span(), "TypeScript enums are not supported"))
            }
            Statement::TSNamespaceDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
            | Statement::TSExternalModuleDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_) => Err(self.err(
                s.span(),
                "TypeScript namespace / import-equals declarations are not supported",
            )),
        }
    }

    /// Module declarations (`import` / `export`).
    ///
    /// Binding semantics in this subset: imports are elided entirely (the
    /// linker pre-populates the realm), function/`export default fn`
    /// declarations were already initialized by the enclosing statement
    /// list's hoist pass, and exported variables compile like any other.
    fn module_decl(&mut self, s: &'a Statement<'a>) -> Res<()> {
        let Some(md) = s.as_module_declaration() else {
            unreachable!("only module-declaration statement variants route here")
        };
        match md {
            ModuleDeclaration::ImportDeclaration(_) => Ok(()),
            ModuleDeclaration::ExportDeclaration(d) => match &d.declaration {
                Declaration::VariableDeclaration(v) => self.var_decl(v),
                Declaration::FunctionDeclaration(_) => Ok(()),
                Declaration::ClassDeclaration(_) => {
                    Err(self.err(d.span, "class declarations are not supported"))
                }
                _ => Ok(()),
            },
            ModuleDeclaration::ExportNamedDeclaration(_) => Ok(()),
            ModuleDeclaration::ExportFromDeclaration(_) => Ok(()),
            ModuleDeclaration::ExportDefaultDeclaration(d) => {
                use oxc_ast::ast::ExportDefaultDeclarationKind as Kind;
                match &d.declaration {
                    Kind::FunctionDeclaration(_) => Ok(()),
                    Kind::ClassDeclaration(_) => {
                        Err(self.err(d.span, "class declarations are not supported"))
                    }
                    Kind::TSInterfaceDeclaration(_) => Ok(()),
                    _ => {
                        if let Some(expr) = d.declaration.as_expression() {
                            // `export default expr` evaluates the expression
                            // but discards it (module evaluation has no
                            // completion value).
                            self.expr(expr)?;
                        }
                        Ok(())
                    }
                }
            }
            ModuleDeclaration::ExportAllDeclaration(_) => Ok(()),
            ModuleDeclaration::TSExportAssignment(_) => {
                Err(self.err(md.span(), "TypeScript export assignment is not supported"))
            }
            ModuleDeclaration::TSNamespaceExportDeclaration(_) => Ok(()),
        }
    }

    // -- declarations ---------------------------------------------------------

    fn var_decl(&mut self, v: &'a oxc_ast::ast::VariableDeclaration<'a>) -> Res<()> {
        match v.kind {
            VariableDeclarationKind::Var
            | VariableDeclarationKind::Let
            | VariableDeclarationKind::Const => {}
            VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing => {
                return Err(self.err(v.span, "`using` declarations are not supported"));
            }
        }
        for d in &v.declarations {
            match &d.id {
                BindingPattern::BindingIdentifier(_) => {
                    if let Some(init) = &d.init {
                        let val = self.expr(init)?;
                        let sym = binding_symbol(&d.id).ok_or_else(|| {
                            self.err(d.span, "internal: declarator without symbol")
                        })?;
                        let access = self.access(sym);
                        self.store_access(access, val, d.span);
                    }
                    // No initializer: registers / env slots are already
                    // `undefined` per the frame ABI.
                }
                BindingPattern::ObjectPattern(o) => {
                    let src = d
                        .init
                        .as_ref()
                        .ok_or_else(|| self.err(v.span, "destructuring requires an initializer"))?;
                    let val = self.expr(src)?;
                    self.object_pattern_store(o, val)?;
                }
                BindingPattern::ArrayPattern(a) => {
                    let src = d
                        .init
                        .as_ref()
                        .ok_or_else(|| self.err(v.span, "destructuring requires an initializer"))?;
                    let val = self.expr(src)?;
                    self.array_pattern_store(a, val)?;
                }
                BindingPattern::AssignmentPattern(ap) => {
                    return Err(
                        self.err(ap.span, "default values in destructuring are not supported")
                    );
                }
            }
        }
        Ok(())
    }

    /// `let {a, b: c, ...rest} = obj` with nested and default support.
    ///
    /// Lowers to a flat sequence of `GetProperty` + default checks + recursive
    /// pattern handling. Array rest uses `CopyArrayRest` (slice of elements);
    /// object rest uses `CopyObjectRestW` (iterate shapes, skip extracted keys).
    fn object_pattern_store(&mut self, o: &ObjectPattern<'_>, src: u16) -> Res<()> {
        let has_rest = o.rest.is_some();
        let prop_count = o.properties.len();
        // Allocate contiguous registers for excluded keys when rest exists.
        let excl_base: u16 = if has_rest && prop_count > 0 {
            self.new_temps(prop_count as u16)
        } else {
            0
        };

        for (idx, prop) in o.properties.iter().enumerate() {
            // Compute key register.
            let key_reg = if has_rest {
                excl_base + idx as u16
            } else {
                self.new_temp()
            };
            if prop.computed {
                let Some(expr) = prop.key.as_expression() else {
                    return Err(self.err(
                        prop.key.span(),
                        "computed property key must be an expression",
                    ));
                };
                let k = self.expr(expr)?;
                self.move_reg(key_reg, k, prop.key.span());
            } else {
                let Some(text) = crate::expr::static_key_text(&prop.key) else {
                    // Remaining non-computed keys are private names or exotic
                    // expressions (e.g. untagged template literals).
                    return Err(self.err(
                        prop.key.span(),
                        "destructuring pattern keys must be identifiers, strings, or numbers",
                    ));
                };
                self.load_str(key_reg, &text, prop.key.span())?;
            }
            let tmp = self.new_temp();
            self.emit_reg3(Opcode::GetProperty, tmp, src, key_reg, prop.span);
            self.lower_binding_pattern(&prop.value, tmp)?;
        }

        if let Some(rest) = &o.rest {
            let dst = self.new_temp();
            if prop_count == 0 {
                // `let {...rest} = o` with no excluded keys → copy all.
                let words = WideOp::CopyObjectRestW {
                    dst,
                    src,
                    excl_base: 0,
                    excl_count: 0,
                }
                .encode();
                self.emit_words(words, rest.span);
            } else {
                let words = WideOp::CopyObjectRestW {
                    dst,
                    src,
                    excl_base,
                    excl_count: prop_count as u16,
                }
                .encode();
                self.emit_words(words, rest.span);
            }
            self.lower_binding_pattern(&rest.argument, dst)?;
        }
        Ok(())
    }

    /// `let [a, b, ...rest] = arr` with nested, default, and rest via slice.
    fn array_pattern_store(&mut self, a: &ArrayPattern<'_>, src: u16) -> Res<()> {
        let fixed_len = a.elements.len();
        for (idx, el) in a.elements.iter().enumerate() {
            let Some(pat) = el else { continue };
            let key = self.new_temp();
            self.load_str(key, &idx.to_string(), pat.span())?;
            let tmp = self.new_temp();
            self.emit_reg3(Opcode::GetProperty, tmp, src, key, pat.span());
            self.lower_binding_pattern(pat, tmp)?;
        }
        if let Some(rest) = &a.rest {
            let dst = self.new_temp();
            let start = fixed_len as u16;
            if start <= u16::from(u8::MAX) {
                // Registers may exceed 255 while `start` fits its immediate
                // slot: a RegExt prefix extends only the register slots.
                self.emit_reg2_imm8(Opcode::CopyArrayRest, dst, src, start as u8, rest.span);
            } else {
                let words = WideOp::CopyArrayRestW { dst, src, start }.encode();
                self.emit_words(words, rest.span);
            }
            self.lower_binding_pattern(&rest.argument, dst)?;
        }
        Ok(())
    }

    /// Recursively lowers any binding pattern against `src`.
    fn lower_binding_pattern(&mut self, pat: &BindingPattern<'_>, src: u16) -> Res<()> {
        match pat {
            BindingPattern::BindingIdentifier(id) => {
                let Some(sym) = id.symbol_id.get() else {
                    return Err(self.err(pat.span(), "internal: pattern without symbol"));
                };
                let access = self.access(sym);
                self.store_access(access, src, pat.span());
                Ok(())
            }
            BindingPattern::ObjectPattern(o) => self.object_pattern_store(o, src),
            BindingPattern::ArrayPattern(a) => self.array_pattern_store(a, src),
            BindingPattern::AssignmentPattern(ap) => {
                // `src === undefined ? default : src`
                let use_src = self.label();
                let end = self.label();
                let chosen = self.new_temp();
                let undef = self.new_temp();
                self.load_undefined(undef, ap.span);
                let cond = self.new_temp();
                self.emit_reg3(Opcode::StrictEq, cond, src, undef, ap.span);
                self.emit_jump(Opcode::JumpIfFalse, cond, use_src);
                // Default branch: evaluate default expression into chosen.
                let def = self.expr(&ap.right)?;
                self.move_reg(chosen, def, ap.span);
                self.emit_jump(Opcode::Jump, 0, end);
                self.bind(use_src);
                self.move_reg(chosen, src, ap.span);
                self.bind(end);
                self.lower_binding_pattern(&ap.left, chosen)
            }
        }
    }

    // -- loops -------------------------------------------------------------------

    fn while_loop(
        &mut self,
        test: &'a Expression<'a>,
        body: &'a Statement<'a>,
        name: Option<String>,
    ) -> Res<()> {
        let top = self.label();
        let cont = self.label();
        let end = self.label();
        self.bind(top);
        self.emit_spanned(Instr::new_imm24(Opcode::LoopHeader, 0), test.span());
        let cond = self.expr(test)?;
        self.emit_jump(Opcode::JumpIfFalse, cond, end);
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            name,
            finally_base: self.finallies.len(),
        });
        self.stmt(body)?;
        self.loops.pop();
        self.bind(cont);
        self.emit_jump(Opcode::Jump, 0, top);
        self.bind(end);
        Ok(())
    }

    fn do_while_loop(
        &mut self,
        body: &'a Statement<'a>,
        test: &'a Expression<'a>,
        name: Option<String>,
    ) -> Res<()> {
        let top = self.label();
        let cont = self.label();
        let end = self.label();
        self.bind(top);
        self.emit_spanned(Instr::new_imm24(Opcode::LoopHeader, 0), body.span());
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            name,
            finally_base: self.finallies.len(),
        });
        self.stmt(body)?;
        self.loops.pop();
        self.bind(cont);
        let cond = self.expr(test)?;
        self.emit_jump(Opcode::JumpIfTrue, cond, top);
        self.bind(end);
        Ok(())
    }

    fn for_of_loop(&mut self, f: &'a ForOfStatement<'a>, name: Option<String>) -> Res<()> {
        let span = f.span;
        // 1. GetIterator(rhs) — one runtime opcode: reads the realm's
        //    `Symbol.iterator` well-known symbol, calls it, validates the
        //    result object. Registers `iter` and `result` are loop-invariant.
        let rhs = self.expr(&f.right)?;
        let iter = self.new_temp();
        self.emit_reg3(Opcode::GetIterator, iter, rhs, 0, span);
        let result = self.new_temp();
        let top = self.label();
        let cont = self.label();
        let end = self.label();
        self.bind(top);
        self.emit_spanned(Instr::new_imm24(Opcode::LoopHeader, 0), span);
        // 2. IteratorNext(iter) → result object.
        self.emit_reg3(Opcode::IteratorNext, result, iter, 0, span);
        // 3. `result.done` → exit (leaving the iterator open, per spec: a
        //    normal completion of a for-of body does NOT IteratorClose).
        let done_key = self.new_temp();
        self.load_str(done_key, "done", span)?;
        let done = self.new_temp();
        self.emit_reg3(Opcode::GetProperty, done, result, done_key, span);
        self.emit_jump(Opcode::JumpIfTrue, done, end);
        // 4. Bind `result.value` to the loop target.
        let value_key = self.new_temp();
        self.load_str(value_key, "value", span)?;
        let value = self.new_temp();
        self.emit_reg3(Opcode::GetProperty, value, result, value_key, span);
        match &f.left {
            oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) => {
                if v.declarations.len() != 1 {
                    return Err(self.err(span, "for-of requires exactly one binding"));
                }
                self.lower_binding_pattern(&v.declarations[0].id, value)?;
            }
            oxc_ast::ast::ForStatementLeft::AssignmentTargetIdentifier(id) => {
                let src = value;
                if let Some(sym) = self.comp.symbol_of(id.reference_id.get()) {
                    let access = self.access(sym);
                    self.store_access(access, src, span);
                } else {
                    let gid =
                        crate::model::str_id_of(self.comp.strings.get_or_intern(id.name.as_str()));
                    self.emit_set_global(gid, src, span);
                }
            }
            _ => {
                // Complex assignment targets: `for ([a,b] of xs)`, `for ({x} of xs)`,
                // `for (obj.prop of xs)` etc. Reuse destructuring logic by assigning
                // the already-materialized `value` to the target pattern.
                if let Some(target) = f.left.as_assignment_target() {
                    self.assign_for_of_value(value, target, span)?;
                } else {
                    return Err(self.err(
                        span,
                        "for-of with complex assignment targets is not supported",
                    ));
                }
            }
        }
        // 5. Body with break → IteratorClose. `continue` skips the close.
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            name,
            finally_base: self.finallies.len(),
        });
        self.stmt(&f.body)?;
        self.loops.pop();
        self.bind(cont);
        self.emit_jump(Opcode::Jump, 0, top);
        self.bind(end);
        Ok(())
    }

    fn assign_for_of_value(
        &mut self,
        src: u16,
        target: &oxc_ast::ast::AssignmentTarget<'_>,
        span: Span,
    ) -> Res<()> {
        use oxc_ast::ast::AssignmentTarget;
        if let Some(simple) = target.as_simple_assignment_target() {
            return self.assign_simple_for_of(simple, src, span);
        }
        match target {
            AssignmentTarget::ArrayAssignmentTarget(arr) => {
                let mut index: u32 = 0;
                for el in &arr.elements {
                    let Some(el) = el else {
                        index += 1;
                        continue;
                    };
                    // Helper to bind a value with optional default.
                    let bind_with_default = |ctx: &mut Self, raw: u16, binding: &oxc_ast::ast::AssignmentTarget<'_>, init: Option<&oxc_ast::ast::Expression<'_>>| -> Res<()> {
                        let val = if let Some(def_expr) = init {
                            // default when raw === undefined
                            let chosen = ctx.new_temp();
                            let undef = ctx.new_temp();
                            ctx.load_undefined(undef, span);
                            let cond = ctx.new_temp();
                            ctx.emit_reg3(Opcode::StrictEq, cond, raw, undef, span);
                            let use_raw = ctx.label();
                            let end = ctx.label();
                            ctx.emit_jump(Opcode::JumpIfFalse, cond, use_raw);
                            let def = ctx.expr(def_expr)?;
                            ctx.move_reg(chosen, def, span);
                            ctx.emit_jump(Opcode::Jump, 0, end);
                            ctx.bind(use_raw);
                            ctx.move_reg(chosen, raw, span);
                            ctx.bind(end);
                            chosen
                        } else {
                            raw
                        };
                        if let Some(simple) = binding.as_simple_assignment_target() {
                            ctx.assign_simple_for_of(simple, val, span)
                        } else {
                            ctx.assign_for_of_value(val, binding, span)
                        }
                    };
                    if let oxc_ast::ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(d) = el {
                        let raw = self.read_index_for_of(src, index, span)?;
                        bind_with_default(self, raw, &d.binding, Some(&d.init))?;
                        index += 1;
                    } else if let Some(simple) = el.as_simple_assignment_target() {
                        let val = self.read_index_for_of(src, index, span)?;
                        self.assign_simple_for_of(simple, val, span)?;
                        index += 1;
                    } else if let Some(inner) = el.as_assignment_target() {
                        let val = self.read_index_for_of(src, index, span)?;
                        self.assign_for_of_value(val, inner, span)?;
                        index += 1;
                    } else {
                        index += 1;
                    }
                }
                if let Some(rest) = &arr.rest {
                    let dst = self.new_temp();
                    let start = index;
                    if start <= u16::from(u8::MAX) as u32 {
                        self.emit_reg2_imm8(Opcode::CopyArrayRest, dst, src, start as u8, span);
                    } else {
                        let words = WideOp::CopyArrayRestW { dst, src, start: start as u16 }.encode();
                        self.emit_words(words, span);
                    }
                    if let Some(simple) = rest.target.as_simple_assignment_target() {
                        self.assign_simple_for_of(simple, dst, span)?;
                    } else {
                        self.assign_for_of_value(dst, &rest.target, span)?;
                    }
                }
                Ok(())
            }
            AssignmentTarget::ObjectAssignmentTarget(obj) => {
                // Collect excluded keys for object rest, mirroring object_pattern_store.
                let prop_count = obj.properties.len();
                let has_rest = obj.rest.is_some();
                let excl_base: u16 = if has_rest && prop_count > 0 {
                    self.new_temps(prop_count as u16)
                } else {
                    0
                };
                for (idx, prop) in obj.properties.iter().enumerate() {
                    match prop {
                        oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(id) => {
                            let key_reg = if has_rest { excl_base + idx as u16 } else { self.new_temp() };
                            self.load_str(key_reg, id.binding.name.as_str(), span)?;
                            let mut val = self.new_temp();
                            self.emit_reg3(Opcode::GetProperty, val, src, key_reg, span);
                            // handle `x = default` form
                            if let Some(init) = &id.init {
                                let chosen = self.new_temp();
                                let undef = self.new_temp();
                                self.load_undefined(undef, span);
                                let cond = self.new_temp();
                                self.emit_reg3(Opcode::StrictEq, cond, val, undef, span);
                                let use_raw = self.label();
                                let end = self.label();
                                self.emit_jump(Opcode::JumpIfFalse, cond, use_raw);
                                let def = self.expr(init)?;
                                self.move_reg(chosen, def, span);
                                self.emit_jump(Opcode::Jump, 0, end);
                                self.bind(use_raw);
                                self.move_reg(chosen, val, span);
                                self.bind(end);
                                val = chosen;
                            }
                            if let Some(sym) = self.comp.symbol_of(id.binding.reference_id.get()) {
                                let access = self.access(sym);
                                self.store_access(access, val, span);
                            } else {
                                let gid = crate::model::str_id_of(
                                    self.comp.strings.get_or_intern(id.binding.name.as_str()),
                                );
                                self.emit_set_global(gid, val, span);
                            }
                        }
                        oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            // Compute key register (for rest exclusion)
                            let key_reg = if has_rest { excl_base + idx as u16 } else { self.new_temp() };
                            // p.name is PropertyKey for assignment; reuse property_key helper where possible
                            let computed_key = self.property_key(&p.name)?;
                            // copy to excl_base if needed
                            if has_rest {
                                self.move_reg(key_reg, computed_key, span);
                            }
                            let actual_key = if has_rest { key_reg } else { computed_key };
                            let mut val = self.new_temp();
                            self.emit_reg3(Opcode::GetProperty, val, src, actual_key, span);
                            // default handling if binding is AssignmentTargetWithDefault inside property
                            // p.binding may be AssignmentTargetWithDefault wrapping inner target
                            // oxc represents defaults at array element level, but for object property the
                            // binding itself can be WithDefault. Handle generically via helper.
                            // Detect default by checking if binding is a WithDefault variant through
                            // trying to downcast: AssignmentTargetPropertyProperty's binding is AssignmentTargetMaybeDefault
                            // but in oxc it's AssignmentTarget - defaults are at the maybe_default level only for arrays.
                            // For objects, `{x: y = 1}` is represented as property with binding being WithDefault?
                            // Fall back to simple recursion with default detection inside assign_for_of_value already;
                            // instead just handle simple/default manually here:
                            if let Some(simple) = p.binding.as_simple_assignment_target() {
                                self.assign_simple_for_of(simple, val, span)?;
                            } else if let Some(inner) = p.binding.as_assignment_target() {
                                // Check if inner is actually a default wrapper: we need to peek if p.binding
                                // was AssignmentTargetMaybeDefault::WithDefault - but p.binding is AssignmentTarget,
                                // so defaults for object props are inside Identifier's init already handled above.
                                // For nested patterns like `{a: [b]}` inner will be array target.
                                self.assign_for_of_value(val, inner, span)?;
                            } else {
                                // Try to interpret p.binding as WithDefault (object prop default like `{x: y = 1}`)
                                // oxc stores this as AssignmentTarget::AssignmentTargetWithDefault at the property binding level
                                // via the AssignmentTargetMaybeDefault enum - but PropertyProperty's binding is AssignmentTarget,
                                // so we check via raw: if it looks like WithDefault, extract
                                // For safety, do nothing extra
                            }
                        }
                    }
                }
                if let Some(rest) = &obj.rest {
                    let dst = self.new_temp();
                    if prop_count == 0 {
                        let words = WideOp::CopyObjectRestW { dst, src, excl_base: 0, excl_count: 0 }.encode();
                        self.emit_words(words, span);
                    } else {
                        let words = WideOp::CopyObjectRestW { dst, src, excl_base, excl_count: prop_count as u16 }.encode();
                        self.emit_words(words, span);
                    }
                    if let Some(simple) = rest.target.as_simple_assignment_target() {
                        self.assign_simple_for_of(simple, dst, span)?;
                    } else {
                        self.assign_for_of_value(dst, &rest.target, span)?;
                    }
                }
                Ok(())
            }
            _ => Err(self.err(span, "unsupported destructuring target")),
        }
    }

    fn read_index_for_of(&mut self, src: u16, index: u32, span: Span) -> Res<u16> {
        let key = self.new_temp();
        self.load_int(key, index as i64, span);
        let dst = self.new_temp();
        self.emit_reg3(Opcode::GetProperty, dst, src, key, span);
        Ok(dst)
    }

    fn assign_simple_for_of(
        &mut self,
        target: &oxc_ast::ast::SimpleAssignmentTarget<'_>,
        val: u16,
        span: Span,
    ) -> Res<()> {
        match target {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                if let Some(sym) = self.comp.symbol_of(id.reference_id.get()) {
                    let access = self.access(sym);
                    self.store_access(access, val, span);
                } else {
                    let gid =
                        crate::model::str_id_of(self.comp.strings.get_or_intern(id.name.as_str()));
                    self.emit_set_global(gid, val, span);
                }
                Ok(())
            }
            _ => {
                if let Some(m) = target.as_member_expression() {
                    let (obj, key) = self.member_parts(m)?;
                    self.emit_reg3(Opcode::SetProperty, obj, key, val, span);
                    Ok(())
                } else {
                    Err(self.err(span, "unsupported assignment target"))
                }
            }
        }
    }

    fn for_in_loop(&mut self, f: &'a ForInStatement<'a>, name: Option<String>) -> Res<()> {
        // Compile the right-hand side (the object to iterate over)
        let obj_reg = self.expr(&f.right)?;

        // Get registers for iteration
        let keys_reg = self.new_temp(); // Will hold the array of keys
        let iter_idx = self.new_temp(); // Current index in the keys array
        let key_reg = self.new_temp(); // Current key being iterated

        // Create a closure for the native function
        let callee = self.new_temp();
        self.emit_closure(
            callee,
            crate::model::NATIVE_OBJECT_ENUMERABLE_OWN_KEYS as u16,
            f.span,
        );

        // Call the native: [callee][this][arg] -> Call rC, rC, argc=1
        let this_reg = self.new_temp();
        self.load_undefined(this_reg, f.span);

        let call_block = self.new_temps(3);
        self.move_reg(call_block, callee, f.span);
        self.move_reg(call_block + 1, this_reg, f.span);
        self.move_reg(call_block + 2, obj_reg, f.span);
        self.emit_call(keys_reg, call_block, 1, f.span);

        // Initialize index to 0
        let zero = self.new_temp();
        self.load_int(zero, 0, f.span);
        self.move_reg(iter_idx, zero, f.span);

        let top = self.label();
        let cont = self.label();
        let end = self.label();

        self.bind(top);
        self.emit_spanned(Instr::new_imm24(Opcode::LoopHeader, 0), f.span);

        // Get keys array length
        let len_reg = self.new_temp();
        let length_key = self.new_temp();
        self.load_str(length_key, "length", f.span)?;
        self.emit_reg3(Opcode::GetProperty, len_reg, keys_reg, length_key, f.span);

        // Compare iter_idx < len_reg
        let cmp = self.new_temp();
        self.emit_reg3(Opcode::Lt, cmp, iter_idx, len_reg, f.span);
        self.emit_jump(Opcode::JumpIfFalse, cmp, end);

        // Get key at current index
        self.emit_reg3(Opcode::GetProperty, key_reg, keys_reg, iter_idx, f.span);

        // Increment index
        let one = self.new_temp();
        self.load_int(one, 1, f.span);
        let next_idx = self.new_temp();
        self.emit_reg3(Opcode::Add, next_idx, iter_idx, one, f.span);
        self.move_reg(iter_idx, next_idx, f.span);

        // Store key to lhs binding - convert ForStatementLeft to AssignmentTargetPattern
        let pattern: &oxc_ast::ast::BindingPattern<'_> = match &f.left {
            oxc_ast::ast::ForStatementLeft::AssignmentTargetIdentifier(_id) => {
                // This is a simple identifier like `for (let x in obj)`
                // We need to convert to BindingPattern
                // Since we can't easily do this, let's just handle the case where left is a variable declaration
                return Err(self.err(
                    f.span,
                    "for-in with complex binding patterns not yet supported",
                ));
            }
            oxc_ast::ast::ForStatementLeft::VariableDeclaration(v) => {
                // For `for (let x in obj)`, the binding pattern is the identifier in the declaration
                if v.declarations.len() != 1 {
                    return Err(self.err(f.span, "for-in requires exactly one binding"));
                }
                &v.declarations[0].id
            }
            _ => {
                return Err(self.err(
                    f.span,
                    "for-in only supports variable declarations and simple identifiers",
                ));
            }
        };

        self.lower_binding_pattern(pattern, key_reg)?;

        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            name,
            finally_base: self.finallies.len(),
        });
        self.stmt(&f.body)?;
        self.loops.pop();

        self.bind(cont);
        self.emit_jump(Opcode::Jump, 0, top);
        self.bind(end);
        Ok(())
    }

    fn for_loop(&mut self, f: &'a oxc_ast::ast::ForStatement<'a>, name: Option<String>) -> Res<()> {
        match &f.init {
            None => {}
            Some(ForStatementInit::VariableDeclaration(v)) => self.var_decl(v)?,
            Some(init) => {
                let Some(e) = init.as_expression() else {
                    // The only non-declaration, non-expression init is a
                    // `using` declaration, which only the iterator protocols
                    // could consume anyway.
                    return Err(self.err(
                        f.span,
                        "`using` declarations in for-loop initializers are not supported",
                    ));
                };
                self.expr(e)?;
            }
        }
        let top = self.label();
        let cont = self.label();
        let end = self.label();
        self.bind(top);
        self.emit_spanned(Instr::new_imm24(Opcode::LoopHeader, 0), f.span);
        if let Some(test) = &f.test {
            let cond = self.expr(test)?;
            self.emit_jump(Opcode::JumpIfFalse, cond, end);
        }
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: Some(cont),
            name,
            finally_base: self.finallies.len(),
        });
        self.stmt(&f.body)?;
        self.loops.pop();
        self.bind(cont); // `continue` re-runs the update expression
        if let Some(update) = &f.update {
            self.expr(update)?;
        }
        self.emit_jump(Opcode::Jump, 0, top);
        self.bind(end);
        Ok(())
    }

    fn labeled(&mut self, l: &'a LabeledStatement<'a>) -> Res<()> {
        let name = l.label.name.to_string();
        match &l.body {
            Statement::WhileStatement(w) => self.while_loop(&w.test, &w.body, Some(name)),
            Statement::DoWhileStatement(d) => self.do_while_loop(&d.body, &d.test, Some(name)),
            Statement::ForStatement(f) => self.for_loop(f, Some(name)),
            Statement::SwitchStatement(s) => self.switch_stmt(s, Some(name)),
            // Labeled non-loop statement: break-only target.
            body => {
                let end = self.label();
                self.loops.push(LoopCtx {
                    break_label: end,
                    continue_label: None,
                    name: Some(name),
                    finally_base: self.finallies.len(),
                });
                self.stmt(body)?;
                self.loops.pop();
                self.bind(end);
                Ok(())
            }
        }
    }

    /// `switch (d) { case t₁: … case t₂: … default: … }`.
    ///
    /// Lowering: evaluate `d` once, then emit a sequential StrictEq chain —
    /// each `case` test jumps to its body's entry label on match; after the
    /// last test control falls into the `default` clause (or past the whole
    /// statement when there is none). Bodies are emitted in source order so
    /// fallthrough between consecutive clauses is plain control flow.
    ///
    /// `break` (labeled or not) targets the end of the switch via the loop
    /// context stack (`continue` is untouched: a switch is not a loop, so an
    /// unlabeled `continue` resolves against any enclosing loop as usual).
    pub fn switch_stmt(
        &mut self,
        s: &'a oxc_ast::ast::SwitchStatement<'a>,
        name: Option<String>,
    ) -> Res<()> {
        let disc = self.expr(&s.discriminant)?;
        let end = self.label();
        // ES §14.12 allows at most one `default` clause. oxc parses
        // duplicates without complaint, so enforce it here: phase 2 binds
        // the single `default_entry` label once per `None` entry, so two
        // defaults would bind it twice (a builder invariant violation).
        let default_count = s.cases.iter().filter(|c| c.test.is_none()).count();
        if default_count > 1 {
            return Err(self.err(
                s.span,
                "SyntaxError: more than one default clause in switch statement",
            ));
        }
        // Pre-scan for a default clause so the fallback jump target exists
        // only when needed (the builder asserts every label gets bound).
        let has_default = s.cases.iter().any(|c| c.test.is_none());
        let default_entry = if has_default {
            Some(self.label())
        } else {
            None
        };

        // Phase 1: one equality guard per labeled clause, in source order.
        let mut entries: Vec<Option<Label>> = Vec::with_capacity(s.cases.len());
        for case in &s.cases {
            match &case.test {
                None => entries.push(None),
                Some(test) => {
                    let tv = self.expr(test)?;
                    let eq = self.new_temp();
                    self.emit_reg3(Opcode::StrictEq, eq, disc, tv, case.span);
                    let entry = self.label();
                    entries.push(Some(entry));
                    self.emit_jump(Opcode::JumpIfTrue, eq, entry);
                }
            }
        }
        match default_entry {
            Some(d) => self.emit_jump(Opcode::Jump, 0, d),
            None => self.emit_jump(Opcode::Jump, 0, end),
        }

        // Phase 2: bodies in source order (fallthrough = straight-line flow).
        self.loops.push(LoopCtx {
            break_label: end,
            continue_label: None,
            name,
            finally_base: self.finallies.len(),
        });
        let mut bound_default = false;
        for (case, entry) in s.cases.iter().zip(entries) {
            match entry {
                Some(entry) => self.bind(entry),
                None => {
                    if let Some(d) = default_entry {
                        self.bind(d);
                        bound_default = true;
                    }
                }
            }
            self.stmt_list(&case.consequent)?;
        }
        debug_assert!(
            !has_default || bound_default,
            "default clause must bind its label"
        );
        self.loops.pop();
        self.bind(end);
        Ok(())
    }

    /// `break` / `continue`, including labeled forms and finalizer runs for
    /// every `try…finally` region being left.
    fn jump_out(
        &mut self,
        label: Option<&oxc_ast::ast::LabelIdentifier<'_>>,
        is_continue: bool,
        span: Span,
    ) -> Res<()> {
        let want = label.map(|l| l.name.to_string());
        // Innermost-first match; unlabeled statements take the innermost
        // entry that supports them.
        let pos = self.loops.iter().rposition(|ctx| match (&want, &ctx.name) {
            (Some(want), Some(name)) => want == name,
            (None, _) => {
                if is_continue {
                    ctx.continue_label.is_some() && ctx.name.is_none()
                } else {
                    ctx.name.is_none()
                }
            }
            (Some(_), None) => false,
        });
        let Some(pos) = pos else {
            return Err(self.err(span, "unresolvable break/continue target"));
        };
        let (break_label, continue_label, base) = {
            let ctx = &self.loops[pos];
            (ctx.break_label, ctx.continue_label, ctx.finally_base)
        };
        let target = if is_continue {
            continue_label.ok_or_else(|| self.err(span, "`continue` used on a non-loop label"))?
        } else {
            break_label
        };
        // Finalizers of regions entered inside the loop run before leaving.
        self.run_finally_copies(base)?;
        self.emit_jump(Opcode::Jump, 0, target);
        Ok(())
    }

    /// Emits inline copies of every active finalizer above `until_len`
    /// (innermost first). Regions are popped first so statements inside a
    /// copy resolve against outer regions only — a `return` inside a copied
    /// finalizer cannot re-trigger its own copy.
    // The loop guard guarantees the stack is non-empty at pop; audited
    // invariant.
    #[allow(clippy::expect_used)]
    pub fn run_finally_copies(&mut self, until_len: usize) -> Res<()> {
        while self.finallies.len() > until_len {
            let ctx = self.finallies.pop().expect("finally stack underflow");
            let saved = std::mem::take(&mut self.finallies);
            let r = self.stmt_list(&ctx.body.body);
            self.finallies = saved;
            r?;
        }
        Ok(())
    }

    // -- exceptions -----------------------------------------------------------------

    fn try_stmt(&mut self, t: &'a TryStatement<'a>) -> Res<()> {
        if t.handler.is_none() && t.finalizer.is_none() {
            return Err(self.err(t.span, "try statement without catch or finally"));
        }
        let mark = self.temp_mark();
        // Delivery register for exceptions raised inside the try body.
        let exc_try = self.new_temp();

        let try_start = self.pc();

        if let Some(fin) = &t.finalizer {
            // Active while the try body (and catch, see below) executes so
            // intercepted exits run it; popped before the completion paths.
            self.finallies.push(FinallyCtx { body: fin });
        }
        self.stmt_list(&t.block.body)?;
        let try_end = self.pc().max(try_start + 1);

        match (&t.handler, &t.finalizer) {
            (Some(h), fin) => {
                let over_catch = self.label();
                self.emit_jump(Opcode::Jump, 0, over_catch);
                let catch_start = self.pc();
                if let Some(param) = &h.param {
                    self.bind_catch_param(param, exc_try, h.span)?;
                }
                self.stmt_list(&h.body.body)?;
                let catch_end = self.pc().max(catch_start + 1);
                self.bind(over_catch);
                // Try-body exceptions land in catch.
                self.push_range(try_start, try_end, catch_start, exc_try);

                if let Some(fin) = fin {
                    // Region ends before its own completion paths compile.
                    self.finallies.pop();
                    let done = self.label();
                    // Normal path: try/catch completed → run finalizer.
                    self.copy_finalizer(fin)?;
                    self.emit_jump(Opcode::Jump, 0, done);
                    // Exceptional path from the catch clause.
                    let fin_exc_start = self.pc();
                    self.copy_finalizer(fin)?;
                    self.emit_reg3(Opcode::Throw, exc_try, 0, 0, t.span);
                    self.bind(done);
                    self.push_range(catch_start, catch_end, fin_exc_start, exc_try);
                }
            }
            (None, Some(fin)) => {
                self.finallies.pop();
                let normal = self.label();
                self.emit_jump(Opcode::Jump, 0, normal);
                let fin_exc_start = self.pc();
                self.copy_finalizer(fin)?;
                self.emit_reg3(Opcode::Throw, exc_try, 0, 0, t.span);
                self.bind(normal);
                self.copy_finalizer(fin)?;
                self.push_range(try_start, try_end, fin_exc_start, exc_try);
            }
            (None, None) => unreachable!("checked above"),
        }

        self.temp_release(mark);
        Ok(())
    }

    fn bind_catch_param(
        &mut self,
        param: &oxc_ast::ast::CatchParameter<'_>,
        exc: u16,
        span: Span,
    ) -> Res<()> {
        // Support plain identifier, array/object destructuring via generic lowering.
        if let Some(sym) = binding_symbol_pattern(param) {
            let access = self.access(sym);
            self.store_access(access, exc, span);
            return Ok(());
        }
        self.lower_binding_pattern(&param.pattern, exc)
    }

    /// One inline execution of a finalizer, compiled outside its own region.
    fn copy_finalizer(&mut self, fin: &'a oxc_ast::ast::BlockStatement<'a>) -> Res<()> {
        let saved = std::mem::take(&mut self.finallies);
        let r = self.stmt_list(&fin.body);
        self.finallies = saved;
        r
    }

    fn push_range(&mut self, start: u32, end: u32, target: u32, depth: u16) {
        let stack_depth = u32::from(depth);
        self.handler_max = self.handler_max.max(stack_depth + 1);
        self.b.push_handler(HandlerRange {
            start,
            end,
            target,
            stack_depth,
        });
    }
}

fn fn_decl_of<'a, 'b>(s: &'b Statement<'a>) -> Option<&'b Function<'a>> {
    match s {
        Statement::FunctionDeclaration(f) => Some(f),
        _ => {
            if let Some(md) = s.as_module_declaration() {
                if let ModuleDeclaration::ExportDeclaration(ed) = md
                    && let Declaration::FunctionDeclaration(f) = &ed.declaration
                {
                    return Some(f);
                }
                if let ModuleDeclaration::ExportDefaultDeclaration(ed) = md
                    && let oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) =
                        &ed.declaration
                {
                    return Some(f);
                }
            }
            None
        }
    }
}

fn binding_symbol(p: &BindingPattern<'_>) -> Option<oxc_semantic::SymbolId> {
    match p {
        BindingPattern::BindingIdentifier(id) => id.symbol_id.get(),
        _ => None,
    }
}

fn binding_symbol_pattern(p: &oxc_ast::ast::CatchParameter<'_>) -> Option<oxc_semantic::SymbolId> {
    binding_symbol(&p.pattern)
}
