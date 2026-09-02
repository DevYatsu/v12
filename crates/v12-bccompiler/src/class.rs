//! Class lowering: `class` declarations and expressions compile to existing
//! bytecode constructs.
//!
//! A class is desugared into:
//! 1. A constructor **function unit** — its body is the explicit `constructor`
//!    body, or a default that forwards to `super(...args)` when the class
//!    extends and returns nothing otherwise.
//! 2. A prototype object (fresh ordinary object) linked to the constructor via
//!    `prototype` / `constructor` properties.
//! 3. Instance methods defined on the prototype; static methods on the
//!    constructor itself; getters/setters via `DefineAccessor`.
//! 4. `extends` wires `[[Prototype]]`: the constructor's prototype becomes the
//!    parent constructor (static inheritance), and the prototype object's
//!    prototype becomes `Parent.prototype` (instance inheritance).

use oxc_ast::ast::{Class, ClassBody, ClassElement, MethodDefinitionKind, PropertyKey};
use oxc_span::{GetSpan, Span};
use v12_bytecode::{Const, Opcode};

use crate::expr::static_key_text;
use crate::model::{CompileError, FnCtx};

/// Fixed environment slots for the class scope captured by the constructor
/// and method closures. Only allocated when the class has a heritage.
pub(crate) const SLOT_SUPER_CTOR: u16 = 0;
pub(crate) const SUPER_ENV_SLOTS: u16 = 1;

/// Lowers a class expression, returning the register holding the constructor.
pub(crate) fn class_expression(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    c: &Class<'_>,
    is_statement: bool,
) -> Result<u16, CompileError> {
    let span = c.span;
    let has_super = c.heritage.is_some();

    // 0. The class scope environment: holds the parent constructor for
    //    `super`. Created *before* any closure so the constructor and every
    //    method capture it; populated once `extends` has been evaluated.
    //    `NewEnvironment` installs it as the current frame's innermost env
    //    (no destination register).
    if has_super {
        cx.emit_new_env(0, SUPER_ENV_SLOTS, span);
    }

    // 1. Compile the constructor unit (registered during collect) and create
    //    its closure — the class function itself.
    let ctor_idx = cx
        .planned_index(span)
        .map_err(|_| cx.err(span, "internal: class missing from plans"))?;
    crate::unit::compile_unit(cx.comp, ctor_idx, crate::unit::UnitNode::Class(c))?;
    let ctor = cx.new_temp();
    cx.emit_closure(ctor, u16::try_from(ctor_idx).unwrap_or(0), span);

    // 2. Evaluate `extends` (parent constructor) before the prototype exists.
    let parent = if let Some(h) = &c.heritage {
        cx.expr(&h.expression)?
    } else {
        let p = cx.new_temp();
        cx.load_undefined(p, span);
        p
    };
    if has_super {
        cx.emit_set_env(0, SLOT_SUPER_CTOR, parent, span);
    }

    // 4. Create the prototype object and link it to the constructor.
    let proto = cx.new_temp();
    cx.emit_reg3(Opcode::NewObject, proto, 0, 0, span);
    let proto_key = cx.load_str_key("prototype", span)?;
    let ctor_key = cx.load_str_key("constructor", span)?;
    // constructor.prototype = proto
    cx.emit_reg3(Opcode::SetProperty, ctor, proto_key, proto, span);
    // proto.constructor = constructor
    cx.emit_reg3(Opcode::SetProperty, proto, ctor_key, ctor, span);

    // 5. Wire `extends` prototype chains.
    if has_super {
        // Check if parent is null (extends null) - in that case don't wire static/prototype inheritance
        let parent_is_null = cx.new_temp();
        let null_reg = cx.new_temp();
        cx.load_const(null_reg, Const::Null, span).unwrap();
        cx.emit_reg3(Opcode::StrictEq, parent_is_null, parent, null_reg, span);
        let skip_wire = cx.label();
        cx.emit_jump(Opcode::JumpIfTrue, parent_is_null, skip_wire);
        // Constructor [[Prototype]] = parent constructor (static inheritance).
        cx.emit_reg3(Opcode::SetPrototype, 0, ctor, parent, span);
        // proto [[Prototype]] = parent.prototype
        let parent_proto_key = cx.load_str_key("prototype", span)?;
        let pp = cx.new_temp();
        cx.emit_reg3(Opcode::GetProperty, pp, parent, parent_proto_key, span);
        cx.emit_reg3(Opcode::SetPrototype, 0, proto, pp, span);
        cx.bind(skip_wire);
    }

    // 6. Define the class elements (methods, getters/setters, statics).
    define_elements(cx, &c.body, ctor, proto)?;

    // Class declarations bind the constructor to the class name.
    if is_statement
        && let Some(id) = &c.id
        && let Some(sym) = id.symbol_id.get()
    {
        let access = cx.access(sym);
        cx.store_access(access, ctor, id.span);
    }

    Ok(ctor)
}

