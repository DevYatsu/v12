use v12_bccompiler::compile_source_with_strings;

#[test]
fn yield_star_compiles_and_contains_suspend_yield() {
    let (prog, _) = compile_source_with_strings("function* g(){ yield* [1,2]; }").unwrap();
    let g = prog
        .functions
        .iter()
        .find(|f| f.is_generator)
        .expect("expected generator function");
    let dump = format!("{g}");
    assert!(
        dump.contains("suspend_yield"),
        "expected SuspendYield in yield* lowering, got {dump}"
    );
}
