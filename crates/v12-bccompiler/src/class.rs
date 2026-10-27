//! Class lowering (ES §15.7.14 `ClassDefinitionEvaluation`).
//!
//! A class evaluates to:
//! 1. A **class-scope environment** (one per evaluation) holding the home
//!    objects `F` and `F.prototype`, the instance-members initializer, the
//!    class's Private Names and its computed field keys. The constructor,
//!    every method and both initializers capture it.
//! 2. A constructor **function unit** (`[[ConstructorKind]]` base or
//!    derived) — the explicit `constructor` body, or the default one.
//! 3. The prototype object, linked to `F` via `prototype`/`constructor`, with
//!    `extends` validated by `CheckHeritage` and wired with `SetPrototype`.
//! 4. Methods and accessors on the prototype or `F`; private ones on their
//!    Private Name.
//! 5. Two synthetic units for the fields: the instance-members initializer
//!    (run by a base constructor on entry and by every `super()` once it has
//!    bound `this`) and the static initializer (run once, `this` = `F`).

use oxc_ast::ast::{Class, ClassBody, ClassElement, Expression, MethodDefinitionKind, PropertyKey};
use oxc_span::{GetSpan, Span};
use v12_bytecode::Opcode;

use crate::expr::static_key_text;
use crate::model::{CompileError, FnCtx, REG_THIS};

/// Class-scope environment layout (see the module docs).
pub(crate) const SLOT_HOME_CTOR: u16 = 0;
pub(crate) const SLOT_HOME_PROTO: u16 = 1;
pub(crate) const SLOT_INSTANCE_INIT: u16 = 2;
/// The class scope's first Private Name slot; name `i` lives at `+ i`, and
/// the computed field keys follow the names.
pub(crate) const SLOT_FIRST_PRIVATE_NAME: u16 = 3;

/// One declared `#name` of a class body: its text (with `#`), its
/// [`v12_bytecode::private_kind`], and whether it is `static`.
struct PrivateDecl {
    name: String,
    kind: u16,
    is_static: bool,
}

