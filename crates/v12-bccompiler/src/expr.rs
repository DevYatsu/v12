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
use v12_bytecode::{Const, Label, Opcode};

use crate::model::{CALL_HEADER_REGS, CompileError, FnCtx, REG_THIS, VarAccess};

type Res<T> = Result<T, CompileError>;

/// One object-literal accessor key: both closures plus the key register,
/// batched until all properties are seen so a `get`/`set` pair shares one
/// `DefineAccessor` (they must land on the same shape transition).
struct AccessorPair {
    /// Static key text, for pairing getter/setter halves.
    text: String,
    /// Register holding the property key.
    key: u16,
    /// Getter body register, when a getter was present.
    getter: Option<u16>,
    /// Setter body register, when a setter was present.
    setter: Option<u16>,
    /// Span of the first (getter or setter) property seen.
    span: Span,
}

impl<'c, 's, 'i, 'a> FnCtx<'c, 's, 'i, 'a> {
    /// Compiles `e`, returning the register that holds its value.
    pub fn expr(&mut self, e: &Expression<'_>) -> Res<u16> {
        match e {
            Expression::BooleanLiteral(b) => {
                let dst = self.new_temp();
                self.load_bool(dst, b.value, b.span);
                Ok(dst)
            }
            Expression::NullLiteral(n) => {
                let dst = self.new_temp();
                self.load_const(dst, Const::Null, n.span)?;
                Ok(dst)
            }
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
            Expression::TemplateLiteral(t) => self.template_literal(t),
            Expression::BigIntLiteral(b) => {
                // Minimal BigInt support: strip separators and suffix, try u64,
                // otherwise preserve as BigIntId string. Falls back to Number
                // only if parsing fails — never the "not supported" error.
                let raw_opt = b.raw.as_deref().unwrap_or("");
                let stripped = raw_opt.trim_end_matches('n').replace('_', "");
                let stripped = if stripped.is_empty() { b.value.to_string() } else { stripped };
                let body = stripped.as_str();
                let dst = self.new_temp();
                // Try hex/binary/octal prefixes via u64
                let parsed_u64 = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
                    u64::from_str_radix(hex, 16).ok()
                } else if let Some(bin) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
                    u64::from_str_radix(bin, 2).ok()
                } else if let Some(oct) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
                    u64::from_str_radix(oct, 8).ok()
                } else {
                    // decimal — allow leading sign handled by parser? BigInt is unsigned with optional sign stripped earlier
                    body.parse::<u64>().ok()
                };
                if let Some(v) = parsed_u64 {
                    self.load_const(dst, Const::BigU64(v), b.span)?;
                } else {
                    // Preserve original decimal text (without suffix/underscores) as BigIntId
                    let id = crate::model::str_id_of(self.comp.strings.get_or_intern(&stripped));
                    self.load_const(dst, Const::BigIntId(id), b.span)?;
                }
                Ok(dst)
            }
            Expression::RegExpLiteral(r) => {
                // `/pattern/flags` → `RegExp("pattern", "flags")`: read the
                // `RegExp` global (a native constructor in the realm) and
                // call it with the pattern text and canonical flag string.
                use oxc_ast::ast::RegExpFlags;
                let pattern = r.regex.pattern.text.as_str();
                let mut flags = String::with_capacity(8);
                let f = r.regex.flags;
                if f.contains(RegExpFlags::D) {
                    flags.push('d');
                }
                if f.contains(RegExpFlags::G) {
                    flags.push('g');
                }
                if f.contains(RegExpFlags::I) {
                    flags.push('i');
                }
                if f.contains(RegExpFlags::M) {
                    flags.push('m');
                }
                if f.contains(RegExpFlags::S) {
                    flags.push('s');
                }
                if f.contains(RegExpFlags::U) {
                    flags.push('u');
                }
                if f.contains(RegExpFlags::V) {
                    flags.push('v');
                }
                if f.contains(RegExpFlags::Y) {
                    flags.push('y');
                }
                let dst = self.new_temp();
                let gid = crate::model::str_id_of(self.comp.strings.get_or_intern("RegExp"));
                self.emit_get_global(dst, gid, r.span);
                let block = self.new_temps(CALL_HEADER_REGS + 2);
                self.move_reg(block, dst, r.span);
                self.load_undefined(block + 1, r.span);
                self.load_str(block + 2, pattern, r.span)?;
                self.load_str(block + 3, &flags, r.span)?;
                self.emit_call(block, block, 2, r.span);
                Ok(block)
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
                    if let Some(slot) = plan.this_slot {
                        let depth = self.comp.plans.env_depth_between(self.unit, home);
                        self.emit_get_env(dst, depth, slot, t.span);
                    } else {
                        // Home did not allocate a `this` env slot (e.g. class
                        // field initializer `this` not walked during collect).
                        // Fall back to direct `this` register — the arrow
                        // captures the same frame's REG_THIS.
                        self.move_reg(dst, REG_THIS, t.span);
                    }
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
            Expression::ChainExpression(c) => self.chain_expr(c),
            Expression::NewExpression(n) => self.new_expr(n),
            Expression::StaticMemberExpression(s) => {
                // Standalone member `obj.prop` (chains route through
                // `chain_expr`, wrapped by oxc `ChainExpression`). The base
                // (`obj`) may be a global (`console`) via `GetGlobal`
                // (PropKey/Spur), the property (`log`) is a string literal
                // via `LoadConst` (Str32/ConstantPool). Even though both
                // string ids originate from the same `Rodeo`, the immediates
                // occupy distinct namespaces, so the `GetProperty` key must
                // be `Str32("log")`, not the `Spur` for `"console"`.
                let obj = self.expr(&s.object)?;
                let key = self.new_temp();
                self.load_str(key, s.property.name.as_str(), s.property.span)?;
                if s.optional {
                    return self.optional_deref(s.span, obj, key);
                }
                let dst = self.new_temp();
                self.emit_reg3(Opcode::GetProperty, dst, obj, key, s.span);
                Ok(dst)
            }
            Expression::ComputedMemberExpression(c) => {
                // Bucket 4: evaluate key expr into temp, then base. Runtime
                // `GetProperty` does ToPropertyKey via `to_key`.
                let key = self.expr(&c.expression)?;
                let obj = self.expr(&c.object)?;
                if c.optional {
                    return self.optional_deref(c.span, obj, key);
                }
                let dst = self.new_temp();
                self.emit_reg3(Opcode::GetProperty, dst, obj, key, c.span);
                Ok(dst)
            }
            Expression::PrivateFieldExpression(p) => {
                let obj = self.expr(&p.object)?;
                let dst = self.new_temp();
                let name_id = crate::model::str_id_of(self.comp.strings.get_or_intern(&format!("#{}", p.field.name)));
                // class_id 0 for minimal brand check
                let words = v12_bytecode::WideOp::GetPrivateW { dst, obj, class_id: 0, name_id }.encode();
                self.emit_words(words, p.span);
                Ok(dst)
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
            Expression::YieldExpression(y) => {
                if y.delegate {
                    let iterable = self.expr(y.argument.as_ref().unwrap())?;
                    // Generic iterator protocol with array fallback.
                    let sym_key = self.new_temp();
                    self.load_str(sym_key, "Symbol.iterator", y.span)?;
                    let method = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, method, iterable, sym_key, y.span);
                    let iter = self.new_temp();
                    let ret = self.new_temp();
                    let has_iter = self.label();
                    let no_iter = self.label();
                    let iter_ready = self.label();
                    let done_label = self.label();
                    self.emit_jump(Opcode::JumpIfTrue, method, has_iter);
                    self.bind(no_iter);
                    let probe_key = self.new_temp();
                    self.load_str(probe_key, "next", y.span)?;
                    let probe_next = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, probe_next, iterable, probe_key, y.span);
                    let array_fallback = self.label();
                    self.emit_jump(Opcode::JumpIfFalse, probe_next, array_fallback);
                    self.move_reg(iter, iterable, y.span);
                    self.emit_jump(Opcode::Jump, 0, iter_ready);
                    self.bind(array_fallback);
                    {
                        let idx = self.new_temp();
                        self.load_int(idx, 0, y.span);
                        let len_key = self.new_temp();
                        self.load_str(len_key, "length", y.span)?;
                        let arr_loop_start = self.label();
                        let arr_loop_end = self.label();
                        self.bind(arr_loop_start);
                        let len = self.new_temp();
                        self.emit_reg3(Opcode::GetProperty, len, iterable, len_key, y.span);
                        let cond = self.new_temp();
                        self.emit_reg3(Opcode::Lt, cond, idx, len, y.span);
                        self.emit_jump(Opcode::JumpIfFalse, cond, arr_loop_end);
                        let elem = self.new_temp();
                        self.emit_reg3(Opcode::GetProperty, elem, iterable, idx, y.span);
                        let ydst = self.new_temp();
                        self.move_reg(ydst, elem, y.span);
                        self.emit_reg3(Opcode::SuspendYield, ydst, 0, 0, y.span);
                        let one = self.new_temp();
                        self.load_int(one, 1, y.span);
                        let nxt = self.new_temp();
                        self.emit_reg3(Opcode::Add, nxt, idx, one, y.span);
                        self.move_reg(idx, nxt, y.span);
                        self.emit_jump(Opcode::Jump, 0, arr_loop_start);
                        self.bind(arr_loop_end);
                        self.load_undefined(ret, y.span);
                        self.emit_jump(Opcode::Jump, 0, done_label);
                    }
                    self.bind(has_iter);
                    {
                        let block = self.new_temps(CALL_HEADER_REGS);
                        self.move_reg(block, method, y.span);
                        self.move_reg(block + 1, iterable, y.span);
                        self.emit_call(block, block, 0, y.span);
                        self.move_reg(iter, block, y.span);
                    }
                    self.bind(iter_ready);
                    let next_key = self.new_temp();
                    self.load_str(next_key, "next", y.span)?;
                    let done_key = self.new_temp();
                    self.load_str(done_key, "done", y.span)?;
                    let value_key = self.new_temp();
                    self.load_str(value_key, "value", y.span)?;
                    let loop_start = self.label();
                    let loop_end = self.label();
                    let result = self.new_temp();
                    self.load_undefined(result, y.span);
                    self.bind(loop_start);
                    {
                        let next_fn = self.new_temp();
                        self.emit_reg3(Opcode::GetProperty, next_fn, iter, next_key, y.span);
                        let block = self.new_temps(CALL_HEADER_REGS);
                        self.move_reg(block, next_fn, y.span);
                        self.move_reg(block + 1, iter, y.span);
                        self.emit_call(block, block, 0, y.span);
                        self.move_reg(result, block, y.span);
                    }
                    let done_val = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, done_val, result, done_key, y.span);
                    self.emit_jump(Opcode::JumpIfTrue, done_val, loop_end);
                    let yielded = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, yielded, result, value_key, y.span);
                    let ydst = self.new_temp();
                    self.move_reg(ydst, yielded, y.span);
                    self.emit_reg3(Opcode::SuspendYield, ydst, 0, 0, y.span);
                    self.emit_jump(Opcode::Jump, 0, loop_start);
                    self.bind(loop_end);
                    self.emit_reg3(Opcode::GetProperty, ret, result, value_key, y.span);
                    self.bind(done_label);
                    return Ok(ret);
                }
                let arg = if let Some(arg) = &y.argument {
                    self.expr(arg)?
                } else {
                    let r = self.new_temp();
                    self.load_undefined(r, y.span);
                    r
                };
                let dst = self.new_temp();
                self.move_reg(dst, arg, y.span);
                self.emit_reg3(Opcode::SuspendYield, dst, 0, 0, y.span);
                Ok(dst)
            }
            Expression::AwaitExpression(a) => {
                let arg = self.expr(&a.argument)?;
                let dst = self.new_temp();
                self.emit_reg3(Opcode::Await, dst, arg, 0, a.span);
                Ok(dst)
            }
            Expression::ClassExpression(c) => crate::class::class_expression(self, c, false),
            Expression::TaggedTemplateExpression(t) => {
                // Minimal: evaluate as tag`template` -> call tag with template strings.
                // Build array of cooked strings, pass as first arg, then expressions.
                let tag = self.expr(&t.tag)?;
                let quasi = &t.quasi;
                let n = quasi.quasis.len();
                let arr_base = self.new_temps(n as u16);
                for (i, q) in quasi.quasis.iter().enumerate() {
                    let raw = q.value.cooked.as_ref().map(|s| s.as_str()).unwrap_or("");
                    self.load_str(arr_base + i as u16, raw, q.span)?;
                }
                let arr = self.new_temp();
                self.emit_new_array(arr, arr_base, n as u8, t.span);
                let argc = (1 + quasi.expressions.len()) as u16;
                let block = self.new_temps(CALL_HEADER_REGS + argc);
                self.move_reg(block, tag, t.span);
                self.load_undefined(block + 1, t.span);
                self.move_reg(block + 2, arr, t.span);
                for (i, e) in quasi.expressions.iter().enumerate() {
                    let r = self.expr(e)?;
                    self.move_reg(block + 3 + i as u16, r, t.span);
                }
                self.emit_call(block, block, argc, t.span);
                let dst = self.new_temp();
                self.move_reg(dst, block, t.span);
                Ok(dst)
            }
            Expression::ImportExpression(i) => {
                // Dynamic import: desugar to a call to the native import helper
                // import(source) -> native_import(source)
                let src = self.expr(&i.source)?;
                let dst = self.new_temp();
                let block = self.new_temps(crate::model::CALL_HEADER_REGS + 1);
                let callee = block;
                self.emit_closure(callee, crate::model::NATIVE_IMPORT_INDEX, i.span);
                self.load_undefined(callee + 1, i.span);
                self.move_reg(callee + 2, src, i.span);
                self.emit_call(callee, callee, 1, i.span);
                self.move_reg(dst, callee, i.span);
                Ok(dst)
            }
            Expression::Super(x) => self.super_expr(x.span),
            Expression::ImportMeta(x) => Err(self.err(x.span, "`import.meta` is not supported")),
            Expression::NewTarget(x) => {
                let dst = self.new_temp();
                self.emit_reg2(Opcode::GetNewTarget, dst, 0, x.span);
                Ok(dst)
            }
            Expression::PrivateInExpression(x) => {
                let obj = self.expr(&x.right)?;
                let dst = self.new_temp();
                let name_id = crate::model::str_id_of(self.comp.strings.get_or_intern(&format!("#{}", x.left.name)));
                let words = v12_bytecode::WideOp::HasPrivateW { dst, obj, class_id: 0, name_id }.encode();
                self.emit_words(words, x.span);
                Ok(dst)
            }
            // TypeScript-only forms never reach here through `script()` /
            // `mjs()` parsing; named rejection keeps direct AST callers safe.
            Expression::TSAsExpression(_)
            | Expression::TSSatisfiesExpression(_)
            | Expression::TSTypeAssertion(_)
            | Expression::TSNonNullExpression(_)
            | Expression::TSInstantiationExpression(_) => Err(self.err(
                e.span(),
                "TypeScript assertion expressions are not supported",
            )),
            Expression::JSXElement(x) => Err(self.err(x.span, "JSX expressions are not supported")),
            Expression::JSXFragment(x) => {
                Err(self.err(x.span, "JSX expressions are not supported"))
            }
            Expression::V8IntrinsicExpression(x) => {
                Err(self.err(x.span, "V8 intrinsic calls (`%name`) are not supported"))
            }
        }
    }

    /// Compiles `e` and forces the result into `forced`.
    pub fn expr_into(&mut self, e: &Expression<'_>, forced: u16) -> Res<()> {
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
    ) -> Res<u16> {
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
            other => {
                let dst = self.new_temp();
                let id = crate::model::str_id_of(self.comp.strings.get_or_intern(other));
                self.emit_get_global(dst, id, span);
                Ok(dst)
            }
        }
    }

    /// Reads storage into `dst`.
    pub fn read_access(&mut self, access: VarAccess, dst: u16, span: Span) {
        match access {
            VarAccess::Reg(r) => self.move_reg(dst, r, span),
            VarAccess::Env { depth, slot } => self.emit_get_env(dst, depth, slot, span),
            VarAccess::Global { sym } => {
                let name = self.comp.scoping.symbol_name(sym);
                let name_id = crate::model::str_id_of(self.comp.strings.get_or_intern(name));
                self.emit_get_global(dst, name_id, span);
            }
        }
    }

    /// Writes `src` into storage.
    pub fn store_access(&mut self, access: VarAccess, src: u16, span: Span) {
        match access {
            VarAccess::Reg(r) => {
                if r != src {
                    self.move_reg(r, src, span);
                }
            }
            VarAccess::Env { depth, slot } => self.emit_set_env(depth, slot, src, span),
            VarAccess::Global { sym } => {
                let name = self.comp.scoping.symbol_name(sym);
                let name_id = crate::model::str_id_of(self.comp.strings.get_or_intern(name));
                self.emit_set_global(name_id, src, span);
            }
        }
    }

    // -- literals -------------------------------------------------------------

    pub fn load_number(&mut self, dst: u16, v: f64, span: Span) -> Res<()> {
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
    ) -> Res<u16> {
        let elems: Vec<_> = elements.collect();
        let has_spread = elems
            .iter()
            .any(|el| matches!(el, ArrayExpressionElement::SpreadElement(_)));
        if !has_spread {
            let n = u8::try_from(elems.len()).map_err(|_| {
                self.err(span, "array literals above 255 elements are not supported")
            })?;
            let base = self.new_temps(u16::from(n));
            for (i, el) in elems.into_iter().enumerate() {
                let slot = base + i as u16;
                match el {
                    ArrayExpressionElement::SpreadElement(_) => unreachable!(),
                    ArrayExpressionElement::Elision(_) => self.load_undefined(slot, span),
                    _ => {
                        // Non-spread, non-elision elements are always
                        // expressions; the guard only protects against future
                        // oxc variants.
                        let Some(x) = el.as_expression() else {
                            return Err(self.err(el.span(), "array element must be an expression"));
                        };
                        self.expr_into(x, slot)?;
                    }
                }
            }
            let dst = self.new_temp();
            self.emit_new_array(dst, base, n, span);
            return Ok(dst);
        }
        // Spread path: build array incrementally.
        let dst = self.new_temp();
        // Empty array
        self.emit_new_array(dst, self.undef_reg(), 0, span);
        for el in elems {
            match el {
                ArrayExpressionElement::SpreadElement(s) => {
                    let src = self.expr(&s.argument)?;
                    self.emit_reg3(Opcode::CheckIsArray, src, 0, 0, s.span);
                    self.emit_reg3(Opcode::ArrayAppend, dst, src, 0, s.span);
                }
                ArrayExpressionElement::Elision(_) => {
                    // Hole: append undefined as hole approximation — create single hole via SetProperty with hole?
                    // For v1 we append undefined (tests treat hole as undefined at index).
                    let undef = self.new_temp();
                    self.load_undefined(undef, span);
                    let len_key = self.new_temp();
                    self.load_str(len_key, "length", span)?;
                    let len = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, len, dst, len_key, span);
                    self.emit_reg3(Opcode::SetProperty, dst, len, undef, span);
                }
                _ => {
                    let Some(x) = el.as_expression() else {
                        return Err(self.err(el.span(), "array element must be an expression"));
                    };
                    let val = self.expr(x)?;
                    let len_key = self.new_temp();
                    self.load_str(len_key, "length", span)?;
                    let len = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, len, dst, len_key, span);
                    self.emit_reg3(Opcode::SetProperty, dst, len, val, span);
                }
            }
        }
        Ok(dst)
    }

    fn object_literal(&mut self, o: &oxc_ast::ast::ObjectExpression<'_>) -> Res<u16> {
        let dst = self.new_temp();
        self.emit_reg3(Opcode::NewObject, dst, 0, 0, o.span);
        // Collect accessor halves by key so `{ get x(){}, set x(){} }` emits
        // ONE `DefineAccessor` carrying both closures (the descriptor needs
        // both halves on the same shape transition).
        let mut accessors: Vec<AccessorPair> = Vec::new();
        for prop_kind in &o.properties {
            match prop_kind {
                ObjectPropertyKind::SpreadProperty(s) => {
                    // `{...src}`: evaluate src, merge its enumerable own
                    // properties onto the literal's object (later writes win).
                    let src = self.expr(&s.argument)?;
                    self.emit_reg3(Opcode::MergeObject, 0, dst, src, s.span);
                }
                ObjectPropertyKind::ObjectProperty(p) => {
                    if p.kind == oxc_ast::ast::PropertyKind::Get
                        || p.kind == oxc_ast::ast::PropertyKind::Set
                    {
                        let (text, key, body) = self.accessor_parts(p)?;
                        let is_get = p.kind == oxc_ast::ast::PropertyKind::Get;
                        let entry = accessors.iter_mut().find(|e| e.text == text);
                        if let Some(entry) = entry {
                            if is_get {
                                entry.getter = Some(body);
                            } else {
                                entry.setter = Some(body);
                            }
                        } else {
                            accessors.push(AccessorPair {
                                text,
                                key,
                                getter: is_get.then_some(body),
                                setter: (!is_get).then_some(body),
                                span: p.span,
                            });
                        }
                    } else {
                        self.object_prop(dst, p)?;
                    }
                }
            }
        }
        for AccessorPair {
            text: _,
            key,
            getter,
            setter,
            span,
        } in accessors
        {
            // Pair registers: r[pair] = getter (or undefined), r[pair+1] = setter.
            // Allocate both as a contiguous block so `pair + 1` is in-range.
            let pair = self.new_temps(2);
            match (getter, setter) {
                (Some(g), Some(s)) => {
                    self.emit_reg2(Opcode::Move, pair, g, span);
                    self.emit_reg2(Opcode::Move, pair + 1, s, span);
                }
                (Some(g), None) => {
                    self.emit_reg2(Opcode::Move, pair, g, span);
                    self.load_undefined(pair + 1, span);
                }
                (None, Some(s)) => {
                    self.load_undefined(pair, span);
                    self.emit_reg2(Opcode::Move, pair + 1, s, span);
                }
                (None, None) => unreachable!("accessor with neither half"),
            }
            self.emit_reg3(Opcode::DefineAccessor, dst, key, pair, span);
        }
        Ok(dst)
    }

    fn object_prop(&mut self, obj: u16, p: &ObjectProperty<'_>) -> Res<()> {
        // Accessors are batched in `object_literal`; reaching this path with
        // a get/set kind is a compiler bug.
        debug_assert!(
            p.kind == oxc_ast::ast::PropertyKind::Init,
            "accessor property reached object_prop"
        );
        // Computed property keys (`{[expr]: value}`) evaluate the key
        // expression into a temp (ToPropertyKey via runtime `to_key`) and
        // use dynamic `SetProperty`. Static keys remain interned strings via
        // `LoadConst` (Str32/pool) — distinct from any `GetGlobal` (PropKey)
        // for the same identifier text.
        let key = if p.computed {
            let Some(expr) = p.key.as_expression() else {
                return Err(self.err(p.key.span(), "computed property key must be an expression"));
            };
            // Evaluate key first, then value — single temp for key, no extra
            // allocation beyond the key's own evaluation.
            self.expr(expr)?
        } else {
            self.property_key(&p.key)?
        };
        // `{m(x){…}}` ≡ `{m: function m(x){…}}` — method shorthand reuses
        // the function-expression closure path, so `this` flows through the
        // standard call ABI when invoked as `obj.m(…)`.
        let val = match &p.value {
            Expression::FunctionExpression(f) if p.method => {
                let r = self.new_temp();
                self.closure_fn(r, f)?;
                r
            }
            other => self.expr(other)?,
        };
        self.emit_reg3(Opcode::SetProperty, obj, key, val, p.span);
        Ok(())
    }

    /// Object literal accessor: compile the body closure, returning the static
    /// key text (for get/set pair dedup), the key register, and the body
    /// register. Both halves of a get/set pair are batched in
    /// [`Self::object_literal`] so they share one shape transition.
    fn accessor_parts(&mut self, p: &ObjectProperty<'_>) -> Res<(String, u16, u16)> {
        let key = if p.computed {
            let Some(expr) = p.key.as_expression() else {
                return Err(self.err(p.key.span(), "computed property key must be an expression"));
            };
            self.expr(expr)?
        } else {
            self.property_key(&p.key)?
        };
        let text = static_key_text(&p.key)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let body = match &p.value {
            Expression::FunctionExpression(f) => {
                let r = self.new_temp();
                self.closure_fn(r, f)?;
                r
            }
            _ => {
                return Err(self.err(p.span, "accessor value must be a function expression"));
            }
        };
        Ok((text, key, body))
    }

    /// Materializes a property key string; `{x}` shorthand keys come straight
    /// from the identifier name.
    pub(crate) fn property_key(&mut self, key: &PropertyKey<'_>) -> Res<u16> {
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

    fn unary(&mut self, op: UnaryOperator, arg: &Expression<'_>, span: Span) -> Res<u16> {
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
                // ES ToNumber; the interpreter's `ToNumber` opcode boxes the result.
                let v = self.expr(arg)?;
                let dst = self.new_temp();
                self.emit_reg2(Opcode::ToNumber, dst, v, span);
                return Ok(dst);
            }
            UnaryOperator::Delete => return self.delete(arg, span),
        };
        let v = self.expr(arg)?;
        let dst = self.new_temp();
        self.emit_reg2(opcode, dst, v, span);
        Ok(dst)
    }

    fn typeof_(&mut self, arg: &Expression<'_>, span: Span) -> Res<u16> {
        // `typeof undeclared` is specified not to throw. For v1 `GetGlobal`
        // on a missing global already yields `undefined`, so the early return
        // is just an optimisation. It must not fire for well-known globals
        // like `Object`/`Array` where `typeof Object` should be `"function"` /
        // `"object"` rather than `"undefined"`. The same applies to the
        // non-writable globals `undefined`/`NaN`/`Infinity` which have
        // dedicated value materialisation in `read_identifier`.
        if let Expression::Identifier(id) = arg
            && self.comp.symbol_of(id.reference_id.get()).is_none()
        {
            let name = id.name.as_str();
            let is_global_intrinsic = crate::model::GLOBAL_INTRINSICS.contains(&name);
            let is_special = matches!(name, "undefined" | "NaN" | "Infinity");
            if !is_global_intrinsic && !is_special {
                let dst = self.new_temp();
                self.load_str(dst, "undefined", span)?;
                return Ok(dst);
            }
        }
        let v = self.expr(arg)?;
        let dst = self.new_temp();
        self.emit_reg3(Opcode::TypeOf, dst, v, 0, span);
        Ok(dst)
    }

    fn delete(&mut self, arg: &Expression<'_>, span: Span) -> Res<u16> {
        // `delete unqualifiedIdentifier`: a SyntaxError early error in strict
        // mode; in sloppy mode Annex B says it evaluates to `true` (and does
        // nothing — the binding is not deleted).
        if !arg.is_member_expression() {
            if self.comp.plans.units[self.unit].is_strict {
                return Err(self.err(
                    span,
                    "SyntaxError: Delete of an unqualified identifier in strict mode",
                ));
            }
            let dst = self.new_temp();
            self.load_bool(dst, true, span);
            return Ok(dst);
        }
        let Some(m) = arg.as_member_expression() else {
            return Err(self.err(span, "`delete` is only supported on properties"));
        };
        if m.optional() {
            return Err(self.err(span, "optional chaining is not supported"));
        }
        let (obj, key) = self.member_parts(m)?;
        let dst = self.new_temp();
        self.emit_reg3(Opcode::DeleteProperty, dst, obj, key, span);
        Ok(dst)
    }

    fn binary(
        &mut self,
        op: BinaryOperator,
        lhs: &Expression<'_>,
        rhs: &Expression<'_>,
        span: Span,
    ) -> Res<u16> {
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
        self.emit_reg3(opcode, dst, l, r, span);
        Ok(dst)
    }

    fn logical(
        &mut self,
        op: LogicalOperator,
        lhs: &Expression<'_>,
        rhs: &Expression<'_>,
        span: Span,
    ) -> Res<u16> {
        // The left value's register doubles as the destination: taken branches
        // keep it, fall-through overwrites it with the right value.
        let dst = self.expr(lhs)?;
        let end = self.label();
        match op {
            LogicalOperator::Or => self.emit_jump(Opcode::JumpIfTrue, dst, end),
            LogicalOperator::And => self.emit_jump(Opcode::JumpIfFalse, dst, end),
            LogicalOperator::Coalesce => {
                // Nullish = `null` or `undefined` (ES `x ?? y`). Loose
                // `Eq` against `null` is true exactly for those two values,
                // so a single `Eq` suffices (covers `null == undefined`).
                let null_reg = self.new_temp();
                self.load_const(null_reg, Const::Null, span)?;
                let t = self.new_temp();
                self.emit_reg3(Opcode::Eq, t, dst, null_reg, span);
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
    ) -> Res<u16> {
        let delta = match op {
            UpdateOperator::Increment => 1i64,
            UpdateOperator::Decrement => -1,
        };
        match target {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let Some(sym) = self.comp.symbol_of(id.reference_id.get()) else {
                    // Unbound identifier → global property via `GetGlobal`/`SetGlobal`.
                    // Mirrors the `assign` global path and keeps `x++` on an
                    // intrinsic working without a `CompileError`. For v1 any
                    // unbound name is treated as a global (missing globals read
                    // as `undefined`, writes create the property).
                    let gid =
                        crate::model::str_id_of(self.comp.strings.get_or_intern(id.name.as_str()));
                    let old = self.new_temp();
                    self.emit_get_global(old, gid, span);
                    let one = self.new_temp();
                    self.load_int(one, delta, span);
                    let new = self.new_temp();
                    self.emit_reg3(Opcode::Add, new, old, one, span);
                    self.emit_set_global(gid, new, span);
                    return Ok(if prefix { new } else { old });
                };
                if self.comp.plans.const_bindings.contains(&sym)
                    && self.comp.plans.units[self.unit].is_strict
                {
                    return Err(self.err(id.span, "SyntaxError: Assignment to constant variable"));
                }
                let access = self.access(sym);
                let old = self.new_temp();
                self.read_access(access, old, span);
                let one = self.new_temp();
                self.load_int(one, delta, span);
                let new = self.new_temp();
                self.emit_reg3(Opcode::Add, new, old, one, span);
                self.store_access(access, new, span);
                Ok(if prefix { new } else { old })
            }
            _ => {
                let Some(m) = target.as_member_expression() else {
                    // The only remaining simple targets are TypeScript
                    // assertion forms, which no runtime location can back.
                    return Err(self.err(
                        span,
                        "TypeScript assertion expressions cannot be increment/decrement targets",
                    ));
                };
                let (obj, key) = self.member_parts(m)?;
                let old = self.new_temp();
                self.emit_reg3(Opcode::GetProperty, old, obj, key, span);
                let one = self.new_temp();
                self.load_int(one, delta, span);
                let new = self.new_temp();
                self.emit_reg3(Opcode::Add, new, old, one, span);
                self.emit_reg3(Opcode::SetProperty, obj, key, new, span);
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
    ) -> Res<u16> {
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
                return self.logical_assign(op, left, right, span);
            }
        };

        // Destructuring assignment: `[a, b] = rhs` / `{x, y} = rhs`.
        if !left.is_simple_assignment_target() {
            return self.destructure_assign(left, right, span);
        }
        let Some(simple) = left.as_simple_assignment_target() else {
            unreachable!("destructuring handled above");
        };
        let rhs = self.expr(right)?;

        match simple {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let Some(sym) = self.comp.symbol_of(id.reference_id.get()) else {
                    // Unbound assignment goes to global.
                    let rhs_val = match binop {
                        None => rhs,
                        Some(opcode) => {
                            let cur = self.new_temp();
                            let gid = crate::model::str_id_of(
                                self.comp.strings.get_or_intern(id.name.as_str()),
                            );
                            self.emit_get_global(cur, gid, span);
                            let out = self.new_temp();
                            self.emit_reg3(opcode, out, cur, rhs, span);
                            out
                        }
                    };
                    let gid =
                        crate::model::str_id_of(self.comp.strings.get_or_intern(id.name.as_str()));
                    self.emit_set_global(gid, rhs_val, span);
                    return Ok(rhs_val);
                };
                // Strict-mode const reassignment is a SyntaxError.
                if self.comp.plans.const_bindings.contains(&sym)
                    && self.comp.plans.units[self.unit].is_strict
                {
                    return Err(self.err(id.span, "SyntaxError: Assignment to constant variable"));
                }
                let access = self.access(sym);
                let value = match binop {
                    None => rhs,
                    Some(opcode) => {
                        let cur = self.new_temp();
                        self.read_access(access, cur, span);
                        let out = self.new_temp();
                        self.emit_reg3(opcode, out, cur, rhs, span);
                        out
                    }
                };
                self.store_access(access, value, span);
                Ok(value)
            }
            _ => {
                let Some(m) = simple.as_member_expression() else {
                    // The only remaining simple targets are TypeScript
                    // assertion forms, which no runtime location can back.
                    return Err(self.err(
                        span,
                        "TypeScript assertion expressions cannot be assignment targets",
                    ));
                };
                let (obj, key) = self.member_parts(m)?;
                let value = match binop {
                    None => rhs,
                    Some(opcode) => {
                        let cur = self.new_temp();
                        self.emit_reg3(Opcode::GetProperty, cur, obj, key, span);
                        let out = self.new_temp();
                        self.emit_reg3(opcode, out, cur, rhs, span);
                        out
                    }
                };
                self.emit_reg3(Opcode::SetProperty, obj, key, value, span);
                Ok(value)
            }
        }
    }

    // -- member access ----------------------------------------------------------

    /// Lowers `[a, b] = rhs` / `{x, y} = rhs`. Evaluates `rhs` once, then
    /// extracts each element/property and assigns to the sub-target.
    fn destructure_assign(
        &mut self,
        left: &AssignmentTarget<'_>,
        right: &Expression<'_>,
        span: Span,
    ) -> Res<u16> {
        let src = self.expr(right)?;
        match left {
            AssignmentTarget::ArrayAssignmentTarget(arr) => {
                // `[a, b, ...rest] = rhs`: element `i` reads index `i` off
                // the source; a rest element copies the tail via
                // `CopyArrayRest`. Elisions (`[a, , b]`) skip.
                let mut index: u32 = 0;
                for el in &arr.elements {
                    // `None` is an elision (`[a, , b]`): the slot is skipped.
                    let Some(el) = el else {
                        index += 1;
                        continue;
                    };
                    // `AssignmentTargetWithDefault` is a direct variant; the
                    // rest of the inner `AssignmentTarget` variants (simple
                    // identifiers/members) are flattened in. The rest element
                    // lives in `arr.rest`, handled after the loop.
                    if let oxc_ast::ast::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(
                        d,
                    ) = el
                    {
                        // `[a = default]`: default applies when the read is
                        // undefined — not handled in this pass.
                        if let Some(target) = d.binding.as_simple_assignment_target() {
                            let val = self.read_index(src, index, span)?;
                            self.assign_simple(target, val, span)?;
                        }
                        index += 1;
                    } else if let Some(simple) = el.as_simple_assignment_target() {
                        let val = self.read_index(src, index, span)?;
                        self.assign_simple(simple, val, span)?;
                        index += 1;
                    } else {
                        index += 1;
                    }
                }
                // `...rest`: tail copy from the current index.
                if let Some(rest) = &arr.rest {
                    let dst = self.new_temp();
                    self.emit_reg3(
                        Opcode::CopyArrayRest,
                        dst,
                        src,
                        index.min(u16::MAX as u32) as u16,
                        span,
                    );
                    if let Some(simple) = rest.target.as_simple_assignment_target() {
                        self.assign_simple(simple, dst, span)?;
                    }
                }
                Ok(src)
            }
            AssignmentTarget::ObjectAssignmentTarget(obj) => {
                // `{x, y: z} = rhs`: property `x` reads `rhs.x`.
                for prop in &obj.properties {
                    match prop {
                        oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(id) => {
                            // `{x} = rhs`: property `x` → target variable `x`.
                            let key = self.new_temp();
                            self.load_str(key, id.binding.name.as_str(), span)?;
                            let val = self.new_temp();
                            self.emit_reg3(Opcode::GetProperty, val, src, key, span);
                            let Some(sym) = self.comp.symbol_of(id.binding.reference_id.get()) else {
                                let gid = crate::model::str_id_of(self.comp.strings.get_or_intern(id.binding.name.as_str()));
                                self.emit_set_global(gid, val, span);
                                continue;
                            };
                            let access = self.access(sym);
                            self.store_access(access, val, span);
                        }
                        oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                            // `{y: z} = rhs`: property `y` → target `z`.
                            let key = self.property_key(&p.name)?;
                            let Some(simple) = p.binding.as_simple_assignment_target() else {
                                continue;
                            };
                            let val = self.new_temp();
                            self.emit_reg3(Opcode::GetProperty, val, src, key, span);
                            self.assign_simple(simple, val, span)?;
                        }
                    }
                }
                Ok(src)
            }
            _ => Err(self.err(span, "unsupported destructuring target")),
        }
    }

    /// Reads `src[index]` into a temp (missing → undefined).
    fn read_index(&mut self, src: u16, index: u32, span: Span) -> Res<u16> {
        let key = self.new_temp();
        self.load_int(key, index as i64, span);
        let dst = self.new_temp();
        self.emit_reg3(Opcode::GetProperty, dst, src, key, span);
        Ok(dst)
    }

    /// Assigns `val` to a simple target (identifier, member, or global).
    fn assign_simple(
        &mut self,
        target: &oxc_ast::ast::SimpleAssignmentTarget<'_>,
        val: u16,
        span: Span,
    ) -> Res<()> {
        match target {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let Some(sym) = self.comp.symbol_of(id.reference_id.get()) else {
                    let gid =
                        crate::model::str_id_of(self.comp.strings.get_or_intern(id.name.as_str()));
                    self.emit_set_global(gid, val, span);
                    return Ok(());
                };
                let access = self.access(sym);
                self.store_access(access, val, span);
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

    /// Evaluates object + key once (evaluation-order safe for read-modify-write
    /// targets) and returns their registers.
    ///
    /// For computed keys the key expression evaluates into a temp before the
    /// base (`obj[expr]` → ToPropertyKey then Get/SetProperty with dynamic
    /// `PropKey`), per bucket 4. Static keys materialize as interned strings.
    ///
    /// For a chain like `console.log`, the base (`console`) is emitted via
    /// `GetGlobal` (PropKey/Spur, string table id) and the property (`log`)
    /// via `LoadConst` (Str32, pool index → string table id). Both originate
    /// from the same `Rodeo` but the `GetProperty` key operand must be the
    /// `Str32` for `"log"`, not the `Spur` for `"console"`. The two
    /// namespaces are documented in `crate::model::Interner`.
    pub(crate) fn member_parts(&mut self, m: &MemberExpression<'_>) -> Res<(u16, u16)> {
        if m.optional() {
            return Err(self.err(m.span(), "optional chaining is not supported"));
        }
        match m {
            MemberExpression::StaticMemberExpression(s) => {
                // `console.log`: `console` → `GetGlobal` (PropKey), `log` →
                // `LoadConst` (Str32). Keep the ids distinct.
                let obj = if s.object.is_super() {
                    self.super_expr(s.object.span())?
                } else {
                    self.expr(&s.object)?
                };
                let key = self.new_temp();
                self.load_str(key, s.property.name.as_str(), s.property.span)?;
                Ok((obj, key))
            }
            MemberExpression::ComputedMemberExpression(c) => {
                // Key first, then base — single temp for key, no extra alloc.
                let key = self.expr(&c.expression)?;
                let obj = if c.object.is_super() {
                    self.super_expr(c.object.span())?
                } else {
                    self.expr(&c.object)?
                };
                Ok((obj, key))
            }
            MemberExpression::PrivateFieldExpression(p) => {
                // private assign targets not yet fully spec-correct; fallback to GetPrivate semantics
                let obj = self.expr(&p.object)?;
                let key = self.new_temp();
                self.load_str(key, &format!("#{}", p.field.name), p.span)?;
                Ok((obj, key))
            }
        }
    }

    // -- template literals ------------------------------------------------------

    /// Lowers `` p₀${e₁}p₁…${eₙ}pₙ `` to a left-fold of `Add` ops.
    ///
    /// The leading quasi is pinned as the left operand, so every subsequent
    /// `Add` has a string left side and takes the ToPrimitive/ToString
    /// concat path (ES `+` semantics: `"" + undefined === "undefined"`).
    /// Intermediate quasis may be empty strings and are skipped when they
    /// contribute nothing.
    fn template_literal(&mut self, t: &oxc_ast::ast::TemplateLiteral<'_>) -> Res<u16> {
        let quasi_text = |q: &oxc_ast::ast::TemplateElement| {
            q.value.cooked.as_deref().unwrap_or_default().to_string()
        };
        // Invariant head of any TemplateLiteral; guarded so malformed parses
        // surface as clean errors rather than indexing panics.
        let Some(first) = t.quasis.first() else {
            return Err(self.err(t.span, "malformed template literal"));
        };
        let mut acc = self.new_temp();
        self.load_str(acc, &quasi_text(first), first.span)?;
        for (i, e) in t.expressions.iter().enumerate() {
            let v = self.expr(e)?;
            let nxt = self.new_temp();
            self.emit_reg3(Opcode::Add, nxt, acc, v, e.span());
            acc = nxt;
            if let Some(q) = t.quasis.get(i + 1) {
                let text = quasi_text(q);
                if !text.is_empty() {
                    let s = self.new_temp();
                    self.load_str(s, &text, q.span)?;
                    let nxt = self.new_temp();
                    self.emit_reg3(Opcode::Add, nxt, acc, s, q.span);
                    acc = nxt;
                }
            }
        }
        Ok(acc)
    }

    // -- optional chains -------------------------------------------------------

    /// `r = v is nullish` — loose equality against `null` covers exactly
    /// the two nullish values (`null`, `undefined`).
    fn nullish_cmp(&mut self, v: u16, span: Span) -> Res<u16> {
        let null = self.new_temp();
        self.load_const(null, Const::Null, span)?;
        let cmp = self.new_temp();
        self.emit_reg3(Opcode::Eq, cmp, v, null, span);
        Ok(cmp)
    }

    /// One optional deref guard: `obj[key]` becomes `obj == null ? undefined
    /// : obj[key]`. Returns the result register.
    fn optional_deref(&mut self, span: Span, obj: u16, key: u16) -> Res<u16> {
        let dst = self.new_temp();
        let done = self.label();
        let cmp = self.nullish_cmp(obj, span)?;
        self.emit_jump(Opcode::JumpIfTrue, cmp, done);
        self.emit_reg3(Opcode::GetProperty, dst, obj, key, span);
        self.emit_jump(Opcode::Jump, 0, done);
        self.bind(done);
        Ok(dst)
    }

    /// Short-circuits to `undefined` when `v` is nullish; control flow
    /// otherwise continues at the bind point of the returned label.
    fn nullish_exit(&mut self, v: u16, dst: u16, exit: Label, span: Span) -> Res<()> {
        let cmp = self.nullish_cmp(v, span)?;
        let keep_going = self.label();
        self.emit_jump(Opcode::JumpIfFalse, cmp, keep_going);
        self.load_undefined(dst, span);
        self.emit_jump(Opcode::Jump, 0, exit);
        self.bind(keep_going);
        Ok(())
    }

    /// Lowering for an entire optional chain (`a?.b[c(x)].d(y)`), which oxc
    /// wraps in a single `ChainExpression` node regardless of nesting depth
    /// or expression position.
    ///
    /// The spine (nested member / call nodes from root down to the base
    /// value) is flattened and evaluated innermost-first. Each `?.` link
    /// guards its *incoming* value: a nullish value short-circuits the whole
    /// chain out through one shared exit label with an `undefined` result,
    /// skipping every remaining link including later argument evaluation.
    ///
    /// Receiver bookkeeping: `f.m(a)` binds `this` to the object preceding
    /// the last member deref; direct calls on call results or plain
    /// identifiers use `undefined`.
    fn chain_expr(&mut self, cx: &oxc_ast::ast::ChainExpression<'_>) -> Res<u16> {
        use oxc_ast::ast::{ChainElement, MemberExpression};

        /// One flattened spine element. Outermost links are pushed first;
        /// evaluation walks the list backwards (base-side first).
        enum SpineLink<'x> {
            Member {
                span: Span,
                optional: bool,
                member: &'x MemberExpression<'x>,
            },
            Call {
                span: Span,
                optional: bool,
                args: &'x [oxc_ast::ast::Argument<'x>],
            },
        }

        // Walk from the chain root down to the base value, flattening as we
        // go. Private fields remain rejected with a named construct error.
        // Each match arm guarantees the `as_member_expression` conversion.
        #[allow(clippy::expect_used)]
        fn collect<'x>(
            links: &mut Vec<SpineLink<'x>>,
            mut cur: &'x Expression<'x>,
        ) -> Result<&'x Expression<'x>, Span> {
            loop {
                match cur {
                    Expression::StaticMemberExpression(s) => {
                        links.push(SpineLink::Member {
                            span: s.span,
                            optional: s.optional,
                            member: cur.as_member_expression().expect("static member"),
                        });
                        cur = &s.object;
                    }
                    Expression::ComputedMemberExpression(c) => {
                        links.push(SpineLink::Member {
                            span: c.span,
                            optional: c.optional,
                            member: cur.as_member_expression().expect("computed member"),
                        });
                        cur = &c.object;
                    }
                    Expression::CallExpression(c) => {
                        links.push(SpineLink::Call {
                            span: c.span,
                            optional: c.optional,
                            args: &c.arguments,
                        });
                        cur = &c.callee;
                    }
                    Expression::PrivateFieldExpression(p) => {
                        links.push(SpineLink::Member {
                            span: p.span,
                            optional: p.optional,
                            member: cur.as_member_expression().expect("private field"),
                        });
                        cur = &p.object;
                    }
                    _ => return Ok(cur),
                }
            }
        }

        let mut links: Vec<SpineLink<'_>> = Vec::new();
        // The root chain element may be a member or a call; push its link
        // manually (it has no wrapping `Expression` to reuse) and continue
        // collecting toward the base value.
        let after_root = match &cx.expression {
            ChainElement::StaticMemberExpression(s) => {
                links.push(SpineLink::Member {
                    span: s.span,
                    optional: s.optional,
                    member: cx.expression.member_expression().expect("static member"),
                });
                &s.object
            }
            ChainElement::ComputedMemberExpression(c) => {
                links.push(SpineLink::Member {
                    span: c.span,
                    optional: c.optional,
                    member: cx.expression.member_expression().expect("computed member"),
                });
                &c.object
            }
            ChainElement::CallExpression(c) => {
                links.push(SpineLink::Call {
                    span: c.span,
                    optional: c.optional,
                    args: &c.arguments,
                });
                &c.callee
            }
            ChainElement::PrivateFieldExpression(p) => {
                links.push(SpineLink::Member {
                    span: p.span,
                    optional: p.optional,
                    member: cx.expression.member_expression().expect("private field"),
                });
                &p.object
            }
            ChainElement::TSNonNullExpression(t) => {
                return Err(self.err(t.span, "non-null assertions are TypeScript-only"));
            }
        };
        let base = collect(&mut links, after_root)
            .map_err(|span| self.err(span, "private fields are not supported"))?;

        let exit = self.label();
        let dst = self.new_temp();
        let mut cur = self.expr(base)?;
        // Register holding the object before the most recent member deref —
        // the receiver candidate for a following call link.
        let mut prev_recv: Option<u16> = None;
        for link in links.iter().rev() {
            match link {
                SpineLink::Member {
                    span,
                    optional,
                    member,
                } => {
                    if *optional {
                        self.nullish_exit(cur, dst, exit, *span)?;
                    }
                    let kreg = match member {
                        MemberExpression::StaticMemberExpression(s) => {
                            let r = self.new_temp();
                            self.load_str(r, s.property.name.as_str(), s.property.span)?;
                            r
                        }
                        MemberExpression::ComputedMemberExpression(c) => {
                            self.expr(&c.expression)?
                        }
                        MemberExpression::PrivateFieldExpression(p) => {
                            let r = self.new_temp();
                            self.load_str(r, &format!("#{}", p.field.name), p.span)?;
                            r
                        }
                    };
                    prev_recv = Some(cur);
                    let nxt = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, nxt, cur, kreg, *span);
                    cur = nxt;
                }
                SpineLink::Call {
                    span,
                    optional,
                    args,
                } => {
                    if *optional {
                        // `?.()` guards the callee value itself.
                        self.nullish_exit(cur, dst, exit, *span)?;
                    }
                    let argc = u16::try_from(args.len()).map_err(|_| {
                        self.err(*span, "calls above 65535 arguments are not supported")
                    })?;
                    // Layout: [callee][this][arg…]; see `CALL_HEADER_REGS`.
                    let window_base = self.new_temps(crate::model::CALL_HEADER_REGS + argc);
                    self.move_reg(window_base, cur, *span);
                    match prev_recv {
                        Some(r) => self.move_reg(window_base + 1, r, *span),
                        None => self.load_undefined(window_base + 1, *span),
                    }
                    let has_spread = args
                        .iter()
                        .any(|a| matches!(a, oxc_ast::ast::Argument::SpreadElement(_)));
                    if !has_spread {
                        for (i, arg) in args.iter().enumerate() {
                            let x = arg.as_expression().expect("non-spread argument");
                            self.expr_into(x, window_base + 2 + i as u16)?;
                        }
                        self.emit_call(window_base, window_base, argc, *span);
                    } else {
                        let arr = self.build_args_array(args, *span)?;
                        self.emit_reg3(Opcode::CallApply, window_base, window_base, arr, *span);
                    }
                    cur = window_base;
                    prev_recv = None; // call results carry no receiver context
                }
            }
        }
        self.move_reg(dst, cur, cx.span);
        self.bind(exit);
        Ok(dst)
    }
    /// `a &&= b`, `a ||= b`, `a ??= b`.
    ///
    /// Read-modify-write with a logical guard: the current value is read
    /// once, a condition decides whether the right-hand side is evaluated,
    /// assigned, and yielded, or whether the original value flows through.
    ///
    /// Deviation (member targets only): ES re-evaluates the target reference
    /// (`a.b ??= c` reads `b` again for the store); we keep the already
    /// resolved `(obj, key)` pair, so getter/setter re-entry between read
    /// and write is not observable here anyway — this subset has no accessors
    /// on plain member targets with observable intermediate state.
    fn logical_assign(
        &mut self,
        op: AssignmentOperator,
        left: &AssignmentTarget<'_>,
        right: &Expression<'_>,
        span: Span,
    ) -> Res<u16> {
        let Some(simple) = left.as_simple_assignment_target() else {
            return Err(self.err(span, "destructuring in logical assignment is not supported"));
        };
        // Storage slot pair: (read register, writer closure context).
        enum Target {
            Id(VarAccess),
            Member { obj: u16, key: u16 },
        }
        let target = match simple {
            oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) => {
                let Some(sym) = self.comp.symbol_of(id.reference_id.get()) else {
                    // Unbound identifier → global property.
                    return self.logical_assign_global(op, id, right, span);
                };
                if self.comp.plans.const_bindings.contains(&sym)
                    && self.comp.plans.units[self.unit].is_strict
                {
                    return Err(self.err(id.span, "SyntaxError: Assignment to constant variable"));
                }
                Target::Id(self.access(sym))
            }
            _ => {
                let Some(m) = simple.as_member_expression() else {
                    return Err(self.err(
                        span,
                        "TypeScript assertion expressions cannot be logical assignment targets",
                    ));
                };
                let (obj, key) = self.member_parts(m)?;
                Target::Member { obj, key }
            }
        };
        // Read current value.
        let cur = self.new_temp();
        match &target {
            Target::Id(access) => self.read_access(*access, cur, span),
            Target::Member { obj, key } => {
                self.emit_reg3(Opcode::GetProperty, cur, *obj, *key, span)
            }
        }

        let end = self.label();
        match op {
            AssignmentOperator::LogicalAnd => {
                // falsy → keep `cur` untouched.
                self.emit_jump(Opcode::JumpIfFalse, cur, end);
            }
            AssignmentOperator::LogicalOr => {
                // truthy → keep `cur` untouched.
                self.emit_jump(Opcode::JumpIfTrue, cur, end);
            }
            AssignmentOperator::LogicalNullish => {
                // non-nullish → keep `cur`; nullish → assign. Reuse the
                // shared guard: invert by jumping on non-nullish to `end`.
                let cmp = self.nullish_cmp(cur, span)?;
                self.emit_jump(Opcode::JumpIfFalse, cmp, end);
            }
            _ => unreachable!("only logical ops routed here"),
        }
        // Assign + yield the right-hand side.
        let rhs = self.expr(right)?;
        match &target {
            Target::Id(access) => self.store_access(*access, rhs, span),
            Target::Member { obj, key } => {
                self.emit_reg3(Opcode::SetProperty, *obj, *key, rhs, span)
            }
        }
        self.bind(end);
        Ok(rhs)
    }

    /// Logical assignment against an unbound (global) identifier.
    fn logical_assign_global(
        &mut self,
        op: AssignmentOperator,
        id: &oxc_ast::ast::IdentifierReference<'_>,
        right: &Expression<'_>,
        span: Span,
    ) -> Res<u16> {
        let gid = crate::model::str_id_of(self.comp.strings.get_or_intern(id.name.as_str()));
        let cur = self.new_temp();
        self.emit_get_global(cur, gid, span);
        let end = self.label();
        match op {
            AssignmentOperator::LogicalAnd => self.emit_jump(Opcode::JumpIfFalse, cur, end),
            AssignmentOperator::LogicalOr => self.emit_jump(Opcode::JumpIfTrue, cur, end),
            AssignmentOperator::LogicalNullish => {
                let cmp = self.nullish_cmp(cur, span)?;
                self.emit_jump(Opcode::JumpIfFalse, cmp, end);
            }
            _ => unreachable!("only logical ops routed here"),
        }
        let rhs = self.expr(right)?;
        self.emit_set_global(gid, rhs, span);
        self.bind(end);
        Ok(rhs)
    }

    // -- constructor invocations ------------------------------------------------

    /// `new F(args)` → [`Opcode::Construct`].
    ///
    /// Same register layout as a call (`[callee][this][arg…]`); the `this`
    /// slot stays `undefined` because the interpreter supplies the freshly
    /// allocated instance itself. Spread arguments are rejected at compile
    /// time with a named error rather than silently truncating the argument
    /// list (there is no ConstructApply opcode yet).
    fn new_expr(&mut self, n: &oxc_ast::ast::NewExpression<'_>) -> Res<u16> {
        let argc = u16::try_from(n.arguments.len()).map_err(|_| {
            self.err(
                n.span,
                "constructor calls above 65535 arguments are not supported",
            )
        })?;
        let window = self.new_temps(argc + crate::model::CALL_HEADER_REGS);
        if let Some(m) = n.callee.as_member_expression() {
            // Property-style constructor reference (`new lib.Widget()`).
            let (obj, key) = self.member_parts(m)?;
            self.emit_reg3(Opcode::GetProperty, window, obj, key, n.span);
        } else {
            let v = self.expr(&n.callee)?;
            self.move_reg(window, v, n.span);
        }
        self.load_undefined(window + 1, n.span);
        for (i, arg) in n.arguments.iter().enumerate() {
            if matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)) {
                return Err(self.err(arg.span(), "spread arguments in `new` are not supported"));
            }
            let x = arg.as_expression().expect("non-spread argument");
            self.expr_into(x, window + 2 + i as u16)?;
        }
        self.emit_construct(window, window, argc, n.span);
        Ok(window)
    }

    // -- calls ---------------------------------------------------------------------

    /// Builds an argument array from a mixed argument list: spread elements
    /// are validated as arrays (`CheckIsArray`) and appended element-wise.
    fn build_args_array(&mut self, args: &[oxc_ast::ast::Argument<'_>], span: Span) -> Res<u16> {
        let arr = self.new_temp();
        self.emit_new_array(arr, self.undef_reg(), 0, span);
        for arg in args {
            match arg {
                oxc_ast::ast::Argument::SpreadElement(s) => {
                    let src = self.expr(&s.argument)?;
                    self.emit_reg3(Opcode::CheckIsArray, src, 0, 0, s.span);
                    self.emit_reg3(Opcode::ArrayAppend, arr, src, 0, s.span);
                }
                _ => {
                    let x = arg
                        .as_expression()
                        .expect("non-spread argument is an expression");
                    let val = self.expr(x)?;
                    let len_key = self.new_temp();
                    self.load_str(len_key, "length", x.span())?;
                    let len = self.new_temp();
                    self.emit_reg3(Opcode::GetProperty, len, arr, len_key, x.span());
                    self.emit_reg3(Opcode::SetProperty, arr, len, val, x.span());
                }
            }
        }
        Ok(arr)
    }

    /// Compiles one call. Non-optional calls (`f(x)`); optional-call forms
    /// route through `chain_expr`.
    fn call(&mut self, c: &CallExpression<'_>) -> Res<u16> {
        if c.optional {
            return Err(self.err(c.span, "optional calls are not supported"));
        }
        let has_spread = c
            .arguments
            .iter()
            .any(|a| matches!(a, oxc_ast::ast::Argument::SpreadElement(_)));
        if !has_spread {
            let argc = u16::try_from(c.arguments.len())
                .map_err(|_| self.err(c.span, "calls above 65535 arguments are not supported"))?;
            // Layout: [callee][this][arg…]; see `CALL_HEADER_REGS`.
            let block = self.new_temps(argc + CALL_HEADER_REGS);

            if c.callee.is_super() {
                // `super(...)`: call the parent constructor with `this`.
                let super_ctor = self.super_ctor(c.span)?;
                self.move_reg(block, super_ctor, c.span);
                self.move_reg(block + 1, REG_THIS, c.span);
            } else if let Some(m) = c.callee.as_member_expression() {
                // Method call: obj + key evaluate first, `this` = object.
                // `super.x(...)` uses `this` as the receiver (the home object
                // supplies the method, the current `this` the receiver).
                let (obj, key) = self.member_parts(m)?;
                self.emit_reg3(Opcode::GetProperty, block, obj, key, c.span);
                if m.object().is_super() {
                    self.move_reg(block + 1, REG_THIS, c.span);
                } else {
                    self.move_reg(block + 1, obj, c.span);
                }
            } else {
                let v = self.expr(&c.callee)?;
                self.move_reg(block, v, c.span);
                self.load_undefined(block + 1, c.span);
            }
            for (i, arg) in c.arguments.iter().enumerate() {
                let Some(x) = arg.as_expression() else {
                    return Err(self.err(arg.span(), "spread arguments are not supported"));
                };
                self.expr_into(x, block + 2 + i as u16)?;
            }
            self.emit_call(block, block, argc, c.span);
            return Ok(block);
        }
        // Spread path: build args array and use CallApply.
        let block = self.new_temps(CALL_HEADER_REGS);
        if let Some(m) = c.callee.as_member_expression() {
            let (obj, key) = self.member_parts(m)?;
            self.emit_reg3(Opcode::GetProperty, block, obj, key, c.span);
            self.move_reg(block + 1, obj, c.span);
        } else {
            let v = self.expr(&c.callee)?;
            self.move_reg(block, v, c.span);
            self.load_undefined(block + 1, c.span);
        }
        // Build args array: spread elements are validated and appended.
        let args_arr = self.build_args_array(&c.arguments, c.span)?;
        let dst = block;
        self.emit_reg3(Opcode::CallApply, dst, block, args_arr, c.span);
        Ok(dst)
    }

    // -- closures --------------------------------------------------------------------

    /// Emits `Closure` for a nested function, compiling its body first.
    pub(crate) fn closure_fn(&mut self, dst: u16, f: &oxc_ast::ast::Function<'_>) -> Res<()> {
        let idx = self.planned_index(f.span)?;
        crate::unit::compile_unit(self.comp, idx, crate::unit::UnitNode::Fn(f))?;
        self.emit_closure_instr(dst, idx, f.span)
    }

    /// Emits `Closure` for a nested arrow, compiling its body first.
    fn closure_arrow(
        &mut self,
        dst: u16,
        a: &oxc_ast::ast::ArrowFunctionExpression<'_>,
    ) -> Res<()> {
        let idx = self.planned_index(a.span)?;
        crate::unit::compile_unit(self.comp, idx, crate::unit::UnitNode::Arrow(a))?;
        self.emit_closure_instr(dst, idx, a.span)
    }

    pub(crate) fn planned_index(&self, span: Span) -> Res<usize> {
        self.comp
            .plans
            .fn_index
            .get(&span)
            .copied()
            .ok_or_else(|| self.err(span, "internal: nested function missing from plans"))
    }

    /// Env depth from the current frame to the class scope env captured by the
    /// nearest method/constructor unit. Each env-bearing unit on the path
    /// (including the method's own prologue env) contributes one parent link;
    /// the class env itself is that link's target (depth 0 when the frame env
    /// is the class env directly).
    fn super_env_depth(&self) -> u8 {
        let units = &self.comp.plans.units;
        let mut depth = 0u8;
        let mut cur = Some(self.unit);
        while let Some(u) = cur {
            if units[u].has_env {
                depth += 1;
            }
            if !units[u].is_arrow && units[u].uses_super {
                break;
            }
            cur = units[u].parent;
        }
        depth
    }

    /// `super` in expression position: resolves the parent constructor from
    /// the class env. For member access the caller additionally dereferences
    /// `.prototype` for instance methods.
    fn super_ctor(&mut self, span: Span) -> Res<u16> {
        // Walk up to find the nearest non-arrow unit that uses super (the class method/constructor).
        let units = &self.comp.plans.units;
        let mut cur = Some(self.unit);
        let mut found = false;
        while let Some(u) = cur {
            if !units[u].is_arrow && units[u].uses_super {
                found = true;
                break;
            }
            cur = units[u].parent;
        }
        if !found {
            return Err(self.err(span, "`super` outside a class method is not supported"));
        }
        let depth = self.super_env_depth();
        let d = self.new_temp();
        self.emit_get_env(d, depth, crate::class::SLOT_SUPER_CTOR, span);
        Ok(d)
    }

    /// Lowers `super` used as a member object: `superCtor.prototype` for
    /// instance methods, `superCtor` for static methods and constructors.
    fn super_expr(&mut self, span: Span) -> Res<u16> {
        let ctor = self.super_ctor(span)?;
        let is_static = self.comp.plans.units[self.unit].static_method;
        if is_static {
            return Ok(ctor);
        }
        let proto = self.new_temp();
        let key = self.new_temp();
        self.load_str(key, "prototype", span)?;
        self.emit_reg3(Opcode::GetProperty, proto, ctor, key, span);
        Ok(proto)
    }

    fn emit_closure_instr(&mut self, dst: u16, idx: usize, span: Span) -> Res<()> {
        let idx16 = u16::try_from(idx)
            .map_err(|_| self.err(span, "programs above 65535 functions are not supported"))?;
        self.emit_closure(dst, idx16, span);
        Ok(())
    }
}

/// Property key text for the static (non-computed) forms we support.
pub(crate) fn static_key_text(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
        PropertyKey::StringLiteral(s) => Some(s.value.to_string()),
        PropertyKey::NumericLiteral(n) => Some(number_to_key(n.value)),
        PropertyKey::PrivateIdentifier(p) => Some(format!("#{}", p.name)),
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
