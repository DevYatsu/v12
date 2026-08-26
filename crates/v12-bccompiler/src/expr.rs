//! Expression emission (`impl FnCtx`): every `compile_expr*` returns the
//! register holding the value. Temporary registers allocated inside an
//! expression stay reserved until the caller releases its watermark at the
//! statement boundary, which keeps results alive without explicit liveness
//! tracking.

use oxc_ast::ast::{
    ArrayExpressionElement, AssignmentOperator, AssignmentTarget, BinaryOperator, CallExpression,
    Expression, LogicalOperator, MemberExpression, ObjectProperty, ObjectPropertyKind, PropertyKey,
    UnaryOperator, UpdateOperator,
};
use oxc_span::{GetSpan, Span};
use v12_bytecode::{Const, Opcode};

use crate::model::{CALL_HEADER_REGS, CompileError, FnCtx, REG_THIS, VarAccess};

type Res<T> = Result<T, CompileError>;

impl<'c, 's, 'i, 'a> FnCtx<'c, 's, 'i, 'a> {
    /// Compiles `e`, returning the register that holds its value.
    pub fn expr(&mut self, e: &Expression<'_>) -> Res<u8> {
        match e {
            Expression::BooleanLiteral(b) => {
                let dst = self.new_temp();
                self.load_bool(dst, b.value, b.span);
                Ok(dst)
            }
            Expression::NullLiteral(n) => Err(self.err(
                n.span,
                "`null` is not supported yet: the bytecode constant pool has no null kind",
            )),
            Expression::NumericLiteral(n) => {
                let dst = self.new_temp();
                self.load_number(dst, n.value, n.span)?;
                Ok(dst)
            }
            Expression::StringLiteral(s) => {
                let dst = self.new_temp();
                self.load_str(dst, s.value.as_str(), s.span)?;
                Ok(dst)
            }
            Expression::TemplateLiteral(t) => {
                if !t.expressions.is_empty() || t.quasis.len() != 1 {
                    return Err(self.err(
                        t.span,
                        "template literals with substitutions are not supported",
                    ));
                }
                let cooked = t.quasis[0].value.cooked.as_deref().unwrap_or_default();
                let dst = self.new_temp();
                self.load_str(dst, cooked, t.span)?;
                Ok(dst)
            }
            Expression::BigIntLiteral(b) => {
                Err(self.err(b.span, "BigInt literals are not supported"))
            }
            Expression::RegExpLiteral(r) => {
                Err(self.err(r.span, "RegExp literals are not supported"))
            }
            Expression::Identifier(id) => {
                self.read_identifier(id.name.as_str(), id.reference_id.get(), id.span)
            }
            Expression::ThisExpression(t) => {
                let dst = self.new_temp();
                let home = self.comp.plans.this_home(self.unit);
                if home == self.unit {
                    self.move_reg(dst, REG_THIS, t.span);
                } else {
                    let plan = &self.comp.plans.units[home];
                    let slot = plan.this_slot.expect("`this` slot planned");
                    let depth = self.comp.plans.env_depth_between(self.unit, home);
                    self.emit_get_env(dst, depth, slot, t.span);
                }
                Ok(dst)
            }
            Expression::ArrayExpression(arr) => self.array_literal(arr.elements.iter(), arr.span),
            Expression::ObjectExpression(o) => self.object_literal(o),
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            Expression::SequenceExpression(sq) => {
                let mut last = None;
                for x in &sq.expressions {
                    last = Some(self.expr(x)?);
                }
                match last {
                    Some(r) => Ok(r),
                    None => {
                        let dst = self.new_temp();
                        self.load_undefined(dst, sq.span);
                        Ok(dst)
                    }
                }
            }
            Expression::UnaryExpression(u) => self.unary(u.operator, &u.argument, u.span),
            Expression::UpdateExpression(u) => {
                self.update(u.operator, u.prefix, &u.argument, u.span)
            }
            Expression::BinaryExpression(b) => self.binary(b.operator, &b.left, &b.right, b.span),
            Expression::LogicalExpression(l) => self.logical(l.operator, &l.left, &l.right, l.span),
            Expression::AssignmentExpression(a) => {
                self.assign(a.operator, &a.left, &a.right, a.span)
            }
            Expression::ConditionalExpression(c) => {
                let cond = self.expr(&c.test)?;
                let dst = self.new_temp();
                let else_l = self.label();
                let end_l = self.label();
                self.emit_jump(Opcode::JumpIfFalse, cond, else_l);
                let then_r = self.expr(&c.consequent)?;
                self.move_reg(dst, then_r, c.span);
                self.emit_jump(Opcode::Jump, 0, end_l);
                self.bind(else_l);
                let else_r = self.expr(&c.alternate)?;
                self.move_reg(dst, else_r, c.span);
                self.bind(end_l);
                Ok(dst)
            }
            Expression::CallExpression(c) => self.call(c),
            Expression::StaticMemberExpression(s) => {
                if s.optional {
                    return Err(self.err(s.span, "optional chaining is not supported"));
                }
                let obj = self.expr(&s.object)?;
                let key = self.new_temp();
                self.load_str(key, s.property.name.as_str(), s.property.span)?;
                let dst = self.new_temp();
                self.emit_spanned(
                    v12_bytecode::Instr::new(Opcode::GetProperty, dst, obj, key),
                    s.span,
                );
                Ok(dst)
            }
            Expression::ComputedMemberExpression(c) => {
                if c.optional {
                    return Err(self.err(c.span, "optional chaining is not supported"));
                }
                let obj = self.expr(&c.object)?;
                let key = self.expr(&c.expression)?;
                let dst = self.new_temp();
                self.emit_spanned(
                    v12_bytecode::Instr::new(Opcode::GetProperty, dst, obj, key),
                    c.span,
                );
                Ok(dst)
            }
            Expression::PrivateFieldExpression(p) => {
                Err(self.err(p.span, "private fields are not supported"))
            }
            Expression::FunctionExpression(f) => {
                let dst = self.new_temp();
                self.closure_fn(dst, f)?;
                Ok(dst)
            }
            Expression::ArrowFunctionExpression(a) => {
                let dst = self.new_temp();
                self.closure_arrow(dst, a)?;
                Ok(dst)
            }
            other => Err(self.err(other.span(), "unsupported expression")),
        }
    }

