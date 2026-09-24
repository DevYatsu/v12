use v12_bccompiler::{
    EvalBindingKind, EvalCallerFacts, EvalCompileFacts, Interner,
    compile_eval_source_with_interner_facts,
};

fn compile(source: &str, bindings: &[(&str, EvalBindingKind)]) -> Result<(), String> {
    let mut interner = Interner::default();
    let facts = EvalCompileFacts {
        direct: true,
        strict: false,
        caller: EvalCallerFacts {
            strict: false,
            arguments_object_needed: false,
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
fn arrow_default_parameter_without_arguments_is_legal() {
    assert!(compile("var arguments; 1", &[]).is_ok());
}
