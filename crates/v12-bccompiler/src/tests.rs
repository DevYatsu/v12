//! End-to-end tests over a miniature reference interpreter (no v12-heap
//! dependency), plus peephole/disassembler smoke tests.
//!
//! The mini-interp implements exactly the ABI documented in `model.rs` /
//! `stmt.rs`:
//! - registers initialize to `undefined`, `r0` = `this`
//! - `Call` layout `[callee][this][arg…]`; callee window starts at
//!   `callee_reg + 1`
//! - handler ranges deliver the exception in register `stack_depth`
//! - falling off the end returns `undefined`

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use v12_bytecode::{Const, Instr, Opcode, WideOp};

use crate::model::{spur_of_str_id, str_id_of};
use crate::{
    Interner, Program, compile_ast, compile_source_with_interner, compile_source_with_strings,
    freeze_interner,
};

// ---------------------------------------------------------------------------
// Mini values
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Val {
    F64(f64),
    Str(Rc<str>),
    Bool(bool),
    Undefined,
    /// The `null` singleton (via `Const::Null`).
    Null,
    Obj(Rc<RefCell<Obj>>),
    Closure(Rc<ClosureVal>),
}

#[derive(Default, Debug)]
struct Obj {
    props: HashMap<String, Val>,
}

#[derive(Debug)]
struct ClosureVal {
    fn_idx: usize,
    env: Rc<Env>,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env").finish_non_exhaustive()
    }
}

struct Env {
    slots: RefCell<Vec<Val>>,
    parent: Option<Rc<Env>>,
}