    /// Compiles `e` and forces the result into `forced`.
    pub fn expr_into(&mut self, e: &Expression<'_>, forced: u8) -> Res<()> {
        let r = self.expr(e)?;
        if r != forced {
            self.move_reg(forced, r, e.span());
        }
        Ok(())
    }

    // -- identifiers ---------------------------------------------------------

    fn read_identifier(
        &mut self,
        name: &str,
        rid: Option<oxc_semantic::ReferenceId>,
        span: Span,
    ) -> Res<u8> {
        if let Some(sym) = self.comp.symbol_of(rid) {
            let dst = self.new_temp();
            self.read_access(self.access(sym), dst, span);
            return Ok(dst);
        }
        // Spec-defined non-writable globals that need no runtime binding.
        match name {
            "undefined" => {
                let dst = self.new_temp();
                self.load_undefined(dst, span);
                Ok(dst)
            }
            "NaN" | "Infinity" => {
                let dst = self.new_temp();
                let v = if name == "NaN" {
                    f64::NAN
                } else {
                    f64::INFINITY
                };
                self.load_const(dst, Const::F64(v), span)?;
                Ok(dst)
            }
            other => Err(self.err(
                span,
                format!("reference to an unbound (global) variable `{other}` is not supported"),
            )),
        }
    }

    /// Reads storage into `dst`.
    pub fn read_access(&mut self, access: VarAccess, dst: u8, span: Span) {
        match access {
            VarAccess::Reg(r) => self.move_reg(dst, r, span),
            VarAccess::Env { depth, slot } => self.emit_get_env(dst, depth, slot, span),
        }
    }

    /// Writes `src` into storage.
    pub fn store_access(&mut self, access: VarAccess, src: u8, span: Span) {
        match access {
            VarAccess::Reg(r) => {
                if r != src {
                    self.move_reg(r, src, span);
                }
            }
            VarAccess::Env { depth, slot } => self.emit_set_env(depth, slot, src, span),
        }
    }

    // -- literals -------------------------------------------------------------

    pub fn load_number(&mut self, dst: u8, v: f64, span: Span) -> Res<()> {
        if v == 0.0 && v.is_sign_negative() {
            // Preserve -0.0: integer loads cannot represent the sign.
            return self.load_const(dst, Const::F64(v), span);
        }
        if v.is_finite() && v.trunc() == v && (v as i64) as f64 == v {
            self.load_int(dst, v as i64, span);
            Ok(())
        } else {
            self.load_const(dst, Const::F64(v), span)
        }
    }

