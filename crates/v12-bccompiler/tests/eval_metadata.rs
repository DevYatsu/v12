use v12_bccompiler::{
    EvalCompileFacts, Interner, compile_eval_source_with_interner_facts, compile_source,
};

#[test]
fn direct_eval_call_site_is_marked_without_new_opcode() {
    let program = compile_source("function f(x) { return eval('x'); }").unwrap();
    let f = program
        .functions
        .iter()
        .find(|f| {
            f.name_hint
                .as_deref()
                .is_some_and(|name| name.ends_with(":f") || name == "f")
        })
        .unwrap();
    assert!(f.eval_metadata.as_ref().unwrap().eval_reachable);
    assert!(
        f.eval_metadata
            .as_ref()
            .unwrap()
            .direct_eval_sites
            .iter()
            .any(|site| {
                f.instrs
                    .get(site.pc as usize)
                    .is_some_and(|i| i.op() == Some(v12_bytecode::Opcode::Call))
            })
    );
}

#[test]
fn direct_eval_site_pc_points_at_call_instruction() {
    let params = (0..260)
        .map(|index| format!("p{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let program = compile_source(&format!("function f({params}) {{ return eval('1'); }}")).unwrap();
    let f = program
        .functions
        .iter()
        .find(|f| {
            f.name_hint
                .as_deref()
                .is_some_and(|name| name.ends_with(":f"))
        })
        .unwrap();
    let site = f
        .eval_metadata
        .as_ref()
        .unwrap()
        .direct_eval_sites
        .first()
        .unwrap();
    assert_eq!(
        f.instrs[site.pc as usize].op(),
        Some(v12_bytecode::Opcode::Call)
    );
}

#[test]
fn eval_reachable_bindings_are_promoted_to_environment() {
    let program =
        compile_source("function f() { let x = 1; return eval('(function(){ return x; })'); }")
            .unwrap();
    let f = program
        .functions
        .iter()
        .find(|f| {
            f.name_hint
                .as_deref()
                .is_some_and(|name| name.ends_with(":f") || name == "f")
        })
        .unwrap();
    let x = f
        .eval_metadata
        .as_ref()
        .unwrap()
        .bindings
        .iter()
        .find(|(name, _)| name == "x")
        .unwrap();
    assert!(matches!(x.1, v12_bytecode::BindingLocation::Env(_)));
}

#[test]
fn eval_source_directive_wins_over_false_caller_strictness() {
    let mut interner = Interner::default();
    let program = compile_eval_source_with_interner_facts(
        "'use strict'; missing = 1",
        &mut interner,
        &EvalCompileFacts {
            direct: true,
            strict: false,
            ..EvalCompileFacts::default()
        },
    )
    .unwrap();
    let main = &program.functions[program.main as usize];
    assert!(main.is_strict);
    assert!(main.eval_metadata.as_ref().unwrap().is_eval_payload);
    let nested = compile_eval_source_with_interner_facts(
        "function nested() {} 1",
        &mut interner,
        &EvalCompileFacts {
            direct: true,
            ..EvalCompileFacts::default()
        },
    )
    .unwrap();
    assert!(
        !nested.functions[1]
            .eval_metadata
            .as_ref()
            .unwrap()
            .is_eval_payload
    );
}

#[test]
fn alias_eval_is_not_marked_as_direct() {
    let program = compile_source("function f() { const e = eval; return e('1'); }").unwrap();
    let f = program
        .functions
        .iter()
        .find(|f| {
            f.name_hint
                .as_deref()
                .is_some_and(|name| name.ends_with(":f") || name == "f")
        })
        .unwrap();
    assert!(f.eval_metadata.is_none());
}