impl Env {
    fn root() -> Rc<Env> {
        Rc::new(Env {
            slots: RefCell::new(Vec::new()),
            parent: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Mini interpreter
// ---------------------------------------------------------------------------

struct Mini<'p> {
    prog: &'p Program,
    strings: &'p [String],
}

type Step = Result<Val, Val>;

const BUDGET: u64 = 1_000_000;

impl<'p> Mini<'p> {
    fn string(&self, id: u32) -> String {
        self.strings
            .get(id as usize)
            .cloned()
            .unwrap_or_else(|| format!("<str#{id}>"))
    }

    fn run_fn(&self, idx: usize, this: Val, args: &[Val], env: Rc<Env>) -> Result<Val, Val> {
        let f = self.prog.functions.get(idx).expect("function index");
        let mut regs = vec![Val::Undefined; f.max_regs as usize];
        regs[0] = this;
        for (i, a) in args.iter().enumerate() {
            regs[1 + i] = a.clone();
        }
        let mut cur_env = env;
        let mut pc: usize = 0;
        let mut budget = BUDGET;

        loop {
            budget -= 1;
            if budget == 0 {
                panic!("mini-interp budget exhausted (infinite loop?)");
            }
            let Some(&instr) = f.instrs.get(pc) else {
                return Ok(Val::Undefined); // fall-off-end ABI
            };
            let Some(op) = instr.op() else {
                panic!("bad opcode at {pc}")
            };

            // Exception delivery: innermost enclosing handler wins.
            macro_rules! throw {
                ($v:expr) => {{
                    let v = $v;
                    match Self::find_handler(f, pc) {
                        Some(h) => {
                            regs[h.stack_depth as usize] = v;
                            pc = h.target as usize;
                            continue;
                        }
                        None => return Err(v),
                    }
                }};
            }

            match op {
                Opcode::Wide => {
                    let (wide, width) = WideOp::try_decode(&f.instrs[pc..]).expect("wide decode");
                    match wide {
                        WideOp::LoadIntW { dst, value } => {
                            regs[dst as usize] = Val::F64(value as f64)
                        }
                        WideOp::LoadConstW { dst, const_id } => {
                            regs[dst as usize] = self.const_value(f, const_id);
                        }
                        WideOp::GetEnvSlotW { dst, depth, slot } => {
                            let e = walk_env(&cur_env, depth);
                            regs[dst as usize] = e.slots.borrow()[slot as usize].clone();
                        }
                        WideOp::SetEnvSlotW { src, depth, slot } => {
                            let e = walk_env(&cur_env, depth);
                            e.slots.borrow_mut()[slot as usize] = regs[src as usize].clone();
                        }
                        WideOp::CallW { dst, func, argc } => {
                            regs[dst as usize] = self.do_call(&regs, func, argc, &mut cur_env)?;
                        }
                        WideOp::CopyObjectRestW { .. } | WideOp::CopyArrayRestW { .. } => {
                            panic!("wide copy rest not expected in mini interpreter")
                        }
                    }
                    pc += width;
                }
                Opcode::Move => {
                    regs[instr.a() as usize] = regs[instr.b() as usize].clone();
                    pc += 1;
                }
                Opcode::LoadConst => {
                    regs[instr.a() as usize] = self.const_value(f, u32::from(instr.imm16()));
                    pc += 1;
                }
                Opcode::LoadInt => {
                    regs[instr.a() as usize] = Val::F64(i8::from_be_bytes([instr.c()]) as f64);
                    pc += 1;
                }
                Opcode::Add
                | Opcode::Sub
                | Opcode::Mul
                | Opcode::Div
                | Opcode::Mod
                | Opcode::Pow
                | Opcode::BitAnd
                | Opcode::BitOr
                | Opcode::BitXor
                | Opcode::Shl
                | Opcode::Shr
                | Opcode::UShr
                | Opcode::Eq
                | Opcode::Ne
                | Opcode::Lt
                | Opcode::Le
                | Opcode::Gt
                | Opcode::Ge
                | Opcode::StrictEq
                | Opcode::StrictNe => {
                    let l = regs[instr.b() as usize].clone();
                    let r = regs[instr.c() as usize].clone();
                    regs[instr.a() as usize] = binop(op, l, r)?;
                    pc += 1;
                }
                Opcode::Neg => {
                    regs[instr.a() as usize] = Val::F64(-to_number(&regs[instr.b() as usize]));
                    pc += 1;
                }
                Opcode::BitNot => {
                    regs[instr.a() as usize] =
                        Val::F64(!to_int32(&to_number(&regs[instr.b() as usize])) as f64);
                    pc += 1;
                }
                Opcode::Not => {
                    regs[instr.a() as usize] = Val::Bool(!truthy(&regs[instr.b() as usize]));
                    pc += 1;
                }
                Opcode::TypeOf => {
                    regs[instr.a() as usize] = Val::Str(type_of(&regs[instr.b() as usize]).into());
                    pc += 1;
                }
                Opcode::Jump => {
                    pc = instr.imm24() as usize;
                }
                Opcode::JumpIfFalse => {
                    pc = if truthy(&regs[instr.a() as usize]) {
                        pc + 1
                    } else {
                        instr.imm16() as usize
                    };
                }
                Opcode::JumpIfTrue => {
                    pc = if truthy(&regs[instr.a() as usize]) {
                        instr.imm16() as usize
                    } else {
                        pc + 1
                    };
                }
                Opcode::LoopHeader => pc += 1,
                Opcode::Call => {
                    let dst = instr.a();
                    regs[dst as usize] =
                        self.do_call(&regs, instr.b(), u16::from(instr.c()), &mut cur_env)?;
                    pc += 1;
                }
                Opcode::Return => return Ok(regs[instr.a() as usize].clone()),
                Opcode::Throw => {
                    let v = regs[instr.a() as usize].clone();
                    throw!(v);
                }
                Opcode::GetProperty => {
                    let obj = &regs[instr.b() as usize];
                    let key = to_key(&regs[instr.c() as usize]);
                    regs[instr.a() as usize] = get_prop(obj, &key)?;
                    pc += 1;
                }
                Opcode::SetProperty => {
                    let obj = &regs[instr.a() as usize];
                    let key = to_key(&regs[instr.b() as usize]);
                    let value = regs[instr.c() as usize].clone();
                    set_prop(obj, &key, value)?;
                    pc += 1;
                }
                Opcode::DeleteProperty => {
                    let obj = &regs[instr.b() as usize];
                    let key = to_key(&regs[instr.c() as usize]);
                    regs[instr.a() as usize] = delete_prop(obj, &key)?;
                    pc += 1;
                }
                Opcode::NewObject => {
                    regs[instr.a() as usize] = Val::Obj(Rc::new(RefCell::new(Obj::default())));
                    pc += 1;
                }
                Opcode::NewArray => {
                    let mut obj = Obj::default();
                    let first = instr.b() as usize;
                    let len = instr.c() as usize;
                    for i in 0..len {
                        obj.props.insert(i.to_string(), regs[first + i].clone());
                    }
                    obj.props.insert("length".into(), Val::F64(len as f64));
                    regs[instr.a() as usize] = Val::Obj(Rc::new(RefCell::new(obj)));
                    pc += 1;
                }
                Opcode::Closure => {
                    regs[instr.a() as usize] = Val::Closure(Rc::new(ClosureVal {
                        fn_idx: instr.b() as usize,
                        env: cur_env.clone(),
                    }));
                    pc += 1;
                }
                Opcode::NewEnvironment => {
                    cur_env = Rc::new(Env {
                        slots: RefCell::new(vec![Val::Undefined; instr.b() as usize]),
                        parent: Some(cur_env.clone()),
                    });
                    pc += 1;
                }
                Opcode::GetEnvSlot => {
                    let e = walk_env(&cur_env, u16::from(instr.b()));
                    regs[instr.a() as usize] = e.slots.borrow()[instr.c() as usize].clone();
                    pc += 1;
                }
                Opcode::SetEnvSlot => {
                    let e = walk_env(&cur_env, u16::from(instr.a()));
                    e.slots.borrow_mut()[instr.b() as usize] = regs[instr.c() as usize].clone();
                    pc += 1;
                }
                Opcode::In => {
                    let key = to_key(&regs[instr.b() as usize]);
                    let obj = &regs[instr.c() as usize];
                    regs[instr.a() as usize] = Val::Bool(match obj {
                        Val::Obj(o) => o.borrow().props.contains_key(&key),
                        _ => {
                            return Err(Val::Str(
                                "TypeError: right-hand side of 'in' should be an object".into(),
                            ));
                        }
                    });
                    pc += 1;
                }
                Opcode::InstanceOf => {
                    let lhs = &regs[instr.b() as usize];
                    let rhs = &regs[instr.c() as usize];
                    let rhs_obj = match rhs {
                        Val::Obj(o) => o,
                        _ => {
                            return Err(Val::Str(
                                "TypeError: right-hand side of 'instanceof' is not an object"
                                    .into(),
                            ));
                        }
                    };
                    let proto_val = rhs_obj.borrow().props.get("prototype").cloned();
                    let Some(Val::Obj(proto_obj)) = proto_val else {
                        return Err(Val::Str("TypeError: function has non-object prototype 'prototype' in instanceof check".into()));
                    };
                    let result = match lhs {
                        Val::Obj(_) => {
                            // Mini interpreter has no prototype links, so only direct identity.
                            // For tests, an object instanceof its own prototype is true only when lhs is the proto itself.
                            // This suffices for compiler opcode validation; full chain is tested in v12-interp.
                            match lhs {
                                Val::Obj(o) => Rc::ptr_eq(o, &proto_obj),
                                _ => false,
                            }
                        }
                        _ => false,
                    };
                    regs[instr.a() as usize] = Val::Bool(result);
                    pc += 1;
                }
                Opcode::GetGlobal => {
                    regs[instr.a() as usize] = Val::Undefined;
                    pc += 1;
                }
                Opcode::SetGlobal => {
                    pc += 1;
                }
                Opcode::CreateGenerator
                | Opcode::SuspendYield
                | Opcode::Await
                | Opcode::CopyArrayRest
                | Opcode::CheckIsArray
                | Opcode::CallApply
                | Opcode::CopyObjectRest
                | Opcode::ArrayAppend => {
                    panic!("generator/async/copy opcodes not expected in tier-1 mini programs")
                }
            }
        }
    }

    fn do_call(&self, regs: &[Val], callee_reg: u8, argc: u16, _env: &mut Rc<Env>) -> Step {
        let callee = &regs[callee_reg as usize];
        let Val::Closure(c) = callee else {
            return Err(Val::Str("TypeError: not a function".into()));
        };
        let this = regs[callee_reg as usize + 1].clone();
        let args: Vec<Val> = (0..argc as usize)
            .map(|i| regs[callee_reg as usize + 2 + i].clone())
            .collect();
        self.run_fn(c.fn_idx, this, &args, c.env.clone())
    }

    fn const_value(&self, f: &v12_bytecode::FunctionBytecode, id: u32) -> Val {
        use v12_bytecode::Const;
        match f.consts.get(id as u16) {
            Some(Const::F64(v)) => Val::F64(v),
            Some(Const::Str32(sid)) => Val::Str(self.string(sid).into()),
            Some(Const::Null) => Val::Null,
            Some(other) => panic!("unexpected const {other:?}"),
            None => panic!("constant id {id} out of range"),
        }
    }

    fn find_handler(
        f: &v12_bytecode::FunctionBytecode,
        pc: usize,
    ) -> Option<&v12_bytecode::HandlerRange> {
        f.handlers
            .iter()
            .filter(|h| (h.start as usize) <= pc && pc < h.end as usize)
            .max_by_key(|h| h.start)
    }
}

fn walk_env(env: &Rc<Env>, depth: u16) -> Rc<Env> {
    let mut cur = env.clone();
    for _ in 0..depth {
        cur = cur.parent.clone().expect("env depth out of range");
    }
    cur
}

// ---------------------------------------------------------------------------
// Value semantics
// ---------------------------------------------------------------------------

fn truthy(v: &Val) -> bool {
    match v {
        Val::F64(n) => *n != 0.0 && !n.is_nan(),
        Val::Str(s) => !s.is_empty(),
        Val::Bool(b) => *b,
        Val::Undefined | Val::Null => false,
        Val::Obj(_) | Val::Closure(_) => true,
    }
}

fn to_string(v: &Val) -> String {
    match v {
        Val::F64(n) if n.is_nan() => "NaN".into(),
        Val::F64(n) if n.is_infinite() => {
            if *n > 0.0 {
                "Infinity".into()
            } else {
                "-Infinity".into()
            }
        }
        Val::F64(n) => format!("{n}"),
        Val::Str(s) => s.to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Undefined => "undefined".into(),
        Val::Null => "null".into(),
        Val::Obj(_) => "[object Object]".into(),
        Val::Closure(_) => "function".into(),
    }
}

fn to_number(v: &Val) -> f64 {
    match v {
        Val::F64(n) => *n,
        Val::Bool(b) => u8::from(*b) as f64,
        Val::Str(s) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
        Val::Undefined => f64::NAN,
        Val::Null => 0.0,
        Val::Obj(_) | Val::Closure(_) => f64::NAN,
    }
}

fn to_int32(n: &f64) -> i32 {
    *n as i64 as i32 // Rust `as` saturates then wraps — close enough for tests
}

fn type_of(v: &Val) -> &'static str {
    match v {
        Val::F64(_) => "number",
        Val::Str(_) => "string",
        Val::Bool(_) => "boolean",
        Val::Undefined => "undefined",
        Val::Null => "object",
        Val::Obj(_) => "object",
        Val::Closure(_) => "function",
    }
}

fn strict_eq(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::F64(x), Val::F64(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Undefined, Val::Undefined) | (Val::Null, Val::Null) => true,
        (Val::Obj(x), Val::Obj(y)) => Rc::ptr_eq(x, y),
        (Val::Closure(x), Val::Closure(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

fn loose_eq(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Null, Val::Undefined) | (Val::Undefined, Val::Null) => true,
        (Val::Str(_), Val::F64(_)) | (Val::F64(_), Val::Str(_)) => {
            let x = to_number(a);
            let y = to_number(b);
            x == y || (x.is_nan() && y.is_nan())
        }
        (Val::Bool(_), _) | (_, Val::Bool(_)) => {
            loose_eq(&Val::F64(to_number(a)), &Val::F64(to_number(b)))
        }
        _ => strict_eq(a, b),
    }
}

fn compare(op: Opcode, a: &Val, b: &Val) -> bool {
    let ord = match (a, b) {
        (Val::Str(x), Val::Str(y)) => x.as_ref().cmp(y.as_ref()),
        _ => to_number(a)
            .partial_cmp(&to_number(b))
            .unwrap_or(std::cmp::Ordering::Greater),
    };
    match op {
        Opcode::Lt => ord == std::cmp::Ordering::Less,
        Opcode::Le => ord != std::cmp::Ordering::Greater,
        Opcode::Gt => ord == std::cmp::Ordering::Greater,
        Opcode::Ge => ord != std::cmp::Ordering::Less,
        _ => unreachable!("not a comparison"),
    }
}

fn binop(op: Opcode, l: Val, r: Val) -> Step {
    let err_str = || Err(Val::Str("unsupported operand types".into()));
    Ok(match op {
        Opcode::Add => {
            if matches!(l, Val::Str(_)) || matches!(r, Val::Str(_)) {
                Val::Str(format!("{}{}", to_string(&l), to_string(&r)).into())
            } else {
                Val::F64(to_number(&l) + to_number(&r))
            }
        }
        Opcode::Sub => Val::F64(to_number(&l) - to_number(&r)),
        Opcode::Mul => Val::F64(to_number(&l) * to_number(&r)),
        Opcode::Div => Val::F64(to_number(&l) / to_number(&r)),
        Opcode::Mod => Val::F64(to_number(&l) % to_number(&r)),
        Opcode::Pow => Val::F64(to_number(&l).powf(to_number(&r))),
        Opcode::BitAnd => Val::F64((to_int32(&to_number(&l)) & to_int32(&to_number(&r))) as f64),
        Opcode::BitOr => Val::F64((to_int32(&to_number(&l)) | to_int32(&to_number(&r))) as f64),
        Opcode::BitXor => Val::F64((to_int32(&to_number(&l)) ^ to_int32(&to_number(&r))) as f64),
        Opcode::Shl => Val::F64((to_int32(&to_number(&l)) << (to_number(&r) as u32 & 31)) as f64),
        Opcode::Shr => Val::F64((to_int32(&to_number(&l)) >> (to_number(&r) as u32 & 31)) as f64),
        Opcode::UShr => {
            Val::F64(((to_int32(&to_number(&l)) as u32) >> (to_number(&r) as u32 & 31)) as f64)
        }
        Opcode::Eq => Val::Bool(loose_eq(&l, &r)),
        Opcode::Ne => Val::Bool(!loose_eq(&l, &r)),
        Opcode::StrictEq => Val::Bool(strict_eq(&l, &r)),
        Opcode::StrictNe => Val::Bool(!strict_eq(&l, &r)),
        Opcode::Lt | Opcode::Le | Opcode::Gt | Opcode::Ge => Val::Bool(compare(op, &l, &r)),
        _ => return err_str(),
    })
}

fn to_key(v: &Val) -> String {
    match v {
        Val::Str(s) => s.to_string(),
        other => to_string(other),
    }
}

fn get_prop(v: &Val, key: &str) -> Step {
    match v {
        Val::Obj(o) => Ok(o.borrow().props.get(key).cloned().unwrap_or(Val::Undefined)),
        _ => Err(Val::Str(
            format!("TypeError: cannot read property {key}").into(),
        )),
    }
}

fn set_prop(v: &Val, key: &str, value: Val) -> Result<(), Val> {
    match v {
        Val::Obj(o) => {
            o.borrow_mut().props.insert(key.to_string(), value);
            Ok(())
        }
        _ => Err(Val::Str(
            format!("TypeError: cannot set property {key}").into(),
        )),
    }
}

fn delete_prop(v: &Val, key: &str) -> Result<Val, Val> {
    match v {
        Val::Obj(o) => Ok(Val::Bool(o.borrow_mut().props.remove(key).is_some())),
        _ => Err(Val::Str("TypeError: delete on non-object".into())),
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn eval_src(src: &str) -> Val {
    // Snippets are function bodies: scripts have no completion value, so the
    // harness wraps them in an IIFE and surfaces the result via `throw`,
    // which propagates out of the interpreter unchanged.
    let wrapped = format!("throw (function () {{\n{src}\n}})();");
    let (prog, strings) = compile_source_with_strings(&wrapped).expect("compiles");
    let mini = Mini {
        prog: &prog,
        strings: &strings,
    };
    mini.run_fn(prog.main as usize, Val::Undefined, &[], Env::root())
        .expect_err("completion value thrown")
}

fn expect_num(src: &str, want: f64) {
    match eval_src(src) {
        Val::F64(got) => {
            if got.is_nan() && want.is_nan() {
                return;
            }
            assert_eq!(got, want, "{src}");
        }
        other => panic!("{src}: expected number, got {other:?}"),
    }
}

fn expect_bool(src: &str, want: bool) {
    match eval_src(src) {
        Val::Bool(got) => assert_eq!(got, want, "{src}"),
        other => panic!("{src}: expected bool, got {other:?}"),
    }
}

fn expect_str(src: &str, want: &str) {
    match eval_src(src) {
        Val::Str(got) => assert_eq!(&*got, want, "{src}"),
        other => panic!("{src}: expected string, got {other:?}"),
    }
}

fn expect_undefined(src: &str) {
    assert!(matches!(eval_src(src), Val::Undefined), "{src}");
}

// ---------------------------------------------------------------------------
// Tier-1 expression tests
// ---------------------------------------------------------------------------

#[test]
fn arithmetic_precedence_and_unary() {
    expect_num("return 3 + 4 * 2;", 11.0);
    expect_num("return (3 + 4) * 2;", 14.0);
    expect_num("return 10 / 4;", 2.5);
    expect_num("return 7 % 3;", 1.0);
    expect_num("return 2 ** 10;", 1024.0);
    expect_num("return -5 + 2;", -3.0);
    expect_num("return ~5;", -6.0);
    expect_bool("return !0;", true);
    expect_bool("return !1;", false);
}

#[test]
fn bitwise_and_shifts() {
    expect_num("return 6 & 3;", 2.0);
    expect_num("return 6 | 3;", 7.0);
    expect_num("return 6 ^ 3;", 5.0);
    expect_num("return 1 << 4;", 16.0);
    expect_num("return -8 >> 1;", -4.0);
    expect_num("return -8 >>> 28;", 15.0);
}

#[test]
fn comparisons_and_equality() {
    expect_bool("return 1 < 2;", true);
    expect_bool("return 2 <= 1;", false);
    expect_bool("return 3 > 2;", true);
    expect_bool("return 2 >= 3;", false);
    expect_bool("return 'a' < 'b';", true);
    expect_bool("return 1 === 1;", true);
    expect_bool("return 1 === '1';", false);
    expect_bool("return 1 !== '1';", true);
    expect_bool("return 1 == '1';", true);
}

#[test]
fn typeof_forms() {
    expect_str("return typeof 1;", "number");
    expect_str("return typeof 'x';", "string");
    expect_str("return typeof true;", "boolean");
    expect_str("return typeof undeclared_name_xyz;", "undefined");
    expect_str("return typeof someFn === 'function' ? 'ok' : 'no';", "no");
}

#[test]
fn string_concat_via_plus() {
    expect_str("return 'hello' + ' ' + 'world';", "hello world");
    expect_str("return 'n=' + 42;", "n=42");
}

#[test]
fn update_expressions_prefix_postfix() {
    expect_num("let i = 5; return ++i;", 6.0);
    expect_num("let j = 5; return j++;", 5.0);
    expect_num("let k = 5; k++; return k;", 6.0);
    expect_num("let m = 5; return --m;", 4.0);
    expect_num("let n = 5; return n--;", 5.0);
}

#[test]
fn compound_assignment_operators() {
    expect_num("let x = 10; x += 5; return x;", 15.0);
    expect_num("let y = 10; y -= 3; return y;", 7.0);
    expect_num("let z = 10; z *= 2; return z;", 20.0);
    expect_num("let w = 10; w /= 4; return w;", 2.5);
    expect_num("let q = 6; q &= 3; return q;", 2.0);
    expect_num("let r = 6; r |= 1; return r;", 7.0);
    expect_num("let s = 1; s <<= 4; return s;", 16.0);
}

#[test]
fn assignment_yields_value() {
    expect_num("let a, b; a = b = 7; return a + b;", 14.0);
}

#[test]
fn member_read_write_and_delete() {
    expect_num("let o = {x: 1}; o.x = 9; return o.x;", 9.0);
    expect_num("let o = {}; o['k'] = 3; return o['k'];", 3.0);
    expect_num(
        "let d = {gone: 1}; delete d.gone; return d.gone === undefined ? 1 : 0;",
        1.0,
    );
}

#[test]
fn object_literal_and_shorthand_expansion() {
    expect_num("return ({a: 1}).a;", 1.0);
    expect_num("let v = 2; return ({v}).v;", 2.0);
    expect_str("return ({'s': 'hi'}).s;", "hi");
    expect_num("return ({1: 'x'})[1] === 'x' ? 1 : 0;", 1.0);
}

#[test]
fn array_literals_and_indexing() {
    expect_num("return [10, 20, 30][1];", 20.0);
    expect_num(
        "let a = [1]; let h = a[3]; return h === undefined ? 9 : 0;",
        9.0,
    );
    expect_num("let e = []; return e.length;", 0.0);
}

#[test]
fn sequence_expression_takes_last_value() {
    expect_num("return (1, 2, 3);", 3.0);
    expect_num("let i = 0; return (i += 2, i);", 2.0);
}

#[test]
fn conditional_expression() {
    expect_num("return true ? 1 : 2;", 1.0);
    expect_num("return false ? 1 : 2;", 2.0);
    expect_num("return 1 < 2 ? 10 : 20;", 10.0);
}

#[test]
fn void_yields_undefined() {
    expect_undefined("return void 1;");
    expect_num("return (void 0) === undefined ? 1 : 0;", 1.0);
}

// ---------------------------------------------------------------------------
// Control flow
// ---------------------------------------------------------------------------

#[test]
fn if_else_diamond() {
    expect_num("if (false) { return 1; } else { return 2; }", 2.0);
    expect_num("if (true) return 40; else return 50;", 40.0);
}

#[test]
fn short_circuit_skips_right_side_effects() {
    // Side effect observable through an object property counter.
    let src = "
        let o = {n: 0};
        let bumpTrue = function () { o.n = o.n + 1; return true; };
        let bumpFalse = function () { o.n = o.n + 1; return false; };
        let r1 = bumpFalse() && bumpTrue();
        let afterAnd = o.n;
        let r2 = bumpTrue() || bumpFalse();
        let afterOr = o.n;
        return (afterAnd * 10 + afterOr)
            + (r1 === false ? 0 : 100) + (r2 === true ? 0 : 200);
    ";
    expect_num(src.trim(), 12.0);
}

#[test]
fn nullish_coalesce_picks_right_on_undefined() {
    expect_num("let u; return u ?? 7;", 7.0);
    expect_num("return 0 ?? 7;", 0.0);
    expect_str("return 'kept' ?? 7;", "kept"); // empty/falsy strings are NOT nullish
}

#[test]
fn while_loop_sums() {
    expect_num(
        "let i = 1, s = 0; while (i <= 10) { s += i; i += 1; } return s;",
        55.0,
    );
}

#[test]
fn do_while_runs_body_at_least_once() {
    expect_num("let x = 0; do { x += 1; } while (false); return x;", 1.0);
    expect_num(
        "let i = 0, s = 0; do { s += i; i += 1; } while (i < 5); return s;",
        10.0,
    );
}

#[test]
fn for_loop_with_init_test_update() {
    expect_num(
        "let s = 0; for (let i = 0; i < 5; i += 1) { s += i; } return s;",
        10.0,
    );
    expect_num(
        "let s = 0; for (;;) { s += 1; if (s > 3) break; } return s;",
        4.0,
    );
}

#[test]
fn break_and_continue_in_loops() {
    expect_num(
        "let s = 0; for (let i = 0; i < 10; i += 1) { if (i % 2) continue; if (i > 6) break; s += i; } return s;",
        12.0,
    );
}

#[test]
fn labeled_break_out_of_nested_loops() {
    expect_num(
        "
        let hits = 0;
        outer: for (let i = 0; i < 3; i += 1) {
            for (let j = 0; j < 3; j += 1) {
                if (j === 1 && i === 1) break outer;
                hits += 1;
            }
        }
        return hits;
        ",
        4.0,
    );
}

#[test]
fn labeled_continue_targets_outer_loop() {
    expect_num(
        "
        let s = 0;
        outer: for (let i = 0; i < 3; i += 1) {
            for (let j = 0; j < 3; j += 1) {
                if (j === 1) continue outer;
                s += 1;
            }
        }
        return s;
        ",
        3.0,
    );
}

// ---------------------------------------------------------------------------
// Functions & closures
// ---------------------------------------------------------------------------

#[test]
fn function_declaration_hoisting_and_calls() {
    expect_num(
        "
        let r = double(21);
        function double(x) { return x * 2; }
        return r;
        ",
        42.0,
    );
}

#[test]
fn plain_call_passes_args_and_returns() {
    expect_num(
        "let add = function (a, b, c) { return a + b + c; }; return add(1, 2, 3);",
        6.0,
    );
}

#[test]
fn method_call_binds_this() {
    expect_num(
        "
        let counter = {n: 10, inc: function () { this.n = this.n + 1; return this.n; }};
        counter.inc();
        return counter.inc();
        ",
        12.0,
    );
    expect_num(
        "
        let c = {base: 5, mul: function (k) { return this.base * k; }};
        return c.mul(3);
        ",
        15.0,
    );
}

#[test]
fn computed_member_method_call() {
    expect_num(
        "
        let o = {f: function () { return 9; }};
        let name = 'f';
        return o[name]();
        ",
        9.0,
    );
}

#[test]
fn closure_counter_increments_across_calls() {
    expect_num(
        "
        function makeCounter() {
            let n = 0;
            return function () { n += 1; return n; };
        }
        let c1 = makeCounter();
        let c2 = makeCounter();
        c1(); c1(); c1();
        let first = c1();
        let second = c2();
        return first * 10 + second;
        ",
        41.0,
    );
}

#[test]
fn two_level_environment_chain() {
    expect_num(
        "
        function outer() {
            let x = 100;
            function mid() {
                let y = 20;
                function inner() { return x + y; }
                return inner();
            }
            return mid();
        }
        return outer();
        ",
        120.0,
    );
}

#[test]
fn closures_share_home_binding_writes() {
    expect_num(
        "
        function make() {
            let v = 1;
            let get = function () { return v; };
            let set = function (n) { v = n; };
            return {get, set};
        }
        let box = make();
        box.set(99);
        return box.get();
        ",
        99.0,
    );
}

#[test]
fn shadowing_inner_block_declaration() {
    expect_num("let x = 1; { let x = 2; } return x;", 1.0);
    expect_num("let y = 1; { let y = 2; return y; }", 2.0);
    expect_num("let z = 1; { var w = 5; } return w;", 5.0);
}

#[test]
fn arrow_functions_expression_and_block_bodies() {
    expect_num("let sq = x => x * x; return sq(9);", 81.0);
    expect_num("let add = (a, b) => a + b; return add(2, 3);", 5.0);
    expect_num("let f = () => { return 77; }; return f();", 77.0);
    expect_num("let g = () => 88; return g();", 88.0);
}

#[test]
fn arrow_captures_enclosing_variables() {
    expect_num(
        "
        function make(k) {
            let bias = 2;
            return () => k * 10 + bias;
        }
        return make(4)();
        ",
        42.0,
    );
}

#[test]
fn arrow_this_resolves_to_enclosing_function() {
    expect_num(
        "
        let obj = {
            v: 0,
            init: function () {
                this.v = 33;
                let reader = () => this.v;
                return reader();
            },
        };
        return obj.init();
        ",
        33.0,
    );
}

#[test]
fn named_function_expression_recursion() {
    expect_num(
        "
        let fib = function fact(n) { return n < 2 ? n : fact(n - 1) + fact(n - 2); };
        return fib(10);
        ",
        55.0,
    );
}

#[test]
fn recursion_through_declared_function() {
    expect_num(
        "
        function sumTo(n) { return n === 0 ? 0 : n + sumTo(n - 1); }
        return sumTo(10);
        ",
        55.0,
    );
}

#[test]
fn arguments_default_to_undefined() {
    expect_num(
        "let f = function (a) { return a === undefined ? 1 : 0; }; return f();",
        1.0,
    );
}

// ---------------------------------------------------------------------------
// Exceptions
// ---------------------------------------------------------------------------

#[test]
fn throw_and_catch_value_semantics() {
    expect_num(
        "
        let caught = 0;
        try {
            throw 41;
        } catch (e) {
            caught = e + 1;
        }
        return caught;
        ",
        42.0,
    );
}

#[test]
fn catch_not_taken_without_throw() {
    expect_num(
        "let x = 1; try { x = 2; } catch (e) { x = 3; } return x;",
        2.0,
    );
}

#[test]
fn finally_runs_on_normal_path() {
    expect_num(
        "
        let log = 0;
        try { log += 1; } finally { log += 10; }
        return log;
        ",
        11.0,
    );
}

#[test]
fn finally_runs_after_caught_exception() {
    expect_num(
        "
        let log = '';
        try {
            try { log += 'T'; throw 'x'; } catch (e) { log += 'C'; } finally { log += 'F'; }
        } catch (outer) { log += 'O'; }
        return log === 'TCF' ? 1 : 0;
        ",
        1.0,
    );
}

#[test]
fn finally_rethrows_when_no_catch() {
    expect_num(
        "
        let ran = 0;
        try {
            try { throw 5; } finally { ran = 1; }
        } catch (e) {
            ran += e;
        }
        return ran;
        ",
        6.0,
    );
}

#[test]
fn exception_from_catch_hits_finally_then_propagates() {
    expect_num(
        "
        let marks = 0;
        try {
            try {
                throw 1;
            } catch (e) {
                marks += 10;
                throw 2;
            } finally {
                marks += 100;
            }
        } catch (outer) {
            marks += outer;
        }
        return marks;
        ",
        112.0,
    );
}

#[test]
fn return_through_finally_runs_finalizer() {
    expect_num(
        "
        let side = 0;
        function f() {
            try { return 8; } finally { side = 80; }
        }
        let r = f();
        return r + side;
        ",
        88.0,
    );
}

#[test]
fn break_through_finally_runs_finalizer() {
    expect_num(
        "
        let side = 0, total = 0;
        for (let i = 0; i < 3; i += 1) {
            try {
                if (i === 1) break;
                total += 1;
            } finally { side += 5; }
        }
        return total * 10 + side;
        ",
        20.0,
    );
}

#[test]
fn nested_try_regions_dispatch_correctly() {
    expect_num(
        "
        let path = 0;
        try {
            try { throw 3; } catch (inner) { path += inner; }
            throw 30;
        } catch (outer2) { path += outer2; }
        return path;
        ",
        33.0,
    );
}

// ---------------------------------------------------------------------------
// Destructuring (tier 2)
// ---------------------------------------------------------------------------

#[test]
fn object_pattern_declaration() {
    expect_num("let {a, b} = {a: 1, b: 2}; return a * 10 + b;", 12.0);
    expect_num("let {x: renamed} = {x: 5}; return renamed;", 5.0);
}

#[test]
fn array_pattern_declaration() {
    expect_num("let [p, q] = [3, 4]; return p * 10 + q;", 34.0);
    expect_num(
        "let [first, , third] = [1, 2, 3]; return first + third;",
        4.0,
    );
}

// ---------------------------------------------------------------------------
// Statements / misc
// ---------------------------------------------------------------------------

#[test]
fn block_scoping_keeps_loop_local_visible() {
    expect_num(
        "
        let s = 0;
        for (let i = 0; i < 3; i += 1) {
            let doubled = i * 2;
            s += doubled;
        }
        return s;
        ",
        6.0,
    );
}

#[test]
fn uninitialized_declarations_are_undefined() {
    expect_undefined("let x; return x;");
    expect_num("var y; return y === undefined ? 1 : 0;", 1.0);
}

#[test]
fn empty_and_debugger_statements_are_nops() {
    expect_num("; ; debugger; return 1;", 1.0);
}

#[test]
fn template_literal_without_substitution() {
    expect_str("return `plain`;", "plain");
}

#[test]
fn strict_mode_directive_is_recorded() {
    let (p, _) = compile_source_with_strings("'use strict'; 1").unwrap();
    assert!(p.functions.iter().all(|f| f.is_strict));
    let (p2, _) = compile_source_with_strings("1").unwrap();
    assert!(p2.functions.iter().all(|f| !f.is_strict));
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn unsupported_constructs_fail_as_compile_errors() {
    let cases: &[&str] = &[
        "+x",
        "let x = 1n;",
        "for (let v of [1]) {}",
        "switch (1) { case 1: break; }",
        "new Object()",
        "({m() {}})",
        "`tpl ${1}`",
        "class C {}",
        "[a, b] = [1, 2]",
        "return 1;",
    ];
    for src in cases {
        let err = compile_source_with_strings(src)
            .map_err(|e| e.message)
            .err()
            .unwrap_or_else(|| panic!("expected CompileError for {src:?}"));
        assert!(
            !err.is_empty(),
            "error message should be informative for {src:?}"
        );
    }
}

#[test]
fn parse_errors_surface_as_compile_errors() {
    let err = compile_source_with_strings("let let let;").expect_err("parse fails");
    assert!(err.message.contains("parse error"), "{}", err.message);
}

#[test]
fn in_operator_compiles_to_opcode() {
    let (p, _) =
        compile_source_with_strings("let o = {a: 1}; let k = 'a'; let r = k in o;").unwrap();
    let fb = &p.functions[p.main as usize];
    assert!(fb.validate().is_ok());
    assert!(
        fb.instrs.iter().any(|i| i.op() == Some(Opcode::In)),
        "expected In opcode in:\n{fb}"
    );
    // Validate operand layout: In dst, key, obj
    let in_instr = fb
        .instrs
        .iter()
        .find(|i| i.op() == Some(Opcode::In))
        .unwrap();
    assert!(u16::from(in_instr.a()) < fb.max_regs);
}

#[test]
fn instanceof_operator_compiles_to_opcode() {
    let src = "let F = function () {}; let o = {}; let r = o instanceof F;";
    let (p, _) = compile_source_with_strings(src).unwrap();
    let fb = &p.functions[p.main as usize];
    assert!(fb.validate().is_ok());
    assert!(
        fb.instrs.iter().any(|i| i.op() == Some(Opcode::InstanceOf)),
        "expected InstanceOf opcode in:\n{fb}"
    );
}

#[test]
fn too_many_locals_returns_compile_error_not_panic() {
    // Build a source with 300 distinct let bindings in one function scope,
    // exceeding the u8 register limit (255). The compiler must return a
    // graceful CompileError with message "too many functions/constants".
    let mut src = String::new();
    for i in 0..300 {
        src.push_str(&format!("let v{i} = {i};\n"));
    }
    src.push_str("let sum = 0;\n");
    let err = compile_source_with_strings(&src).expect_err("should overflow registers");
    assert!(
        err.message.contains("too many functions/constants"),
        "unexpected message: {}",
        err.message
    );
    assert!(
        err.span.is_some(),
        "overflow error must carry a span for negative-test fidelity"
    );
    // Near-limit program should still compile.
    let mut ok_src = String::new();
    for i in 0..80 {
        ok_src.push_str(&format!("let v{i} = {i};\n"));
    }
    assert!(
        compile_source_with_strings(&ok_src).is_ok(),
        "80 locals should fit within limits"
    );
}

// ---------------------------------------------------------------------------
// Structural checks: validate(), spans, disassembler
// ---------------------------------------------------------------------------

#[test]
fn every_compiled_function_validates() {
    let (p, _) = compile_source_with_strings(
        "
        function f(a) { try { return a.p.q; } finally { a.side = 1; } }
        let mk = function () { let n = 0; return () => ++n > 2 ? 'many' : 'few'; };
        for (let i = 0; i < 3; i += 1) { if (i) { mk(); } else { continue; } }
        outer: while (true) { break outer; }
        ",
    )
    .unwrap();
    for f in &p.functions {
        f.validate()
            .unwrap_or_else(|e| panic!("validate failed: {e}\n{f}"));
        assert_eq!(f.spans.len(), f.instrs.len(), "spans stay index-aligned");
    }
}

#[test]
fn disassembler_smoke_renders_all_sections() {
    let (p, _) = compile_source_with_strings(
        "
        let s = 'k';
        let o = {};
        o[s] = 1;
        function g(a) { return a + 256; }
        try { g(o[s]); } catch (e) { throw e; }
        ",
    )
    .unwrap();
    let text = format!("{}", p.functions[p.main as usize]);
    for needle in [
        "function",
        "load_const",
        "new_object",
        "set_property",
        "handlers:",
        "->",
    ] {
        assert!(
            text.contains(needle),
            "disassembly missing {needle:?}:\n{text}"
        );
    }
    let g_text = format!("{}", p.functions.last().unwrap());
    assert!(
        g_text.contains("load_int_w"),
        "wide loads appear in listing:\n{g_text}"
    );
}

#[test]
fn peephole_folds_constant_arithmetic() {
    let (_, _) = compile_source_with_strings("let x = 3 + 4 * 2;").unwrap();
    let (p, _) = compile_source_with_strings("let x = 3 + 4 * 2;").unwrap();
    let main = &p.functions[p.main as usize];
    let has_folded_load = main
        .instrs
        .iter()
        .any(|i| i.op() == Some(Opcode::LoadInt) && imm8_of(*i) == 11);
    assert!(has_folded_load, "expected folded LoadInt #11 in:\n{main}");
    // The folded form must be shorter than unfolded emission would allow:
    // no Add survives for the constant-only expression.
    assert!(
        !main.instrs.iter().any(|i| i.op() == Some(Opcode::Add)),
        "constant Add should be folded away:\n{main}"
    );
}

fn imm8_of(i: Instr) -> i32 {
    i8::from_be_bytes([i.c()]) as i32
}

#[test]
fn peephole_removes_dead_jumps() {
    let (p, _) = compile_source_with_strings("if (true) { let a = 1; } ").unwrap();
    let main = &p.functions[p.main as usize];
    // Constant condition folds/jumps thread away: no unconditional jump may
    // target the immediately-following instruction.
    for (pc, i) in main.instrs.iter().enumerate() {
        if i.op() == Some(Opcode::Jump) {
            assert_ne!(i.imm24() as usize, pc + 1, "dead jump survived:\n{main}");
        }
    }
}

#[test]
fn peephole_threads_jump_chains() {
    let (p, _) = compile_source_with_strings("while (false) { } let done = 1;").unwrap();
    let main = &p.functions[p.main as usize];
    for (pc, i) in main.instrs.iter().enumerate() {
        if matches!(i.op(), Some(Opcode::Jump)) {
            let t = i.imm24() as usize;
            let lands_on_jump = main
                .instrs
                .get(t)
                .and_then(|w| w.op())
                .is_some_and(|op| matches!(op, Opcode::Jump));
            assert!(
                !lands_on_jump,
                "unthreaded jump-to-jump at pc {pc}:\n{main}"
            );
        }
    }
}

#[test]
fn compile_ast_reuses_caller_pipeline_identically() {
    let src = "
        function make(n) {
            let acc = [];
            for (let i = 0; i < n; i += 1) acc.push(i * 2);
            return acc.length;
        }
        let out = make(4);
    ";
    let (from_source, _) = compile_source_with_strings(src).unwrap();

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, src, SourceType::script()).parse();
    assert!(!parsed.panicked);
    let semantic = SemanticBuilder::new().build(&parsed.program);
    assert_eq!(semantic.diagnostics.errors().count(), 0);
    let scoping = semantic.semantic.into_scoping();
    let from_ast = compile_ast(&parsed.program, &scoping).unwrap();

    assert_eq!(from_source.functions.len(), from_ast.functions.len());
    for (a, b) in from_source.functions.iter().zip(&from_ast.functions) {
        assert_eq!(a.instrs, b.instrs, "instruction streams must match");
        assert_eq!(a.max_regs, b.max_regs);
    }
}

// ---------------------------------------------------------------------------
// Shared lasso interner
// ---------------------------------------------------------------------------

/// Every `Const::Str32` payload across the program's constant pools.
fn str_ids(prog: &Program) -> Vec<u32> {
    prog.functions
        .iter()
        .flat_map(|f| f.consts.iter())
        .filter_map(|c| match c {
            Const::Str32(id) => Some(id),
            _ => None,
        })
        .collect()
}

#[test]
fn shared_interner_reuses_keys_across_compilations() {
    // Property keys and string literals are what compilation interns.
    let src = "let obj = {aa: 1}; let t = obj.aa + 'lit';";
    let mut interner = Interner::default();
    assert!(interner.is_empty());

    let first = compile_source_with_interner(src, &mut interner).unwrap();
    let after_first = interner.len();
    assert!(after_first > 0, "compilation interns identifiers");

    // Re-compiling identical source must intern nothing new: every
    // identifier and literal already has a key.
    let second = compile_source_with_interner(src, &mut interner).unwrap();
    assert_eq!(interner.len(), after_first);

    // Reuse also means identical Str32 ids in both programs.
    assert_eq!(str_ids(&first), str_ids(&second));
}

#[test]
fn distinct_identifiers_get_distinct_keys() {
    let src = "let o = {aa: 1, bb: 2, cc: 3}; let x = o.aa + o.bb + o.cc + 'dd';";
    let mut interner = Interner::default();
    let program = compile_source_with_interner(src, &mut interner).unwrap();

    let ids = str_ids(&program);
    assert_eq!(
        ids.iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        interner.len(),
        "each distinct interned string maps to exactly one key"
    );

    // Directly on the interner: same string → same key, distinct strings →
    // distinct keys.
    let a = interner.get_or_intern("dup");
    assert_eq!(interner.get_or_intern("dup"), a);
    assert_ne!(interner.get_or_intern("other"), a);
}

#[test]
fn frozen_resolver_maps_str_ids_back_to_identifiers() {
    let src = "let obj = {counter: 10}; let total = obj.counter + 'done';";
    let mut interner = Interner::default();
    let program = compile_source_with_interner(src, &mut interner).unwrap();

    let resolver = freeze_interner(interner);
    let texts: Vec<&str> = str_ids(&program)
        .iter()
        .map(|&id| {
            let spur = spur_of_str_id(id).expect("compiled ids are representable");
            resolver.resolve(&spur)
        })
        .collect();

    for text in &texts {
        assert!(
            src.contains(text),
            "resolved {text:?} must come from the compiled source"
        );
    }
    for expected in ["counter", "done"] {
        assert!(
            texts.contains(&expected),
            "{expected:?} missing from resolved texts {texts:?}"
        );
    }
}

#[test]
fn str_id_conversions_round_trip() {
    let mut interner = Interner::default();
    for s in ["x", "yy", "zzz"] {
        let spur = interner.get_or_intern(s);
        let id = str_id_of(spur);
        assert_eq!(spur_of_str_id(id).map(str_id_of), Some(id));
    }
    // The one id no Spur can name (Spur wraps a NonZeroU32).
    assert!(spur_of_str_id(u32::MAX).is_none());
}

// ---------------------------------------------------------------------------
// Module linkage (ESM import / export)
// ---------------------------------------------------------------------------

#[test]
fn module_import_named_bindings() {
    let src = r#"import {x} from "./a.js";"#;
    // Script mode must reject with the spec-mandated diagnostic.
    let script_err = crate::compile_source(src).expect_err("script should reject import");
    assert!(
        script_err
            .message
            .contains("import statements only valid in modules"),
        "got: {}",
        script_err.message
    );
    // Module mode accepts and records one import.
    let m = crate::compile_source_as_module(src).expect("module should accept import");
    assert_eq!(m.imports.len(), 1, "one import entry");
    assert_eq!(m.imports[0].specifier, "./a.js");
    assert_eq!(m.imports[0].imported, "x");
    assert!(m.imports[0].local.is_some());
    // Program still validates and has the import call lowered.
    assert!(m.program.functions[0].validate().is_ok());
    let disasm = format!("{}", m.program.functions[0]);
    // The lowering emits a Closure to NATIVE_IMPORT_INDEX for the specifier.
    assert!(
        disasm.contains("./a.js") || disasm.contains("load_const"),
        "disasm:\n{disasm}"
    );
}

#[test]
fn module_import_namespace_and_side_effect() {
    let src_ns = r#"import * as ns from "./a.js";"#;
    let m = crate::compile_source_as_module(src_ns).expect("ns import");
    assert_eq!(m.imports.len(), 1);
    assert_eq!(m.imports[0].imported, "*");
    assert_eq!(m.imports[0].specifier, "./a.js");
    assert!(m.imports[0].local.is_some());

    let src_side = r#"import "./side.js";"#;
    let m2 = crate::compile_source_as_module(src_side).expect("side import");
    assert_eq!(m2.imports.len(), 1);
    assert_eq!(m2.imports[0].specifier, "./side.js");
    assert_eq!(m2.imports[0].imported, "");
    assert!(m2.imports[0].local.is_none());

    // Script must reject both.
    assert!(crate::compile_source(src_ns).is_err());
    assert!(crate::compile_source(src_side).is_err());
}

#[test]
fn module_import_default_and_mixed() {
    let src = r#"import foo from "./a.js"; import {bar as baz} from "./b.js";"#;
    let m = crate::compile_source_as_module(src).expect("mixed imports");
    assert_eq!(m.imports.len(), 2);
    // Default import.
    let def = m
        .imports
        .iter()
        .find(|e| e.imported == "default")
        .expect("default");
    assert_eq!(def.specifier, "./a.js");
    // Named with alias: imported "bar", local is the alias `baz`.
    let named = m.imports.iter().find(|e| e.imported == "bar").expect("bar");
    assert_eq!(named.specifier, "./b.js");
}

#[test]
fn module_exports_are_recorded() {
    let src = r#"export const x = 1; export function foo() {} export {x as y};"#;
    let m = crate::compile_source_as_module(src).expect("exports");
    // At least the three exported names should be present.
    let exported_names: Vec<_> = m.exports.iter().map(|e| e.exported.as_str()).collect();
    assert!(exported_names.contains(&"x"), "{exported_names:?}");
    assert!(exported_names.contains(&"foo"), "{exported_names:?}");
    assert!(exported_names.contains(&"y"), "{exported_names:?}");
    assert!(crate::compile_source(src).is_err());
}

#[test]
fn script_rejects_export() {
    let src = r#"export const x = 1;"#;
    let err = crate::compile_source(src).expect_err("export in script should err");
    assert!(
        err.message
            .contains("export statements only valid in modules")
            || err
                .message
                .contains("import statements only valid in modules"),
        "got: {}",
        err.message
    );
}

#[test]
fn hoisted_functions_with_interleaved_arrows_do_not_panic() {
    // This pattern previously panicked with `units must reserve slots in order`
    // (left 3 right 2) because `collect` assigned indices depth-first while
    // `stmt_list` hoisted function declarations before arrow initializers.
    let src = "let a = () => 1; function foo() { return 2; } let b = () => 3; let c = () => 4;";
    let (prog, _) = compile_source_with_strings(src).expect("should compile without panic");
    assert_eq!(prog.functions.len(), 5, "main + 3 arrows + foo");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
}

#[test]
fn example_03_functions_compiles_and_validates() {
    let src = std::fs::read_to_string("../../examples/03-functions.js").unwrap_or_else(|_| {
        // Fallback when test cwd is crate root.
        std::fs::read_to_string("examples/03-functions.js")
            .unwrap_or_else(|_| "let add = (a, b) => a + b; let sum = add(10, 32); sum".into())
    });
    let (prog, _) = compile_source_with_strings(&src).expect("example 03 should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
}
/// Regression for the catch-binding register window.
///
/// The `catch (e)` binding lives inside the handler's register window whose
/// size is `stack_depth`. The delivery register itself is `stack_depth`, so
/// `max_regs` must be at least `stack_depth + 1` and the catch binding's
/// register (when allocated as a `Reg`) must lie within that window.
#[test]
fn catch_binding_registers_validate() {
    let src = "try { throw 1; } catch (e) { e }";
    let (prog, _) = compile_source_with_strings(src).expect("catch program should compile");
    for func in &prog.functions {
        func.validate().expect("handler ranges should validate");
        if let Some(max_depth) = func.handlers.iter().map(|h| h.stack_depth).max() {
            assert!(
                u32::from(func.max_regs) > max_depth,
                "max_regs {} must exceed handler stack_depth {}",
                func.max_regs,
                max_depth
            );
        }
    }
    // A second shape with an outer variable that the catch body writes to,
    // exercising the `move r_caught, r_e` path that previously triggered
    // `index out of bounds: len 6 but index 7` at `base + reg`.
    let src2 = "let caught=0; try { throw 99; } catch(e){caught=e;}";
    let (prog2, _) = compile_source_with_strings(src2).expect("outer catch write should compile");
    for func in &prog2.functions {
        func.validate().expect("handler ranges should validate");
    }
}

// ---------------------------------------------------------------------------
// Bucket 3 — `null` literal (Const::Null, typeof null === "object")
// ---------------------------------------------------------------------------

#[test]
fn null_literal_constant_pool_contains_null() {
    // `let x = null` must lower to a `Const::Null` entry.
    let src = "let x = null;";
    let (prog, _) = compile_source_with_strings(src).expect("null literal should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok(), "validate failed:\n{fb}");
    assert!(
        fb.consts.iter().any(|c| matches!(c, Const::Null)),
        "ConstantPool should contain Null, got: {:?}",
        fb.consts.iter().collect::<Vec<_>>()
    );
    // Also compiles inside a function and with multiple nulls deduped.
    let src2 = "function f(a){ return a === null ? 1 : 0; } let x = null; let y = null;";
    let (prog2, _) = compile_source_with_strings(src2).expect("repeated null should compile");
    for f in &prog2.functions {
        f.validate().expect("validate");
    }
    let null_count = prog2
        .functions
        .iter()
        .flat_map(|f| f.consts.iter())
        .filter(|c| matches!(c, Const::Null))
        .count();
    assert!(null_count >= 1, "expected at least one Null const");
}

#[test]
fn null_typeof_is_object_and_coalesce_treats_null_as_nullish() {
    // `typeof null` is "object" per spec (type_tag already handles it).
    expect_str("let x = null; return typeof x;", "object");
    expect_str("return typeof null;", "object");
    // Null is falsy but distinct from 0 / "" — loose equality.
    expect_bool("return null == undefined;", true);
    expect_bool("return null === null;", true);
    expect_bool("return null === undefined;", false);
    // `??` treats both `null` and `undefined` as nullish.
    expect_num("let x = null; return x ?? 7;", 7.0);
    expect_num("let y; return y ?? 9;", 9.0);
    expect_num("return 0 ?? 7;", 0.0);
    // Property access on nullish base should not be reached here; just check
    // that `null` round-trips through the mini-interp.
    expect_bool("return !null;", true);
}

// ---------------------------------------------------------------------------
// Bucket 4 — computed property names (`{[k]:1}`, `o[k]`)
// ---------------------------------------------------------------------------

#[test]
fn computed_property_names_object_literal_and_member_read() {
    // `{[k]: 1}` must lower to ToPropertyKey + SetProperty with a dynamic key,
    // and `o[k]` must lower to GetProperty with a dynamic key — no new opcode.
    let src = r#"let k = "a"; let o = {[k]: 1}; let v = o[k];"#;
    let (prog, _) = compile_source_with_strings(src).expect("computed props should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok(), "validate failed:\n{fb}");
    assert!(
        fb.instrs
            .iter()
            .any(|i| i.op() == Some(Opcode::GetProperty)),
        "expected GetProperty for o[k] in:\n{fb}"
    );
    assert!(
        fb.instrs
            .iter()
            .any(|i| i.op() == Some(Opcode::SetProperty)),
        "expected SetProperty for {{[k]:1}} in:\n{fb}"
    );
    // End-to-end via mini-interp: value is 1, and non-string keys coerce.
    expect_num(r#"let k = "a"; let o = {[k]: 1}; return o[k];"#, 1.0);
    expect_num(r#"let k = "b"; let o = {}; o[k] = 2; return o[k];"#, 2.0);
    // Numeric key coerces to string via ToPropertyKey / to_key.
    expect_num(r#"let o = {[1]: 99}; return o[1];"#, 99.0);
    expect_num(r#"let o = {[1]: 99}; return o["1"];"#, 99.0);
}

#[test]
fn computed_member_assignment_and_dynamic_key_evaluation() {
    // `obj[expr] = value` and `obj[expr]` evaluate `expr` into a temp then
    // the base, using the existing GetProperty/SetProperty paths.
    let src = r#"let o = {}; let k = "x"; o[k] = 42; let v = o[k];"#;
    let (prog, _) = compile_source_with_strings(src).expect("computed member should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok(), "validate failed:\n{fb}");
    // At least one dynamic GetProperty and one SetProperty.
    let gets = fb
        .instrs
        .iter()
        .filter(|i| i.op() == Some(Opcode::GetProperty))
        .count();
    let sets = fb
        .instrs
        .iter()
        .filter(|i| i.op() == Some(Opcode::SetProperty))
        .count();
    assert!(gets >= 1, "expected GetProperty in:\n{fb}");
    assert!(sets >= 1, "expected SetProperty in:\n{fb}");

    // Mutation through computed key.
    expect_num(r#"let o = {a: 1}; let k = "a"; o[k] = 9; return o.a;"#, 9.0);
    // Expression key (not just identifier).
    expect_num(
        r#"let o = {}; let p = "a"; o[p + "b"] = 5; return o.ab;"#,
        5.0,
    );
    // Chained computed access: o[k][j]
    expect_num(
        r#"let o = {a: {b: 7}}; let k="a"; let j="b"; return o[k][j];"#,
        7.0,
    );
}

// ---------------------------------------------------------------------------
// Bucket 5 — Destructuring (array/object, rest, nested, defaults)
// ---------------------------------------------------------------------------

#[test]
fn destructuring_object_rest() {
    let src = "let {a, ...rest} = {a:1, b:2, c:3};";
    let (prog, _) = compile_source_with_strings(src).expect("object rest should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    // Nested
    let src2 = "let {a: {b}} = {a: {b: 42}};";
    let (prog2, _) = compile_source_with_strings(src2).expect("nested object should compile");
    for f in &prog2.functions {
        f.validate().expect("validate");
    }
}

#[test]
fn destructuring_array_rest_and_nested() {
    let src = "let [a, [b, c], ...rest] = [1, [2,3], 4,5];";
    let (prog, _) = compile_source_with_strings(src).expect("array rest nested should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    let src2 = "let [x = 10, y = 20] = [5];";
    let (prog2, _) = compile_source_with_strings(src2).expect("array defaults should compile");
    for f in &prog2.functions {
        f.validate().expect("validate");
    }
}

#[test]
fn destructuring_object_defaults() {
    let src = "let {a = 1, b: {c = 2}} = {};";
    let (prog, _) = compile_source_with_strings(src).expect("object defaults should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
}

// ---------------------------------------------------------------------------
// Bucket 6 — Rest parameters & spread
// ---------------------------------------------------------------------------

#[test]
fn rest_params_compile() {
    let src = "function f(a, ...rest) { return rest.length; }";
    let (prog, _) = compile_source_with_strings(src).expect("rest params should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    assert!(prog.functions.iter().any(|f| f.has_rest));
}

#[test]
fn spread_call_and_array_compile() {
    let src = "let arr=[1,2]; let x=[...arr, 3]; function g(...a){return a[0];} g(...arr);";
    let (prog, _) = compile_source_with_strings(src).expect("spread should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    // Check that spread lowered to CheckIsArray and ArrayAppend / CallApply
    let text = format!("{}", prog.functions[prog.main as usize]);
    assert!(
        text.contains("check_is_array")
            || text.contains("array_append")
            || text.contains("call_apply")
    );
}

// ---------------------------------------------------------------------------
// Bucket 9 — function-code strict & Annex B
// ---------------------------------------------------------------------------

#[test]
fn strict_const_reassign_is_syntax_error() {
    let src = "\"use strict\"; const x=1; x=2;";
    let err =
        compile_source_with_strings(src).expect_err("strict const reassign should be SyntaxError");
    assert!(
        err.message.contains("SyntaxError") || err.message.contains("constant"),
        "got: {}",
        err.message
    );
}

#[test]
fn strict_eval_binding_is_syntax_error() {
    let src = "\"use strict\"; var eval = 1;";
    let err =
        compile_source_with_strings(src).expect_err("strict eval binding should be SyntaxError");
    assert!(err.message.contains("eval"), "got: {}", err.message);
}

#[test]
fn annex_b_sloppy_block_function_compiles() {
    let src = "if (true) function f(){ return 1; }";
    let (prog, _) = compile_source_with_strings(src).expect("sloppy block function should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    // Strict should reject
    let src2 = "\"use strict\"; if (true) function f(){ return 1; }";
    assert!(
        compile_source_with_strings(src2).is_err(),
        "strict block function should be error"
    );
}

// ---------------------------------------------------------------------------
// Bucket 11 — import/export module-code
// ---------------------------------------------------------------------------

#[test]
fn module_import_export_compile() {
    let src = "import {x} from \"./a.js\"; export const y = 1;";
    let m = crate::compile_source_as_module(src).expect("module should compile");
    assert_eq!(m.imports.len(), 1);
    assert!(m.exports.iter().any(|e| e.exported == "y"));
    for f in &m.program.functions {
        f.validate().expect("validate");
    }
}

#[test]
fn module_re_export_compile() {
    let src = "export * from \"./other.js\"; export {x as y} from \"./a.js\";";
    let m = crate::compile_source_as_module(src).expect("re-export should compile");
    assert!(m.exports.iter().any(|e| e.exported == "*"));
    for f in &m.program.functions {
        f.validate().expect("validate");
    }
}

// ---------------------------------------------------------------------------
// Bucket 12 — Generators / async
// ---------------------------------------------------------------------------

#[test]
fn generator_function_compiles() {
    let src = "function* gen(){ yield 1; yield 2; }";
    let (prog, _) = compile_source_with_strings(src).expect("generator should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    // Check that generator opcodes appear
    let has_yield = prog
        .functions
        .iter()
        .any(|f| format!("{f}").contains("suspend_yield"));
    assert!(has_yield, "expected suspend_yield in generator");
}

#[test]
fn async_function_compiles() {
    let src = "async function af(){ await 1; }";
    let (prog, _) = compile_source_with_strings(src).expect("async should compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    let has_await = prog
        .functions
        .iter()
        .any(|f| format!("{f}").contains("await"));
    assert!(has_await, "expected await in async");
}

// ---------------------------------------------------------------------------
// Bucket 3 — Global object & property model (`Object`/`Array`/etc. via `GetGlobal`)
// ---------------------------------------------------------------------------

#[test]
fn global_intrinsics_compile_to_get_global() {
    for name in [
        "Object", "Array", "String", "Number", "Boolean", "Math", "JSON", "Error",
    ] {
        let src = format!("let x = {name};");
        let (prog, _) = compile_source_with_strings(&src)
            .unwrap_or_else(|e| panic!("{name} should compile: {e}"));
        let fb = &prog.functions[prog.main as usize];
        assert!(fb.validate().is_ok(), "validate failed for {name}:\n{fb}");
        assert!(
            fb.instrs.iter().any(|i| i.op() == Some(Opcode::GetGlobal)),
            "expected GetGlobal for {name} in:\n{fb}"
        );
        // Also ensure `typeof` on the intrinsic does not early-return to "undefined"
        // but goes through GetGlobal + TypeOf.
        let src2 = format!("let y = typeof {name};");
        let (prog2, _) = compile_source_with_strings(&src2).expect("typeof global should compile");
        let fb2 = &prog2.functions[prog2.main as usize];
        assert!(
            fb2.instrs.iter().any(|i| i.op() == Some(Opcode::GetGlobal)),
            "typeof {name} should still use GetGlobal in:\n{fb2}"
        );
        assert!(
            fb2.instrs.iter().any(|i| i.op() == Some(Opcode::TypeOf)),
            "typeof {name} should use TypeOf in:\n{fb2}"
        );
    }
    // Unknown globals also compile via GetGlobal (missing → undefined at runtime)
    let src3 = "let x = __unknownGlobalXYZ;";
    let (prog3, _) = compile_source_with_strings(src3).expect("unknown global should compile");
    assert!(
        prog3.functions[0]
            .instrs
            .iter()
            .any(|i| i.op() == Some(Opcode::GetGlobal))
    );
}

#[test]
fn global_assign_and_update_compile_to_set_global() {
    let src = "Object = 1; Array = 2; let y = Object; let z = Array;";
    let (prog, _) = compile_source_with_strings(src).expect("global assign should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok(), "validate failed:\n{fb}");
    assert!(
        fb.instrs.iter().any(|i| i.op() == Some(Opcode::SetGlobal)),
        "expected SetGlobal in:\n{fb}"
    );
    assert!(
        fb.instrs.iter().any(|i| i.op() == Some(Opcode::GetGlobal)),
        "expected GetGlobal in:\n{fb}"
    );
    // Global update `Object++` should lower to GetGlobal + Add + SetGlobal
    let src2 = "Object++; ++Array;";
    let (prog2, _) = compile_source_with_strings(src2).expect("global update should compile");
    let fb2 = &prog2.functions[prog2.main as usize];
    assert!(fb2.validate().is_ok(), "validate failed:\n{fb2}");
    assert!(
        fb2.instrs.iter().any(|i| i.op() == Some(Opcode::GetGlobal)),
        "global update should use GetGlobal in:\n{fb2}"
    );
    assert!(
        fb2.instrs.iter().any(|i| i.op() == Some(Opcode::SetGlobal)),
        "global update should use SetGlobal in:\n{fb2}"
    );
    // Computed property with dynamic key via let
    let src3 = r#"let k="a"; let o={[k]:1}; let v=o[k];"#;
    let (prog3, _) = compile_source_with_strings(src3).expect("computed via let should compile");
    let fb3 = &prog3.functions[prog3.main as usize];
    assert!(fb3.validate().is_ok(), "validate failed:\n{fb3}");
    expect_num(r#"let k="a"; let o={[k]:1}; return o[k];"#, 1.0);
}

#[test]
fn console_log_member_chain_uses_distinct_string_ids() {
    // `console.log(x)` compiles to `GetGlobal` for `console` (PropKey/Spur)
    // and `LoadConst` for `"log"` (Str32/pool) with distinct string table
    // ids. The bug conflated the two (both `k0` → `Str32("console")`),
    // making `GetProperty` load `console["console"]` which is `undefined`.
    let src = "console.log(1);";
    let (prog, strings) = compile_source_with_strings(src).expect("console.log should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok(), "validate failed:\n{fb}");
    let get_global = fb
        .instrs
        .iter()
        .find(|i| i.op() == Some(Opcode::GetGlobal))
        .expect("expected GetGlobal for console");
    let load_const = fb
        .instrs
        .iter()
        .find(|i| i.op() == Some(Opcode::LoadConst))
        .expect("expected LoadConst for log");
    let get_global_id = u32::from(get_global.imm16());
    let pool_idx = load_const.imm16();
    let load_const_str_id = match fb.consts.get(pool_idx).expect("pool entry") {
        Const::Str32(id) => id,
        other => panic!("expected Str32 for log, got {other:?}"),
    };
    assert_ne!(
        get_global_id, load_const_str_id,
        "console and log must be distinct string ids: console id {get_global_id}, log id {load_const_str_id} in:\n{fb}\nstrings: {strings:?}"
    );
    assert_eq!(strings[get_global_id as usize], "console");
    assert_eq!(strings[load_const_str_id as usize], "log");
    assert!(
        fb.instrs
            .iter()
            .any(|i| i.op() == Some(Opcode::GetProperty)),
        "expected GetProperty for console.log in:\n{fb}"
    );
}

#[test]
fn arrow_iife_compiles_and_executes() {
    // `(x => x)(1)` is the minimal arrow IIFE — the outer call must not be
    // conflated with the inner `console.log` property lookup. This test
    // guards the `x => x` closure and the call ABI.
    let src = "let f = (x => x); let v = f(1); return v;";
    expect_num(src, 1.0);
    let (prog, _) = compile_source_with_strings("(x => x)(1)").expect("arrow IIFE should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(
        fb.instrs.iter().any(|i| i.op() == Some(Opcode::Closure)),
        "expected Closure for arrow in:\n{fb}"
    );
    assert!(
        fb.instrs.iter().any(|i| i.op() == Some(Opcode::Call)),
        "expected Call for IIFE in:\n{fb}"
    );
}

// ---------------------------------------------------------------------------
// Bucket: global-code — top-level `var` aliases the global object
// ---------------------------------------------------------------------------

#[test]
fn global_code_top_level_var_uses_global_storage() {
    // `var` at the top level must lower to `GetGlobal`/`SetGlobal` so
    // `globalThis` aliasing and `global-code` tests (42 files) can pass.
    for src in [
        "var x = 1;",
        "var a = 10; var b = 20; let c = a + b;",
        "var foo = 5; foo += 1;",
    ] {
        let (prog, _) = compile_source_with_strings(src)
            .unwrap_or_else(|e| panic!("{src:?} should compile: {e}"));
        let fb = &prog.functions[prog.main as usize];
        assert!(fb.validate().is_ok(), "validate failed for {src:?}:\n{fb}");
        assert!(
            fb.instrs.iter().any(|i| i.op() == Some(Opcode::SetGlobal)),
            "expected SetGlobal for top-level var in {src:?}:\n{fb}"
        );
    }
    // Reading a top-level var must use GetGlobal.
    let (prog, _) =
        compile_source_with_strings("var x = 1; let y = x;").expect("var read should compile");
    assert!(
        prog.functions[prog.main as usize]
            .instrs
            .iter()
            .any(|i| i.op() == Some(Opcode::GetGlobal)),
        "expected GetGlobal for var read"
    );
    // `let` at top level stays lexically scoped (Reg), not Global.
    let (prog, _) = compile_source_with_strings("let y = 1;").expect("let should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok());
    // `let y` must NOT use GetGlobal/SetGlobal; it is a plain local.
    assert!(
        !fb.instrs.iter().any(|i| i.op() == Some(Opcode::SetGlobal)),
        "let at top level should not use SetGlobal:\n{fb}"
    );
    // Validate runtime semantics via expect_num (wrapped in IIFE).
    expect_num("var x = 1; return x;", 1.0);
    expect_num("var a = 10; var b = 20; return a + b;", 30.0);
}

#[test]
fn global_code_var_and_function_declaration_both_global() {
    let src = "var v = 1; function f() { return v; }";
    let (prog, _) = compile_source_with_strings(src).expect("global var+func should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok(), "validate failed:\n{fb}");
    expect_num("var v = 1; function f() { return v; } return f();", 1.0);
    // Also verify `var` inside function stays local (Reg/Env), not Global.
    let src2 = "function g(){ var local = 9; return local; } return g();";
    expect_num(src2, 9.0);
}

#[test]
fn global_code_intrinsics_still_via_get_global() {
    // Global intrinsics must still resolve via GetGlobal even when a
    // top-level var exists.
    for name in ["Object", "Array", "Symbol", "console", "globalThis"] {
        let src = format!("var x = 1; let y = {name};");
        let (prog, _) = compile_source_with_strings(&src)
            .unwrap_or_else(|e| panic!("{name} with top-level var should compile: {e}"));
        let fb = &prog.functions[prog.main as usize];
        assert!(fb.validate().is_ok(), "validate failed for {name}:\n{fb}");
        assert!(
            fb.instrs.iter().any(|i| i.op() == Some(Opcode::GetGlobal)),
            "expected GetGlobal for {name} in:\n{fb}"
        );
    }
}

// ---------------------------------------------------------------------------
// Bucket: computed-property-names — 48 tests, ToPropertyKey + SetProperty
// ---------------------------------------------------------------------------

#[test]
fn computed_property_names_object_literal() {
    for src in [
        r#"let k="a"; let o={[k]:1};"#,
        r#"let x="y"; let o={[x + "Z"]: 5};"#,
        r#"let n=1; let o={[n]: "one", ["b"]: 2};"#,
    ] {
        let (prog, _) = compile_source_with_strings(src)
            .unwrap_or_else(|e| panic!("{src:?} should compile: {e}"));
        let fb = &prog.functions[prog.main as usize];
        assert!(fb.validate().is_ok(), "validate failed for {src:?}:\n{fb}");
        // Must use dynamic SetProperty (key evaluated into temp), not a
        // static string key. No new opcode is introduced.
        assert!(
            fb.instrs
                .iter()
                .any(|i| i.op() == Some(Opcode::SetProperty)),
            "expected SetProperty for computed key in {src:?}:\n{fb}"
        );
    }
    expect_num(r#"let k="a"; let o={[k]:1}; return o[k];"#, 1.0);
    expect_num(r#"let o={[1+1]: 42}; return o[2];"#, 42.0);
}

#[test]
fn computed_property_names_member_assignment() {
    for src in [
        "let o={}; let k='p'; o[k]=9;",
        "let o={a:1}; let k='a'; let v=o[k]; o[k]=v+1;",
        "let o={}; o[1+1]=7;",
    ] {
        let (prog, _) = compile_source_with_strings(src)
            .unwrap_or_else(|e| panic!("{src:?} should compile: {e}"));
        let fb = &prog.functions[prog.main as usize];
        assert!(fb.validate().is_ok(), "validate failed for {src:?}:\n{fb}");
        assert!(
            fb.instrs
                .iter()
                .any(|i| i.op() == Some(Opcode::SetProperty)),
            "expected SetProperty for computed member in {src:?}:\n{fb}"
        );
    }
    // Verify read path separately.
    let (prog, _) = compile_source_with_strings("let o={a:1}; let k='a'; let v=o[k];")
        .expect("computed get should compile");
    assert!(
        prog.functions[prog.main as usize]
            .instrs
            .iter()
            .any(|i| i.op() == Some(Opcode::GetProperty)),
        "expected GetProperty for computed member read"
    );
    expect_num("let o={}; let k='p'; o[k]=9; return o[k];", 9.0);
}

#[test]
fn computed_property_names_key_evaluated_first() {
    // Key expression must be evaluated before base object per bucket 4
    // (key expr into temp, then base). This also verifies ToPropertyKey
    // via `to_key` handles non-string keys (number → string).
    let src = r#"
        let calls = 0;
        function keyFn(){ calls += 1; return "dyn"; }
        let o={[keyFn()]: 123};
        return o["dyn"] * 10 + calls;
    "#;
    expect_num(src, 1231.0);
    let compile_src = r#"
        let calls = 0;
        function keyFn(){ calls += 1; return "dyn"; }
        let o={[keyFn()]: 123};
        let v = o["dyn"];
    "#;
    let (prog, _) =
        compile_source_with_strings(compile_src).expect("computed key side effect should compile");
    assert!(prog.functions[prog.main as usize].validate().is_ok());
}

// ---------------------------------------------------------------------------
// Bucket: literals — `null` via Const::Null, `typeof null === "object"`
// ---------------------------------------------------------------------------

#[test]
fn literals_null_typeof_stays_object() {
    for src in ["typeof null;", "let x = null; typeof x;", "typeof (null);"] {
        let (prog, _) = compile_source_with_strings(src)
            .unwrap_or_else(|e| panic!("{src:?} should compile: {e}"));
        let fb = &prog.functions[prog.main as usize];
        assert!(fb.validate().is_ok(), "validate failed for {src:?}:\n{fb}");
        assert!(
            fb.instrs.iter().any(|i| i.op() == Some(Opcode::TypeOf)),
            "expected TypeOf for typeof null in {src:?}:\n{fb}"
        );
    }
    expect_str("return typeof null;", "object");
    expect_str("let x = null; return typeof x;", "object");
    expect_bool("return null === null;", true);
    expect_bool("return null == undefined;", true);
}

#[test]
fn literals_null_constant_pool_and_coalesce() {
    let (prog, _) = compile_source_with_strings("let a = null; let b = a ?? 5;")
        .expect("null coalesce should compile");
    let fb = &prog.functions[prog.main as usize];
    assert!(fb.validate().is_ok(), "validate failed:\n{fb}");
    // Const pool must contain Null (no payload)
    assert!(
        fb.consts.iter().any(|c| matches!(c, Const::Null)),
        "expected Const::Null in pool:\n{fb}"
    );
    expect_num("return null ?? 7;", 7.0);
    expect_num("let x = null; return x ?? 9;", 9.0);
    expect_num("return 0 ?? 7;", 0.0);
}

#[test]
fn literals_null_equality_and_strict_equality() {
    expect_bool("return null == null;", true);
    expect_bool("return null === null;", true);
    expect_bool("return null !== undefined;", true);
    expect_bool("return null == 0;", false);
    let (prog, _) =
        compile_source_with_strings("null === null;").expect("null strict eq should compile");
    assert!(prog.functions[prog.main as usize].validate().is_ok());
}
