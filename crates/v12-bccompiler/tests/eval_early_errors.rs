use v12_bccompiler::{
    EvalBindingKind, EvalCallerFacts, EvalCompileFacts, Interner,
    compile_eval_source_with_interner_facts, compile_source,
};

fn compile(source: &str, bindings: &[(&str, EvalBindingKind)]) -> Result<(), String> {
    compile_with_direct(source, bindings, true)
}

fn compile_with_direct(
    source: &str,
    bindings: &[(&str, EvalBindingKind)],
    direct: bool,
) -> Result<(), String> {
    let mut interner = Interner::default();
    let facts = EvalCompileFacts {
        direct,
        strict: false,
        caller: EvalCallerFacts {
            strict: false,
            arguments_object_needed: bindings
                .iter()
                .any(|(_, kind)| *kind == EvalBindingKind::ImplicitArguments),
            bindings: bindings
                .iter()
                .map(|(name, kind)| ((*name).to_string(), *kind))
                .collect(),
        },
    };
    compile_eval_source_with_interner_facts(source, &mut interner, &facts)
        .map(|_| ())
        .map_err(|error| error.message)
}

#[test]
fn rejects_var_arguments_for_parameter_caller() {
    assert!(
        compile(
            "marker = 1; var arguments = 'param'",
            &[("arguments", EvalBindingKind::Parameter)]
        )
        .is_err()
    );
}

#[test]
fn rejects_var_arguments_for_lexical_caller() {
    assert!(
        compile(
            "var arguments = 'lexical'",
            &[("arguments", EvalBindingKind::Lexical)]
        )
        .is_err()
    );
}

#[test]
fn rejects_var_arguments_for_implicit_arguments_caller() {
    assert!(
        compile(
            "var arguments = 'implicit'",
            &[("arguments", EvalBindingKind::ImplicitArguments)]
        )
        .is_err()
    );
}

#[test]
fn nested_function_var_does_not_join_eval_var_names() {
    assert!(
        compile(
            "function nested() { var arguments; } 1",
            &[("arguments", EvalBindingKind::ImplicitArguments)]
        )
        .is_ok()
    );
}

#[test]
fn sloppy_var_eval_is_not_rejected_for_non_simple_caller() {
    assert!(compile("var eval = 1", &[]).is_ok());
}

#[test]
fn strict_var_eval_remains_rejected() {
    let mut interner = Interner::default();
    let facts = EvalCompileFacts {
        direct: true,
        strict: true,
        ..EvalCompileFacts::default()
    };
    assert!(
        compile_eval_source_with_interner_facts("var eval = 1", &mut interner, &facts).is_err()
    );
}

#[test]
fn arrow_parameter_site_excludes_body_arguments_binding() {
    let program =
        compile_source("const f = (p = eval(\"var arguments\")) => { var arguments; return 1; };")
            .unwrap();
    let sites: Vec<_> = program
        .functions
        .iter()
        .flat_map(|f| f.eval_metadata.iter().flat_map(|m| &m.direct_eval_sites))
        .collect();
    assert!(
        !sites.is_empty(),
        "no eval sites: {:?}",
        program
            .functions
            .iter()
            .map(|f| &f.name_hint)
            .collect::<Vec<_>>()
    );
    let site = sites
        .iter()
        .find(|site| matches!(site.phase, v12_bytecode::EvalPhase::Parameter))
        .unwrap_or_else(|| panic!("no parameter site: {:?}", sites));
    assert!(
        !site
            .caller
            .bindings
            .iter()
            .any(|(name, _)| name == "arguments"),
        "{:?}",
        site
    );
}

#[test]
fn nested_arrow_inherits_non_simple_owner_arguments_fact() {
    let program =
        compile_source("function outer(a = 1) { return (() => eval(\"var arguments; 1\"))(); }")
            .unwrap();
    let site = program
        .functions
        .iter()
        .flat_map(|f| f.eval_metadata.iter().flat_map(|m| &m.direct_eval_sites))
        .next()
        .unwrap();
    assert!(site.caller.arguments_object_needed);
    assert!(
        site.caller
            .bindings
            .iter()
            .any(|(name, kind)| name == "arguments" && *kind == EvalBindingKind::ImplicitArguments)
    );
}

