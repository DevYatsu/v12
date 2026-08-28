use v12_bccompiler::compile_source_with_strings;

#[test]
fn yield_star_delegates() {
    let (prog, _) = compile_source_with_strings("function* g(){ yield* [1,2]; }").unwrap();
    // generator function is not main (index 0), check any function contains call/get_property
    let found = prog.functions.iter().any(|f| format!("{f}").contains("call"));
    assert!(found, "expected yield* to emit call/next, got: {}", prog.functions.iter().map(|f| format!("{f}")).collect::<Vec<_>>().join("\n---\n"));
}