/// The Private Names a class body declares, deduplicated (a getter/setter
/// pair shares one name) and in first-declaration order — the slot order.
fn private_decls(body: &ClassBody<'_>) -> Vec<PrivateDecl> {
    let mut out: Vec<PrivateDecl> = Vec::new();
    for el in &body.body {
        let (key, kind, is_static) = match el {
            ClassElement::MethodDefinition(m) => {
                let kind = match m.kind {
                    MethodDefinitionKind::Get | MethodDefinitionKind::Set => {
                        v12_bytecode::private_kind::ACCESSOR
                    }
                    _ => v12_bytecode::private_kind::METHOD,
                };
                (&m.key, kind, m.r#static)
            }
            ClassElement::PropertyDefinition(p) => {
                (&p.key, v12_bytecode::private_kind::FIELD, p.r#static)
            }
            ClassElement::StaticBlock(_)
            | ClassElement::AccessorProperty(_)
            | ClassElement::TSIndexSignature(_) => continue,
        };
        let PropertyKey::PrivateIdentifier(id) = key else {
            continue;
        };
        let name = format!("#{}", id.name);
        if !out.iter().any(|d| d.name == name) {
            out.push(PrivateDecl {
                name,
                kind,
                is_static,
            });
        }
    }
    out
}

/// Hop count from the current emission point to class scope `scope_idx`'s
/// environment: the current unit's live class scopes, plus — for every unit
/// between here and the class's outer unit — its own environment and the
/// class scopes it was created under, minus the scopes the outer unit had
/// pushed *above* the class's (none of which the class's closures captured).
fn class_scope_depth(
    cx: &FnCtx<'_, '_, '_, '_>,
    scope_idx: usize,
    span: Span,
) -> Result<u16, CompileError> {
    let scope = &cx.comp.class_scopes[scope_idx];
    let plans = &cx.comp.plans;
    let mut depth = u32::from(cx.comp.live_env_bias);
    let mut cur = cx.unit;
    while cur != scope.outer_unit {
        let unit = &plans.units[cur];
        depth += u32::from(unit.has_env) + u32::from(unit.creation_bias);
        let Some(parent) = unit.parent else {
            break;
        };
        cur = parent;
    }
    let depth = depth.saturating_sub(u32::from(scope.bias_level));
    u16::try_from(depth).map_err(|_| cx.err(span, "class scope environment depth out of range"))
}

/// Loads slot `slot` of class scope `scope_idx`'s environment into a fresh
/// register.
fn load_class_slot(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    scope_idx: usize,
    slot: u16,
    span: Span,
) -> Result<u16, CompileError> {
    let depth = class_scope_depth(cx, scope_idx, span)?;
    let dst = cx.new_temp();
    cx.emit_get_env(dst, depth, slot, span);
    Ok(dst)
}

/// The class scope whose constructor is the `this` home of the current unit
/// — the class a `super()` here constructs for.
pub(crate) fn ctor_class_scope(cx: &FnCtx<'_, '_, '_, '_>) -> Option<usize> {
    let home = cx.comp.plans.this_home(cx.unit);
    cx.comp.class_scopes.iter().rposition(|s| s.ctor_unit == home)
}

/// Emits `InitializeInstanceElements(this_reg, F)` for class scope
/// `scope_idx`: a call of its instance-members initializer, when it has one.
pub(crate) fn emit_instance_init_call(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    scope_idx: usize,
    this_reg: u16,
    span: Span,
) -> Result<(), CompileError> {
    if cx.comp.class_scopes[scope_idx].instance_init.is_none() {
        return Ok(());
    }
    let init = load_class_slot(cx, scope_idx, SLOT_INSTANCE_INIT, span)?;
    let block = cx.new_temps(crate::model::CALL_HEADER_REGS);
    cx.move_reg(block, init, span);
    cx.move_reg(block + 1, this_reg, span);
    cx.emit_call(block, block, 0, span);
    Ok(())
}

/// Loads the Private Name `#name` (given without the `#`) into a fresh
/// register, resolving it to the innermost enclosing class that declares it.
pub(crate) fn load_private_name(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    name: &str,
    span: Span,
) -> Result<u16, CompileError> {
    let text = format!("#{name}");
    let Some((scope_idx, index)) = cx
        .comp
        .class_scopes
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, s)| s.names.iter().position(|n| *n == text).map(|p| (i, p)))
    else {
        return Err(cx.err(
            span,
            format!("SyntaxError: Private field '{text}' must be declared in an enclosing class"),
        ));
    };
    let slot = u16::try_from(index)
        .ok()
        .and_then(|i| i.checked_add(SLOT_FIRST_PRIVATE_NAME))
        .ok_or_else(|| cx.err(span, "too many private names in one class"))?;
    load_class_slot(cx, scope_idx, slot, span)
}

/// The fields of `body` with the given `static`-ness, in order.
fn fields<'b, 'a>(
    body: &'b ClassBody<'a>,
    is_static: bool,
) -> impl Iterator<Item = &'b oxc_ast::ast::PropertyDefinition<'a>> {
    body.body.iter().filter_map(move |el| match el {
        ClassElement::PropertyDefinition(p) if p.r#static == is_static => Some(&**p),
        _ => None,
    })
}