    fn array_literal<'x>(
        &mut self,
        elements: impl Iterator<Item = &'x ArrayExpressionElement<'x>>,
        span: Span,
    ) -> Res<u8> {
        let elems: Vec<_> = elements.collect();
        let n = u8::try_from(elems.len())
            .map_err(|_| self.err(span, "array literals above 255 elements are not supported"))?;
        let base = self.new_temps(n);
        for (i, el) in elems.into_iter().enumerate() {
            let slot = base + i as u8;
            match el {
                ArrayExpressionElement::SpreadElement(_) => {
                    return Err(self.err(el.span(), "spread elements are not supported"));
                }
                ArrayExpressionElement::Elision(_) => self.load_undefined(slot, span),
                _ => {
                    let Some(x) = el.as_expression() else {
                        return Err(self.err(el.span(), "unsupported array element"));
                    };
                    self.expr_into(x, slot)?;
                }
            }
        }
        let dst = self.new_temp();
        self.emit_spanned(
            v12_bytecode::Instr::new(Opcode::NewArray, dst, base, n),
            span,
        );
        Ok(dst)
    }

    fn object_literal(&mut self, o: &oxc_ast::ast::ObjectExpression<'_>) -> Res<u8> {
        let dst = self.new_temp();
        self.emit_spanned(
            v12_bytecode::Instr::new(Opcode::NewObject, dst, 0, 0),
            o.span,
        );
        for prop_kind in &o.properties {
            match prop_kind {
                ObjectPropertyKind::SpreadProperty(s) => {
                    return Err(self.err(s.span, "object spread is not supported"));
                }
                ObjectPropertyKind::ObjectProperty(p) => self.object_prop(dst, p)?,
            }
        }
        Ok(dst)
    }

    fn object_prop(&mut self, obj: u8, p: &ObjectProperty<'_>) -> Res<()> {
        if p.method || p.kind != oxc_ast::ast::PropertyKind::Init {
            return Err(self.err(p.span, "object methods / accessors are not supported"));
        }
        let key = self.property_key(&p.key)?;
        let val = self.expr(&p.value)?;
        self.emit_spanned(
            v12_bytecode::Instr::new(Opcode::SetProperty, obj, key, val),
            p.span,
        );
        Ok(())
    }

    /// Materializes a property key string; `{x}` shorthand keys come straight
    /// from the identifier name.
    pub(crate) fn property_key(&mut self, key: &PropertyKey<'_>) -> Res<u8> {
        let Some(text) = static_key_text(key) else {
            return Err(self.err(
                key.span(),
                "computed / private property keys are not supported",
            ));
        };
        let dst = self.new_temp();
        self.load_str(dst, &text, key.span())?;
        Ok(dst)
    }

    // -- operators ---------------------------------------------------------------

    fn unary(&mut self, op: UnaryOperator, arg: &Expression<'_>, span: Span) -> Res<u8> {
        let opcode = match op {
            UnaryOperator::UnaryNegation => Opcode::Neg,
            UnaryOperator::LogicalNot => Opcode::Not,
            UnaryOperator::BitwiseNot => Opcode::BitNot,
            UnaryOperator::Typeof => return self.typeof_(arg, span),
            UnaryOperator::Void => {
                let _ = self.expr(arg)?; // side effects only
                let dst = self.new_temp();
                self.load_undefined(dst, span);
                return Ok(dst);
            }
            UnaryOperator::UnaryPlus => {
                return Err(self.err(span, "unary `+` is not supported (no ToNumber opcode yet)"));
            }
            UnaryOperator::Delete => return self.delete(arg, span),
        };
        let v = self.expr(arg)?;
        let dst = self.new_temp();
        self.emit_spanned(v12_bytecode::Instr::new(opcode, dst, v, 0), span);
        Ok(dst)
    }

    fn typeof_(&mut self, arg: &Expression<'_>, span: Span) -> Res<u8> {
        // `typeof undeclared` is specified not to throw.
        if let Expression::Identifier(id) = arg
            && self.comp.symbol_of(id.reference_id.get()).is_none()
        {
            let dst = self.new_temp();
            self.load_str(dst, "undefined", span)?;
            return Ok(dst);
        }
        let v = self.expr(arg)?;
        let dst = self.new_temp();
        self.emit_spanned(v12_bytecode::Instr::new(Opcode::TypeOf, dst, v, 0), span);
        Ok(dst)
    }

    fn delete(&mut self, arg: &Expression<'_>, span: Span) -> Res<u8> {
        let Some(m) = arg.as_member_expression() else {
            return Err(self.err(span, "`delete` is only supported on properties"));
        };
        if m.optional() {
            return Err(self.err(span, "optional chaining is not supported"));
        }
        let (obj, key) = self.member_parts(m)?;
        let dst = self.new_temp();
        self.emit_spanned(
            v12_bytecode::Instr::new(Opcode::DeleteProperty, dst, obj, key),
            span,
        );
        Ok(dst)
    }

    fn binary(
        &mut self,
        op: BinaryOperator,
        lhs: &Expression<'_>,
        rhs: &Expression<'_>,
        span: Span,
    ) -> Res<u8> {
        let opcode = match op {
            BinaryOperator::Addition => Opcode::Add,
            BinaryOperator::Subtraction => Opcode::Sub,
            BinaryOperator::Multiplication => Opcode::Mul,
            BinaryOperator::Division => Opcode::Div,
            BinaryOperator::Remainder => Opcode::Mod,
            BinaryOperator::Exponential => Opcode::Pow,
            BinaryOperator::BitwiseAnd => Opcode::BitAnd,
            BinaryOperator::BitwiseOR => Opcode::BitOr,
            BinaryOperator::BitwiseXOR => Opcode::BitXor,
            BinaryOperator::ShiftLeft => Opcode::Shl,
            BinaryOperator::ShiftRight => Opcode::Shr,
            BinaryOperator::ShiftRightZeroFill => Opcode::UShr,
            BinaryOperator::Equality => Opcode::Eq,
            BinaryOperator::Inequality => Opcode::Ne,
            BinaryOperator::StrictEquality => Opcode::StrictEq,
            BinaryOperator::StrictInequality => Opcode::StrictNe,
            BinaryOperator::LessThan => Opcode::Lt,
            BinaryOperator::LessEqualThan => Opcode::Le,
            BinaryOperator::GreaterThan => Opcode::Gt,
            BinaryOperator::GreaterEqualThan => Opcode::Ge,
            BinaryOperator::In => Opcode::In,
            BinaryOperator::Instanceof => Opcode::InstanceOf,
        };
        let l = self.expr(lhs)?;
        let r = self.expr(rhs)?;
        let dst = self.new_temp();
        self.emit_spanned(v12_bytecode::Instr::new(opcode, dst, l, r), span);
        Ok(dst)
    }

    fn logical(
        &mut self,
        op: LogicalOperator,
        lhs: &Expression<'_>,
        rhs: &Expression<'_>,
        span: Span,
    ) -> Res<u8> {
        // The left value's register doubles as the destination: taken branches
        // keep it, fall-through overwrites it with the right value.
        let dst = self.expr(lhs)?;
        let end = self.label();
        match op {
            LogicalOperator::Or => self.emit_jump(Opcode::JumpIfTrue, dst, end),
            LogicalOperator::And => self.emit_jump(Opcode::JumpIfFalse, dst, end),
            LogicalOperator::Coalesce => {
                // Nullish = `undefined` only in this subset: `null` values
                // cannot be produced yet (see `NullLiteral` rejection).
                let t = self.new_temp();
                let undef = self.undef_reg();
                self.emit_spanned(
                    v12_bytecode::Instr::new(Opcode::StrictEq, t, dst, undef),
                    span,
                );
                self.emit_jump(Opcode::JumpIfFalse, t, end);
            }
        }
        let r = self.expr(rhs)?;
        self.move_reg(dst, r, span);
        self.bind(end);
        Ok(dst)
    }

    fn update(
        &mut self,
        op: UpdateOperator,
        prefix: bool,
        target: &oxc_ast::ast::SimpleAssignmentTarget<'_>,
        span: Span,
    ) -> Res<u8> {
        let delta = match op {
            UpdateOperator::Increment => 1i64,
            UpdateOperator::Decrement => -1,
        };
        match target {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let Some(sym) = self.comp.symbol_of(id.reference_id.get()) else {
                    return Err(self.err(
                        id.span,
                        "assignment to an unbound variable is not supported",
                    ));
                };
                let access = self.access(sym);
                let old = self.new_temp();
                self.read_access(access, old, span);
                let one = self.new_temp();
                self.load_int(one, delta, span);
                let new = self.new_temp();
                self.emit_spanned(v12_bytecode::Instr::new(Opcode::Add, new, old, one), span);
                self.store_access(access, new, span);
                Ok(if prefix { new } else { old })
            }
            _ => {
                let Some(m) = target.as_member_expression() else {
                    return Err(self.err(span, "unsupported update target"));
                };
                let (obj, key) = self.member_parts(m)?;
                let old = self.new_temp();
                self.emit_spanned(
                    v12_bytecode::Instr::new(Opcode::GetProperty, old, obj, key),
                    span,
                );
                let one = self.new_temp();
                self.load_int(one, delta, span);
                let new = self.new_temp();
                self.emit_spanned(v12_bytecode::Instr::new(Opcode::Add, new, old, one), span);
                self.emit_spanned(
                    v12_bytecode::Instr::new(Opcode::SetProperty, obj, key, new),
                    span,
                );
                Ok(if prefix { new } else { old })
            }
        }
    }

    fn assign(
        &mut self,
        op: AssignmentOperator,
        left: &AssignmentTarget<'_>,
        right: &Expression<'_>,
        span: Span,
    ) -> Res<u8> {
        let binop = match op {
            AssignmentOperator::Assign => None,
            AssignmentOperator::Addition => Some(Opcode::Add),
            AssignmentOperator::Subtraction => Some(Opcode::Sub),
            AssignmentOperator::Multiplication => Some(Opcode::Mul),
            AssignmentOperator::Division => Some(Opcode::Div),
            AssignmentOperator::Remainder => Some(Opcode::Mod),
            AssignmentOperator::Exponential => Some(Opcode::Pow),
            AssignmentOperator::ShiftLeft => Some(Opcode::Shl),
            AssignmentOperator::ShiftRight => Some(Opcode::Shr),
            AssignmentOperator::ShiftRightZeroFill => Some(Opcode::UShr),
            AssignmentOperator::BitwiseOR => Some(Opcode::BitOr),
            AssignmentOperator::BitwiseXOR => Some(Opcode::BitXor),
            AssignmentOperator::BitwiseAnd => Some(Opcode::BitAnd),
            AssignmentOperator::LogicalAnd
            | AssignmentOperator::LogicalOr
            | AssignmentOperator::LogicalNullish => {
                return Err(self.err(span, "logical assignment operators are not supported"));
            }
        };

        let Some(simple) = left.as_simple_assignment_target() else {
            return Err(self.err(span, "destructuring assignment is not supported"));
        };
        let rhs = self.expr(right)?;

        match simple {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let Some(sym) = self.comp.symbol_of(id.reference_id.get()) else {
                    return Err(self.err(
                        id.span,
                        "assignment to an unbound variable is not supported",
                    ));
                };
                let access = self.access(sym);
                let value = match binop {
                    None => rhs,
                    Some(opcode) => {
                        let cur = self.new_temp();
                        self.read_access(access, cur, span);
                        let out = self.new_temp();
                        self.emit_spanned(v12_bytecode::Instr::new(opcode, out, cur, rhs), span);
                        out
                    }
                };
                self.store_access(access, value, span);
                Ok(value)
            }
            _ => {
                let Some(m) = simple.as_member_expression() else {
                    return Err(self.err(span, "unsupported assignment target"));
                };
                let (obj, key) = self.member_parts(m)?;
                let value = match binop {
                    None => rhs,
                    Some(opcode) => {
                        let cur = self.new_temp();
                        self.emit_spanned(
                            v12_bytecode::Instr::new(Opcode::GetProperty, cur, obj, key),
                            span,
                        );
                        let out = self.new_temp();
                        self.emit_spanned(v12_bytecode::Instr::new(opcode, out, cur, rhs), span);
                        out
                    }
                };
                self.emit_spanned(
                    v12_bytecode::Instr::new(Opcode::SetProperty, obj, key, value),
                    span,
                );
                Ok(value)
            }
        }
    }

    // -- member access ----------------------------------------------------------

    /// Evaluates object + key once (evaluation-order safe for read-modify-write
    /// targets) and returns their registers.
    fn member_parts(&mut self, m: &MemberExpression<'_>) -> Res<(u8, u8)> {
        if m.optional() {
            return Err(self.err(m.span(), "optional chaining is not supported"));
        }
        let obj = self.expr(m.object())?;
        let key = match m {
            MemberExpression::StaticMemberExpression(s) => {
                let k = self.new_temp();
                self.load_str(k, s.property.name.as_str(), s.property.span)?;
                k
            }
            MemberExpression::ComputedMemberExpression(c) => self.expr(&c.expression)?,
            MemberExpression::PrivateFieldExpression(p) => {
                return Err(self.err(p.span, "private fields are not supported"));
            }
        };
        Ok((obj, key))
    }

    // -- calls ---------------------------------------------------------------------

    fn call(&mut self, c: &CallExpression<'_>) -> Res<u8> {
        if c.optional {
            return Err(self.err(c.span, "optional calls are not supported"));
        }
        let argc = u16::try_from(c.arguments.len())
            .map_err(|_| self.err(c.span, "calls above 65535 arguments are not supported"))?;
        if argc > u16::from(u8::MAX - CALL_HEADER_REGS) {
            return Err(self.err(c.span, "calls above 253 arguments are not supported"));
        }
        let argc8 = u8::try_from(argc).expect("argc checked against u8::MAX above");

        // Layout: [callee][this][arg…]; see `CALL_HEADER_REGS`.
        let block = self.new_temps(argc8 + CALL_HEADER_REGS);

        if let Some(m) = c.callee.as_member_expression() {
            // Method call: obj + key evaluate first, `this` = object.
            let (obj, key) = self.member_parts(m)?;
            self.emit_spanned(
                v12_bytecode::Instr::new(Opcode::GetProperty, block, obj, key),
                c.span,
            );
            self.move_reg(block + 1, obj, c.span);
        } else {
            let v = self.expr(&c.callee)?;
            self.move_reg(block, v, c.span);
            self.load_undefined(block + 1, c.span);
        }
        for (i, arg) in c.arguments.iter().enumerate() {
            let Some(x) = arg.as_expression() else {
                return Err(self.err(arg.span(), "spread arguments are not supported"));
            };
            self.expr_into(x, block + 2 + i as u8)?;
        }
        self.emit_call(block, block, argc, c.span);
        Ok(block)
    }

    // -- closures --------------------------------------------------------------------

    /// Emits `Closure` for a nested function, compiling its body first.
    pub(crate) fn closure_fn(&mut self, dst: u8, f: &oxc_ast::ast::Function<'_>) -> Res<()> {
        let idx = self.planned_index(f.span)?;
        crate::unit::compile_unit(self.comp, idx, crate::unit::UnitNode::Fn(f))?;
        self.emit_closure_instr(dst, idx, f.span)
    }

    /// Emits `Closure` for a nested arrow, compiling its body first.
    fn closure_arrow(&mut self, dst: u8, a: &oxc_ast::ast::ArrowFunctionExpression<'_>) -> Res<()> {
        let idx = self.planned_index(a.span)?;
        crate::unit::compile_unit(self.comp, idx, crate::unit::UnitNode::Arrow(a))?;
        self.emit_closure_instr(dst, idx, a.span)
    }

    fn planned_index(&self, span: Span) -> Res<usize> {
        self.comp
            .plans
            .fn_index
            .get(&span)
            .copied()
            .ok_or_else(|| self.err(span, "internal: nested function missing from plans"))
    }

    fn emit_closure_instr(&mut self, dst: u8, idx: usize, span: Span) -> Res<()> {
        let idx8 = u8::try_from(idx)
            .map_err(|_| self.err(span, "programs above 255 functions are not supported"))?;
        self.emit_spanned(
            v12_bytecode::Instr::new(Opcode::Closure, dst, idx8, 0),
            span,
        );
        Ok(())
    }
}

/// Property key text for the static (non-computed) forms we support.
fn static_key_text(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
        PropertyKey::StringLiteral(s) => Some(s.value.to_string()),
        PropertyKey::NumericLiteral(n) => Some(number_to_key(n.value)),
        _ => None,
    }
}

/// Formats a numeric literal as an object property key (ES ToString).
fn number_to_key(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}
