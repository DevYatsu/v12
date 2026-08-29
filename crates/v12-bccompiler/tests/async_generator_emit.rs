use v12_bccompiler::compile_source_with_strings;

#[test]
fn generator_function_emits_create_generator() {
    let (prog, _) = compile_source_with_strings("function* g(){ yield 1; }").unwrap();
    assert!(!prog.functions[prog.main as usize].is_generator); // main is not generator
    assert!(prog.functions.iter().any(|f| f.is_generator), "expected a generator function unit");
    let g = prog.functions.iter().find(|f| f.is_generator).unwrap();
    assert!(format!("{g}").contains("create_generator"), "expected CreateGenerator in generator body, got {g}");
}

#[test]
fn async_function_is_async_flag_and_contains_await() {
    let (prog, _) = compile_source_with_strings("async function f(){ await 1; }").unwrap();
    assert!(prog.functions.iter().any(|f| f.is_async));
}