#[test]
fn explicit_arrow_arguments_binding_wins_over_owner_implicit_fact() {
    let program = compile_source(
        "function outer(a = 1) { return (arguments => eval(\"var arguments; 1\"))(1); }",
    )
    .unwrap();
    let site = program
        .functions
        .iter()
        .flat_map(|f| f.eval_metadata.iter().flat_map(|m| &m.direct_eval_sites))
        .next()
        .unwrap();
    assert!(
        site.caller
            .bindings
            .iter()
            .any(|(name, kind)| name == "arguments" && *kind == EvalBindingKind::Parameter)
    );
    assert!(
        !site
            .caller
            .bindings
            .iter()
            .any(|(name, kind)| name == "arguments" && *kind == EvalBindingKind::ImplicitArguments)
    );
}

#[test]
fn arrow_body_site_sees_body_arguments_binding() {
    let program =
        compile_source("const f = () => { let arguments; return eval(\"var arguments\"); };")
            .unwrap();
    let site = program
        .functions
        .iter()
        .flat_map(|f| f.eval_metadata.iter().flat_map(|m| &m.direct_eval_sites))
        .find(|site| matches!(site.phase, v12_bytecode::EvalPhase::Body))
        .unwrap();
    assert!(
        site.caller
            .bindings
            .iter()
            .any(|(name, kind)| name == "arguments" && *kind == EvalBindingKind::Lexical)
    );
}

#[test]
fn eval_outside_nested_arguments_block_is_legal() {
    let program =
        compile_source("function f() { { let arguments; } return eval(\"var arguments\"); }")
            .unwrap();
    let site = program
        .functions
        .iter()
        .flat_map(|f| f.eval_metadata.iter().flat_map(|m| &m.direct_eval_sites))
        .next()
        .unwrap();
    assert!(
        !site
            .caller
            .bindings
            .iter()
            .any(|(name, _)| name == "arguments")
    );
}

#[test]
fn ordinary_non_simple_caller_reports_implicit_arguments() {
    let program = compile_source("function f(a = 1) { return eval(\"var arguments\"); }").unwrap();
    let site = program
        .functions
        .iter()
        .flat_map(|f| f.eval_metadata.iter().flat_map(|m| &m.direct_eval_sites))
        .next()
        .unwrap();
    assert!(site.caller.arguments_object_needed);
    assert!(
        site.caller
            .bindings
            .iter()
            .any(|(name, kind)| name == "arguments" && *kind == EvalBindingKind::ImplicitArguments)
    );
}

#[test]
fn simple_ordinary_caller_has_no_implicit_arguments_fact() {
    let program = compile_source("function f() { return eval(\"var arguments\"); }").unwrap();
    let site = program
        .functions
        .iter()
        .flat_map(|f| f.eval_metadata.iter().flat_map(|m| &m.direct_eval_sites))
        .next()
        .unwrap();
    assert!(!site.caller.arguments_object_needed);
    assert!(
        !site
            .caller
            .bindings
            .iter()
            .any(|(name, _)| name == "arguments")
    );
}

#[test]
fn indirect_facts_do_not_reject_arguments_var() {
    assert!(
        compile_with_direct(
            "var arguments; 1",
            &[("arguments", EvalBindingKind::Var)],
            false
        )
        .is_ok()
    );
}

#[test]
fn eval_var_declared_names_include_loops_and_exclude_nested_functions() {
    let mut interner = Interner::default();
    let facts = EvalCompileFacts {
        direct: true,
        ..EvalCompileFacts::default()
    };
    let program = compile_eval_source_with_interner_facts(
        "for (var x in {}) {} for (var y of []) {} function direct() {} function nested() { var z; } 1",
        &mut interner,
        &facts,
    )
    .unwrap();
    let names = &program.functions[program.main as usize]
        .eval_metadata
        .as_ref()
        .unwrap()
        .var_declared_names;
    assert!(names.iter().any(|name| name == "x"));
    assert!(names.iter().any(|name| name == "y"));
    assert!(names.iter().any(|name| name == "direct"));
    assert!(!names.iter().any(|name| name == "z"));
}
