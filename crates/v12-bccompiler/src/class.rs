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

use oxc_ast::ast::{Class, ClassBody, ClassElement, Expression, MethodDefinitionKind, PropertyKey};
use oxc_span::{GetSpan, Span};
use v12_bytecode::{Const, Opcode};

use crate::expr::static_key_text;
use crate::model::{CompileError, FnCtx};

/// Fixed environment slots for the class scope captured by the constructor
/// and method closures. Allocated when the class has a heritage or declares
/// private names.
pub(crate) const SLOT_SUPER_CTOR: u16 = 0;
/// The class scope's first Private Name slot; name `i` lives at `+ i`.
pub(crate) const SLOT_FIRST_PRIVATE_NAME: u16 = 1;

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

/// Loads the Private Name `#name` (given without the `#`) into a fresh
/// register, resolving it to the innermost enclosing class that declares it.
///
/// The name lives in that class's scope environment. The hop count is the
/// current unit's live class scopes, plus — for every unit between here and
/// the class's outer unit — its own environment and the class scopes it was
/// created under, minus the scopes the outer unit had pushed *above* the
/// class's (none of which the class's closures captured).
pub(crate) fn load_private_name(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    name: &str,
    span: Span,
) -> Result<u16, CompileError> {
    let text = format!("#{name}");
    let Some((scope_idx, index)) = cx
        .comp
        .private_scopes
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
    let scope = &cx.comp.private_scopes[scope_idx];
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
    let depth = u16::try_from(depth)
        .map_err(|_| cx.err(span, "private name environment depth out of range"))?;
    let slot = u16::try_from(index)
        .ok()
        .and_then(|i| i.checked_add(SLOT_FIRST_PRIVATE_NAME))
        .ok_or_else(|| cx.err(span, "too many private names in one class"))?;
    let dst = cx.new_temp();
    cx.emit_get_env(dst, depth, slot, span);
    Ok(dst)
}