/// Lowers a class expression, returning the register holding the constructor.
pub(crate) fn class_expression(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    c: &Class<'_>,
    is_statement: bool,
) -> Result<u16, CompileError> {
    let _ = is_statement;
    let span = c.span;
    let privates = private_decls(&c.body);
    let computed_fields: Vec<Span> = c
        .body
        .body
        .iter()
        .filter_map(|el| match el {
            ClassElement::PropertyDefinition(p) if p.computed => Some(p.span),
            _ => None,
        })
        .collect();
    let inits = cx.comp.plans.class_inits.get(&span).copied().unwrap_or_default();
    let too_many = || "too many private names or computed fields in one class";
    let first_key_slot = u16::try_from(privates.len())
        .ok()
        .and_then(|n| n.checked_add(SLOT_FIRST_PRIVATE_NAME))
        .ok_or_else(|| cx.err(span, too_many()))?;
    let slots = u16::try_from(computed_fields.len())
        .ok()
        .and_then(|n| n.checked_add(first_key_slot))
        .ok_or_else(|| cx.err(span, too_many()))?;

    // 1. The class scope. Pushed before any closure is created so the
    //    constructor, methods and initializers all capture it; *scoped*, so
    //    it is retired at the end and the rest of the unit resolves at its
    //    structural depths (`emit_scoped_env` records the push in
    //    `env_bias`, which covers the heritage and computed keys evaluated
    //    while it is the innermost environment). Elided when nothing in
    //    the class reads it (`ClassInitUnits::needs_env`).
    if inits.needs_env {
        cx.emit_scoped_env(0, slots, span);
    }
    //    Every `#name` gets a fresh Private Name per evaluation (step 6).
    for (i, d) in privates.iter().enumerate() {
        let dst = cx.new_temp();
        let description = crate::model::str_id_of(cx.comp.strings.get_or_intern(&d.name));
        let words = v12_bytecode::WideOp::NewPrivateNameW {
            dst,
            kind: d.kind,
            description,
        }
        .encode();
        cx.emit_words(words, span);
        // `i` is bounded by the slot count checked above.
        cx.emit_set_env(0, SLOT_FIRST_PRIVATE_NAME + i as u16, dst, span);
    }

    // 2. `extends` (step 8) evaluates with the *outer* private environment,
    //    so the class's own scope is registered only after it.
    let heritage = match &c.heritage {
        Some(h) => {
            let parent = cx.expr(&h.expression)?;
            let proto_parent = cx.new_temp();
            cx.emit_reg3(Opcode::CheckHeritage, proto_parent, parent, 0, span);
            Some((parent, proto_parent))
        }
        None => None,
    };
    let ctor_idx = cx
        .planned_index(span)
        .map_err(|_| cx.err(span, "internal: class missing from plans"))?;
    let bias_level = cx.comp.live_env_bias;
    cx.comp.class_scopes.push(crate::model::ClassScope {
        names: privates.iter().map(|d| d.name.clone()).collect(),
        outer_unit: cx.unit,
        bias_level,
        ctor_unit: ctor_idx,
        instance_init: inits.instance,
        field_keys: computed_fields
            .iter()
            .enumerate()
            .map(|(i, &s)| (s, first_key_slot + i as u16))
            .collect(),
    });

    // 3. The constructor `F` and its prototype object.
    crate::unit::compile_unit(cx.comp, ctor_idx, crate::unit::UnitNode::Class(c))?;
    let ctor = cx.new_temp();
    cx.emit_closure_instr(ctor, ctor_idx, span)?;
    let proto = cx.new_temp();
    cx.emit_reg3(Opcode::NewObject, proto, 0, 0, span);
    if let Some((parent, proto_parent)) = heritage {
        // `extends null` keeps `F.[[Prototype]]` = `%Function.prototype%`.
        cx.emit_reg3(Opcode::SetPrototype, 0, proto, proto_parent, span);
        let null_reg = cx.new_temp();
        cx.load_const(null_reg, v12_bytecode::Const::Null, span)?;
        let is_null = cx.new_temp();
        cx.emit_reg3(Opcode::StrictEq, is_null, parent, null_reg, span);
        let skip = cx.label();
        cx.emit_jump(Opcode::JumpIfTrue, is_null, skip);
        cx.emit_reg3(Opcode::SetPrototype, 0, ctor, parent, span);
        cx.bind(skip);
    }
    let proto_key = cx.load_str_key("prototype", span)?;
    let ctor_key = cx.load_str_key("constructor", span)?;
    cx.emit_reg3(Opcode::DefineMethod, ctor, proto_key, proto, span);
    cx.emit_reg3(Opcode::DefineMethod, proto, ctor_key, ctor, span);
    if inits.needs_env {
        cx.emit_set_env(0, SLOT_HOME_CTOR, ctor, span);
        cx.emit_set_env(0, SLOT_HOME_PROTO, proto, span);
    }

    // 4. Methods, accessors and the computed field keys, in element order.
    define_elements(cx, &c.body, ctor, proto)?;

    // 5. The instance-members initializer, kept in the class scope for the
    //    constructor (base) or every `super()` (derived) to call.
    if let Some(idx) = inits.instance {
        crate::unit::compile_unit(cx.comp, idx, crate::unit::UnitNode::InstanceInit(c))?;
        let init = cx.new_temp();
        cx.emit_closure_instr(init, idx, span)?;
        cx.emit_set_env(0, SLOT_INSTANCE_INIT, init, span);
    }

    // 6. Static private methods join `F` (step 30), then the inner class
    //    binding is initialized (step 27 — before any static initializer
    //    can read it), then the static fields run with `this` = `F`.
    for d in &privates {
        if d.is_static && d.kind != v12_bytecode::private_kind::FIELD {
            let name = load_private_name(cx, &d.name[1..], span)?;
            let undef = cx.new_temp();
            cx.load_undefined(undef, span);
            let words = v12_bytecode::WideOp::DefinePrivateW {
                obj: ctor,
                name,
                value: undef,
            }
            .encode();
            cx.emit_words(words, span);
        }
    }
    if let Some(id) = &c.id
        && let Some(sym) = id.symbol_id.get()
    {
        let access = cx.access(sym);
        cx.store_access(access, ctor, id.span);
    }
    if let Some(idx) = inits.static_ {
        crate::unit::compile_unit(cx.comp, idx, crate::unit::UnitNode::StaticInit(c))?;
        let block = cx.new_temps(crate::model::CALL_HEADER_REGS);
        cx.emit_closure_instr(block, idx, span)?;
        cx.move_reg(block + 1, ctor, span);
        cx.emit_call(block, block, 0, span);
    }

    // 7. Retire the class scope.
    cx.comp.class_scopes.pop();
    if inits.needs_env {
        cx.emit_pop_env(span);
    }
    Ok(ctor)
}