/// Defines every class element on the prototype (`proto`) or the constructor
/// (`ctor`) depending on its `static` modifier.
fn define_elements(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    body: &ClassBody<'_>,
    ctor: u16,
    proto: u16,
) -> Result<(), CompileError> {
    for el in &body.body {
        match el {
            ClassElement::MethodDefinition(m) => {
                let target = if m.r#static { ctor } else { proto };
                let is_ctor = m.kind == MethodDefinitionKind::Constructor;
                let key_reg = property_key_reg(cx, &m.key, m.computed, m.span)?;
                // private methods: store as private on ctor
                let is_private = matches!(&m.key, PropertyKey::PrivateIdentifier(_));
                if is_private {
                    let name = match &m.key { PropertyKey::PrivateIdentifier(id) => format!("#{}", id.name), _ => unreachable!() };
                    let name_id = crate::model::str_id_of(cx.comp.strings.get_or_intern(&name));
                    let fn_reg = method_fn(cx, m)?;
                    let target_obj = if m.r#static { ctor } else { ctor };
                    let words = v12_bytecode::WideOp::DefinePrivateW { obj: target_obj, class_id: 0, name_id, value: fn_reg }.encode();
                    cx.emit_words(words, m.span);
                    continue;
                }
                match m.kind {
                    MethodDefinitionKind::Get => {
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
                    _ => {
                        // Constructor is the class function itself; do not
                        // redefine it on the prototype.
                        if !is_ctor {
                            let fn_reg = method_fn(cx, m)?;
                            cx.emit_reg3(Opcode::SetProperty, target, key_reg, fn_reg, m.span);
                        }
                    }
                }
            }
            ClassElement::PropertyDefinition(p) => {
                let is_private = matches!(&p.key, PropertyKey::PrivateIdentifier(_));
                if is_private {
                    let name = match &p.key { PropertyKey::PrivateIdentifier(id) => format!("#{}", id.name), _ => unreachable!() };
                    let name_id = crate::model::str_id_of(cx.comp.strings.get_or_intern(&name));
                    let value_reg = if let Some(v) = &p.value { cx.expr(v)? } else { let d = cx.new_temp(); cx.load_undefined(d, p.span); d };
                    if p.r#static {
                        let words = v12_bytecode::WideOp::DefinePrivateW { obj: ctor, class_id: 0, name_id, value: value_reg }.encode();
                        cx.emit_words(words, p.span);
                    } else {
                        // instance private field: store as template on ctor (will be cloned on construct)
                        // For now define on ctor template; Construct will clone to instances
                        let words = v12_bytecode::WideOp::DefinePrivateW { obj: ctor, class_id: 0, name_id, value: value_reg }.encode();
                        cx.emit_words(words, p.span);
                    }
                    continue;
                }
                let target = if p.r#static { ctor } else { proto };
                let key_reg = property_key_reg(cx, &p.key, p.computed, p.span)?;
                let value_reg = if let Some(v) = &p.value {
                    cx.expr(v)?
                } else {
                    let d = cx.new_temp();
                    cx.load_undefined(d, p.span);
                    d
                };
                cx.emit_reg3(Opcode::SetProperty, target, key_reg, value_reg, p.span);
            }
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
            ClassElement::TSIndexSignature(_) => {}
        }
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
    cx.emit_closure(d, u16::try_from(idx).unwrap_or(0), m.value.span);
    Ok(d)
}

/// Evaluates a property key into a register: static keys load the interned
/// text; computed keys evaluate the expression.
fn property_key_reg(
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