/// Lowers a class expression, returning the register holding the constructor.
pub(crate) fn class_expression(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    c: &Class<'_>,
    is_statement: bool,
) -> Result<u16, CompileError> {
    let span = c.span;
    let has_super = c.heritage.is_some();
    let privates = private_decls(&c.body);
    let has_scope = has_super || !privates.is_empty();

    // 0. The class scope environment: holds the parent constructor for
    //    `super`. Created *before* any closure so the constructor and every
    //    method capture it; populated once `extends` has been evaluated.
    //    `NewEnvironment` installs it as the current frame's innermost env
    //    (no destination register).
    //
    //    It is *scoped*: step 7 retires it with `PopEnvironment`. Leaving it
    //    in place let it shadow the enclosing function's own environment for
    //    the rest of the unit, so every later `GetEnvSlot`/`SetEnvSlot` at
    //    `depth=0` — and every closure created after the class, which captures
    //    the frame's innermost env as its root — resolved one hop too shallow.
    //    That was silent, not an error: the scope holds one slot (the parent
    //    constructor), so a misdirected access read the constructor back.
    //
    //    `emit_scoped_env` also records the push in `env_bias`, because the
    //    heritage expression and the computed keys in steps 2 and 6 are
    //    evaluated while this environment is the frame's innermost one.
    if has_scope {
        let slots = u16::try_from(privates.len())
            .ok()
            .and_then(|n| n.checked_add(SLOT_FIRST_PRIVATE_NAME))
            .ok_or_else(|| cx.err(span, "too many private names in one class"))?;
        cx.emit_scoped_env(0, slots, span);
    }
    //    Every `#name` gets a fresh Private Name per evaluation of the class
    //    (ES §15.7.14 step 6), before any element closure exists.
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
    let bias_level = cx.comp.live_env_bias;
    cx.comp.private_scopes.push(crate::model::PrivateScope {
        names: privates.iter().map(|d| d.name.clone()).collect(),
        outer_unit: cx.unit,
        bias_level,
    });

    // 1. Compile the constructor unit (registered during collect) and create
    //    its closure — the class function itself.
    let ctor_idx = cx
        .planned_index(span)
        .map_err(|_| cx.err(span, "internal: class missing from plans"))?;
    crate::unit::compile_unit(cx.comp, ctor_idx, crate::unit::UnitNode::Class(c))?;
    let ctor = cx.new_temp();
    cx.emit_closure_instr(ctor, ctor_idx, span)?;

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
    cx.emit_reg3(Opcode::DefineMethod, ctor, proto_key, proto, span);
    // proto.constructor = constructor
    cx.emit_reg3(Opcode::DefineMethod, proto, ctor_key, ctor, span);

    // 5. Wire `extends` prototype chains.
    if has_super {
        // Check if parent is null (extends null) - in that case don't wire static/prototype inheritance
        let parent_is_null = cx.new_temp();
        let null_reg = cx.new_temp();
        cx.load_const(null_reg, Const::Null, span)?;
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
    cx.comp.private_scopes.pop();

    // 7. Retire the class scope. Everything the scope existed for is already
    //    built — the constructor and every method closure captured it, and the
    //    `super` slot is populated — so the rest of the unit runs against the
    //    enclosing function's own environment again. Done before the name
    //    store so that binding, like all following code, resolves at the
    //    structural depth the plan computed.
    if has_scope {
        cx.emit_pop_env(span);
    }

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
                if let PropertyKey::PrivateIdentifier(id) = &m.key {
                    // The function hangs off the Private Name: every object
                    // that later receives the name shares it. Static names are
                    // added to the constructor once all methods exist; instance
                    // names in `InitializeInstanceElements`.
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
                match m.kind {
                    MethodDefinitionKind::Get => {
                        // A CLASS accessor is non-enumerable (ES 15.7.6), so it
                        // keeps the plain `DefineAccessor`; the object-literal
                        // spelling emits `DefineAccessorEnumerable` instead
                        // (ADR-0005). Nothing here changed, which is what makes
                        // "the class case is unaffected" a statement about the
                        // emitted bytes and not only about the answers.
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
                            cx.emit_reg3(Opcode::DefineMethod, target, key_reg, fn_reg, m.span);
                        }
                    }
                }
            }
            // Fields run after every method exists (ES §15.7.14 steps 31–32):
            // see the second pass below.
            ClassElement::PropertyDefinition(_) => {}
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
    // Static private methods and accessors join the constructor before any
    // static field initializer can observe it.
    for d in private_decls(body) {
        if d.is_static && d.kind != v12_bytecode::private_kind::FIELD {
            let name = load_private_name(cx, &d.name[1..], body.span)?;
            let undef = cx.new_temp();
            cx.load_undefined(undef, body.span);
            let words = v12_bytecode::WideOp::DefinePrivateW {
                obj: ctor,
                name,
                value: undef,
            }
            .encode();
            cx.emit_words(words, body.span);
        }
    }
    for el in &body.body {
        let ClassElement::PropertyDefinition(p) = el else {
            continue;
        };
        if !p.r#static {
            continue;
        }
        let value_reg = if let Some(v) = &p.value {
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
                obj: ctor,
                name,
                value: value_reg,
            }
            .encode();
            cx.emit_words(words, p.span);
        } else {
            let key_reg = property_key_reg(cx, &p.key, p.computed, p.span)?;
            cx.emit_reg3(Opcode::SetProperty, ctor, key_reg, value_reg, p.span);
        }
    }
    Ok(())
}

/// The instance half of `InitializeInstanceElements` for private methods and
/// accessors (ES §7.3.35 step 2): adds each instance `#name` that names a
/// method or accessor to `this`, before any field initializer runs.
pub(crate) fn emit_instance_private_methods(
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
            obj: crate::model::REG_THIS,
            name,
            value: undef,
        }
        .encode();
        cx.emit_words(words, c.span);
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