/// Defines every method and accessor on the prototype (`proto`) or the
/// constructor (`ctor`), private ones on their Private Name, and evaluates
/// each computed field key into its class-scope slot — all in element order
/// (ES §15.7.14 step 26 `ClassElementEvaluation`).
fn define_elements(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    body: &ClassBody<'_>,
    ctor: u16,
    proto: u16,
) -> Result<(), CompileError> {
    for el in &body.body {
        match el {
            ClassElement::MethodDefinition(m) => {
                if m.kind == MethodDefinitionKind::Constructor {
                    continue;
                }
                if let PropertyKey::PrivateIdentifier(id) = &m.key {
                    // The function hangs off the Private Name: every object
                    // that later receives the name shares it.
                    let fn_reg = method_fn(cx, m)?;
                    let name = load_private_name(cx, &id.name, m.span)?;
                    let slot = u16::from(m.kind == MethodDefinitionKind::Set);
                    let words = v12_bytecode::WideOp::SetPrivateMethodW {
                        name,
                        value: fn_reg,
                        slot,
                    }
                    .encode();
                    cx.emit_words(words, m.span);
                    continue;
                }
                let target = if m.r#static { ctor } else { proto };
                let key_reg = property_key_reg(cx, &m.key, m.computed, m.span)?;
                match m.kind {
                    MethodDefinitionKind::Get => {
                        // A CLASS accessor is non-enumerable (ES 15.7.6), so it
                        // keeps the plain `DefineAccessor`; the object-literal
                        // spelling emits `DefineAccessorEnumerable` (ADR-0005).
                        let pair = cx.new_temps(2);
                        let getter = method_fn(cx, m)?;
                        cx.move_reg(pair, getter, m.span);
                        cx.load_undefined(pair + 1, m.span);
                        cx.emit_reg3(Opcode::DefineAccessor, target, key_reg, pair, m.span);
                    }
                    MethodDefinitionKind::Set => {
                        let pair = cx.new_temps(2);
                        cx.load_undefined(pair, m.span);
                        let setter = method_fn(cx, m)?;
                        cx.move_reg(pair + 1, setter, m.span);
                        cx.emit_reg3(Opcode::DefineAccessor, target, key_reg, pair, m.span);
                    }
                    MethodDefinitionKind::Method | MethodDefinitionKind::Constructor => {
                        let fn_reg = method_fn(cx, m)?;
                        cx.emit_reg3(Opcode::DefineMethod, target, key_reg, fn_reg, m.span);
                    }
                }
            }
            ClassElement::PropertyDefinition(p) if p.computed => {
                // `ClassFieldDefinitionEvaluation` step 1: the key is
                // evaluated (and `ToPropertyKey`'d) once, here.
                let Some(e) = p.key.as_expression() else {
                    return Err(cx.err(p.span, "unsupported computed class field key"));
                };
                let raw = cx.expr(e)?;
                let key = cx.new_temp();
                cx.emit_reg3(Opcode::ToPropertyKey, key, raw, 0, p.span);
                let scope_idx = cx.comp.class_scopes.len() - 1;
                let slot = field_key_slot(cx, scope_idx, p.span)?;
                cx.emit_set_env(0, slot, key, p.span);
            }
            ClassElement::PropertyDefinition(_) | ClassElement::TSIndexSignature(_) => {}
            ClassElement::StaticBlock(_) => {
                return Err(cx.err(
                    el.span(),
                    "static initialization blocks are not supported yet",
                ));
            }
            ClassElement::AccessorProperty(_) => {
                return Err(cx.err(
                    el.span(),
                    "accessor properties (`accessor` keyword) are not supported yet",
                ));
            }
        }
    }
    Ok(())
}

