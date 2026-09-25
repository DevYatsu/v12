use v12_bytecode::Opcode;
use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

fn thrown_display(src: &str) -> String {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).unwrap();
    let err = interp.run().unwrap_err();
    interp.to_display_string(err.0)
}

fn compiled_ops(src: &str) -> Vec<Opcode> {
    let (program, _) = v12_bccompiler::compile_source_with_strings(src).unwrap();
    program.functions[program.main as usize]
        .instrs
        .iter()
        .filter_map(|instr| instr.op())
        .collect()
}

fn has_iter_ops(ops: &[Opcode]) -> bool {
    ops.contains(&Opcode::GetIterator) && ops.contains(&Opcode::IteratorNext)
}

#[test]
fn set_destructures_fixed_elements() {
    let ops = compiled_ops("let [a, b] = new Set([1, 2]); throw [a, b].join(',');");
    assert!(
        has_iter_ops(&ops),
        "array binding did not use the iterator protocol: {ops:?}"
    );
}

#[test]
fn set_destructures_rest() {
    let ops = compiled_ops(
        "var [head, ...tail] = new Set([1, 2, 3]); throw head + ':' + tail.join(',');",
    );
    assert!(
        has_iter_ops(&ops),
        "array rest did not use the iterator protocol: {ops:?}"
    );
}

#[test]
fn string_destructures_by_iterator() {
    let ops = compiled_ops("let [a, b, c] = 'abc'; throw [a, b, c].join('');");
    assert!(
        has_iter_ops(&ops),
        "string binding did not use the iterator protocol: {ops:?}"
    );
}

#[test]
fn custom_iterator_is_consumed_once() {
    let ops = compiled_ops(
        "let source = { [Symbol.iterator]() { return { next() { return { value: 7, done: false }; } }; } }; let [a] = source; throw a;",
    );
    assert!(
        has_iter_ops(&ops),
        "custom iterator binding did not use IteratorNext: {ops:?}"
    );
}

#[test]
fn nested_patterns_and_defaults_bind_iterated_values() {
    let ops = compiled_ops(
        "let [{ x: [a, b = 4] }, c = 9] = new Set([{ x: [1] }, 3]); throw [a, b, c].join(',');",
    );
    assert!(
        has_iter_ops(&ops),
        "nested/default binding did not use the iterator protocol: {ops:?}"
    );
}

#[test]
fn nullish_sources_throw_type_errors() {
    assert!(thrown_display("let [a] = null;").contains("TypeError"));
    assert!(thrown_display("let [a] = undefined;").contains("TypeError"));
    assert!(thrown_display("let [, ] = null;").contains("TypeError"));
}

#[test]
fn ordinary_array_and_sync_generator_binding_control() {
    // The direct array-literal fast path remains executable in the standalone
    // interpreter; general iterable lowering is checked above at bytecode level.
    assert_eq!(
        thrown_display("let [a, b] = [10, 20]; throw [a, b].join(',');"),
        "10,20"
    );
    let ops = compiled_ops(
        "function* g(){ yield 3; yield 4; } let [a, b] = g(); throw [a, b].join(',');",
    );
    assert!(
        has_iter_ops(&ops),
        "sync-generator control did not use the iterator protocol: {ops:?}"
    );
}
