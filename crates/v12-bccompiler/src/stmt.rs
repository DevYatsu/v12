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
    BindingPattern, Declaration, Expression, ForStatementInit, Function, LabeledStatement,
    ModuleDeclaration, Statement, TryStatement, VariableDeclarationKind,
};
use oxc_span::{GetSpan, Span};
use v12_bytecode::{HandlerRange, Instr, Opcode};

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

    /// Compiles one statement.
    pub fn stmt(&mut self, s: &'a Statement<'a>) -> Res<()> {
        if let Some(md) = s.as_module_declaration() {
            return match md {
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
                        Kind::FunctionDeclaration(f) => {
                            // Hoisted like a function declaration; binding
                            // already initialised by the hoist pass.
                            let _ = f;
                            Ok(())
                        }
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
                    Err(self.err(md.span(), "typescript export assignment is not supported"))
                }
                ModuleDeclaration::TSNamespaceExportDeclaration(_) => Ok(()),
            };
        }
        match s {
            Statement::BlockStatement(b) => self.stmt_list(&b.body),
            Statement::ExpressionStatement(e) => {
                self.expr(&e.expression)?;
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
                self.emit_spanned(Instr::new(Opcode::Return, v, 0, 0), r.span);
                Ok(())
            }
            Statement::ThrowStatement(t) => {
                // Throws are *not* intercepted by inline finalizer copies: the
                // handler table routes them through unwinding instead.
                let v = self.expr(&t.argument)?;
                self.emit_spanned(Instr::new(Opcode::Throw, v, 0, 0), t.span);
                Ok(())
            }
            Statement::TryStatement(t) => self.try_stmt(t),
            Statement::FunctionDeclaration(_) => {
                // Initialized by the enclosing statement list's hoist pass.
                Ok(())
            }
            Statement::VariableDeclaration(v) => self.var_decl(v),
            other => Err(self.err(other.span(), "unsupported statement")),
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

    /// `let {a, b: c} = obj` → property loads (flat identifier patterns only).
    fn object_pattern_store(&mut self, o: &oxc_ast::ast::ObjectPattern<'_>, val: u8) -> Res<()> {
        if o.rest.is_some() {
            return Err(self.err(o.span, "rest elements in destructuring are not supported"));
        }
        for prop in &o.properties {
            if !matches!(prop.value, BindingPattern::BindingIdentifier(_)) {
                return Err(self.err(prop.span, "nested destructuring patterns are not supported"));
            }
            let key = self.property_key(&prop.key)?;
            let tmp = self.new_temp();
            self.emit_spanned(Instr::new(Opcode::GetProperty, tmp, val, key), prop.span);
            let sym = binding_symbol(&prop.value)
                .ok_or_else(|| self.err(prop.span, "internal: pattern without symbol"))?;
            let access = self.access(sym);
            self.store_access(access, tmp, prop.span);
        }
        Ok(())
    }

    /// `let [a, b] = arr` → indexed loads (flat identifier patterns only).
    fn array_pattern_store(&mut self, a: &oxc_ast::ast::ArrayPattern<'_>, val: u8) -> Res<()> {
        if a.rest.is_some() {
            return Err(self.err(a.span, "rest elements in destructuring are not supported"));
        }
        for (idx, el) in a.elements.iter().enumerate() {
            let Some(pat) = el else { continue }; // holes bind nothing
            if !matches!(pat, BindingPattern::BindingIdentifier(_)) {
                return Err(self.err(
                    pat.span(),
                    "nested destructuring patterns are not supported",
                ));
            }
            let key = self.new_temp();
            self.load_str(key, &idx.to_string(), pat.span())?;
            let tmp = self.new_temp();
            self.emit_spanned(Instr::new(Opcode::GetProperty, tmp, val, key), pat.span());
            let sym = binding_symbol(pat)
                .ok_or_else(|| self.err(pat.span(), "internal: pattern without symbol"))?;
            let access = self.access(sym);
            self.store_access(access, tmp, pat.span());
        }
        Ok(())
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

    fn for_loop(&mut self, f: &'a oxc_ast::ast::ForStatement<'a>, name: Option<String>) -> Res<()> {
        match &f.init {
            None => {}
            Some(ForStatementInit::VariableDeclaration(v)) => self.var_decl(v)?,
            Some(init) => {
                let Some(e) = init.as_expression() else {
                    return Err(self.err(f.span, "unsupported for-init"));
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
                    self.emit_spanned(Instr::new(Opcode::Throw, exc_try, 0, 0), t.span);
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
                self.emit_spanned(Instr::new(Opcode::Throw, exc_try, 0, 0), t.span);
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
        exc: u8,
        span: Span,
    ) -> Res<()> {
        let Some(sym) = binding_symbol_pattern(param) else {
            return Err(self.err(span, "catch parameter must be a plain identifier"));
        };
        let access = self.access(sym);
        self.store_access(access, exc, span);
        Ok(())
    }

    /// One inline execution of a finalizer, compiled outside its own region.
    fn copy_finalizer(&mut self, fin: &'a oxc_ast::ast::BlockStatement<'a>) -> Res<()> {
        let saved = std::mem::take(&mut self.finallies);
        let r = self.stmt_list(&fin.body);
        self.finallies = saved;
        r
    }

    fn push_range(&mut self, start: u32, end: u32, target: u32, depth: u8) {
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