/// The class-scope slot holding the computed key of the field at `span`.
fn field_key_slot(
    cx: &FnCtx<'_, '_, '_, '_>,
    scope_idx: usize,
    span: Span,
) -> Result<u16, CompileError> {
    cx.comp.class_scopes[scope_idx]
        .field_keys
        .iter()
        .find_map(|&(s, slot)| (s == span).then_some(slot))
        .ok_or_else(|| cx.err(span, "internal: computed field key missing from class scope"))
}

/// Body of the instance-members initializer (ES §7.3.35
/// `InitializeInstanceElements`) on `this`: instance private methods and
/// accessors first, then every instance field in declaration order.
pub(crate) fn emit_instance_initializer(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    c: &Class<'_>,
) -> Result<(), CompileError> {
    for d in private_decls(&c.body) {
        if d.is_static || d.kind == v12_bytecode::private_kind::FIELD {
            continue;
        }
        let name = load_private_name(cx, &d.name[1..], c.span)?;
        let undef = cx.new_temp();
        cx.load_undefined(undef, c.span);
        let words = v12_bytecode::WideOp::DefinePrivateW {
            obj: REG_THIS,
            name,
            value: undef,
        }
        .encode();
        cx.emit_words(words, c.span);
    }
    emit_field_definitions(cx, &c.body, false)
}

/// Body of the static initializer: every static field in order, `this` = `F`.
pub(crate) fn emit_static_initializer(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    c: &Class<'_>,
) -> Result<(), CompileError> {
    emit_field_definitions(cx, &c.body, true)
}

/// `DefineField(this, field)` (ES §7.3.33) for each field of the given
/// `static`-ness: `PrivateFieldAdd` for a private one, else
/// `CreateDataPropertyOrThrow`.
fn emit_field_definitions(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    body: &ClassBody<'_>,
    is_static: bool,
) -> Result<(), CompileError> {
    for p in fields(body, is_static) {
        let value = if let Some(v) = &p.value {
            apply_field_function_name(cx, &p.key, p.computed, v);
            cx.expr(v)?
        } else {
            let d = cx.new_temp();
            cx.load_undefined(d, p.span);
            d
        };
        if let PropertyKey::PrivateIdentifier(id) = &p.key {
            let name = load_private_name(cx, &id.name, p.span)?;
            let words = v12_bytecode::WideOp::DefinePrivateW {
                obj: REG_THIS,
                name,
                value,
            }
            .encode();
            cx.emit_words(words, p.span);
            continue;
        }
        let key = if p.computed {
            let scope_idx = cx
                .comp
                .class_scopes
                .len()
                .checked_sub(1)
                .ok_or_else(|| cx.err(p.span, "internal: field outside a class scope"))?;
            let slot = field_key_slot(cx, scope_idx, p.span)?;
            load_class_slot(cx, scope_idx, slot, p.span)?
        } else {
            property_key_reg(cx, &p.key, false, p.span)?
        };
        cx.emit_reg3(Opcode::DefineField, REG_THIS, key, value, p.span);
    }
    Ok(())
}

/// Compiles a method's function unit and returns the register with its closure.
fn method_fn(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    m: &oxc_ast::ast::MethodDefinition<'_>,
) -> Result<u16, CompileError> {
    let idx = cx
        .planned_index(m.value.span)
        .map_err(|_| cx.err(m.span, "internal: class method missing from plans"))?;
    crate::unit::compile_unit(cx.comp, idx, crate::unit::UnitNode::Method(&m.value))?;
    let d = cx.new_temp();
    cx.emit_closure_instr(d, idx, m.value.span)?;
    Ok(d)
}

/// Applies spec `SetFunctionName` for a class field whose initializer is an
/// anonymous function definition (`DefineField` step 7: `static f =
/// function() {}`, `x = () => ...`, `static c = class {}` name the value
/// after the field when it carries no own `name`).
///
/// Implemented at compile time: a syntactically anonymous function never has
/// an own `name` (a named function expression keeps its own via `collect`),
/// so the planned unit's `function_name` is stamped *before* `cx.expr`
/// compiles it. Must run before the initializer is lowered. Computed keys
/// stay unresolved (accepted gap); private fields use their `#name` text.
pub(crate) fn apply_field_function_name(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    key: &PropertyKey<'_>,
    computed: bool,
    init: &Expression<'_>,
) {
    if computed {
        return;
    }
    let Some(name) = static_key_text(key) else {
        return;
    };
    apply_function_name(cx, &name, init);
}

/// Applies spec `SetFunctionName` for any binding/assignment whose value is a
/// syntactically anonymous function definition (`IsAnonymousFunctionDefinition`
/// → `SetFunctionName(v, name)`). Must run before the initializer is lowered:
/// the planned unit's `function_name` is stamped, and `Closure` installs the
/// own `name` property from it. A named function expression already carries
/// its own name and is left untouched.
pub(crate) fn apply_function_name(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    name: &str,
    init: &Expression<'_>,
) {
    let Some(span) = anonymous_function_span(init) else {
        return;
    };
    if let Ok(idx) = cx.planned_index(span)
        && cx.comp.plans.units[idx].function_name.is_none()
    {
        cx.comp.plans.units[idx].function_name = Some(name.to_string());
    }
}

/// The span of the planned unit for `init` when it is a syntactically
/// anonymous function definition, peeling transparent wrappers. `None` for
/// non-function initializers and named definitions.
pub(crate) fn anonymous_function_span(init: &Expression<'_>) -> Option<Span> {
    let mut e = init;
    loop {
        match e {
            Expression::ParenthesizedExpression(p) => e = &p.expression,
            Expression::TSAsExpression(p) => e = &p.expression,
            Expression::TSSatisfiesExpression(p) => e = &p.expression,
            Expression::TSNonNullExpression(p) => e = &p.expression,
            _ => break,
        }
    }
    match e {
        Expression::FunctionExpression(f) if f.id.is_none() => Some(f.span),
        Expression::ArrowFunctionExpression(a) => Some(a.span),
        Expression::ClassExpression(c) if c.id.is_none() => Some(c.span),
        _ => None,
    }
}

/// Evaluates a property key into a register: static keys load the interned
/// text; computed keys evaluate the expression.
pub(crate) fn property_key_reg(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    key: &PropertyKey<'_>,
    computed: bool,
    span: Span,
) -> Result<u16, CompileError> {
    if !computed {
        let text =
            static_key_text(key).ok_or_else(|| cx.err(span, "unsupported class property key"))?;
        let d = cx.new_temp();
        cx.load_str(d, &text, span)?;
        Ok(d)
    } else {
        let Some(e) = key.as_expression() else {
            return Err(cx.err(span, "unsupported computed class property key"));
        };
        cx.expr(e)
    }
}
