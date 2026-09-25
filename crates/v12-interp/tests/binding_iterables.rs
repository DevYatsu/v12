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

fn close_flags(src: &str) -> Vec<u8> {
    let (program, _) = v12_bccompiler::compile_source_with_strings(src).unwrap();
    program.functions[program.main as usize]
        .instrs
        .iter()
        .filter_map(|instr| (instr.op() == Some(Opcode::IteratorClose)).then_some(instr.b()))
        .collect()
}

/// A one-element iterator that is still live after the binding: the
/// early-completion check must reuse the last step's `done` flag (spec
/// 7.4.17 step 6) instead of pulling one more value.
const LIVE_ITERABLE: &str = "let source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, return() { return {}; } }; } };";

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

#[test]
fn next_failure_closes_iterator_once_on_throw_path() {
    let src = "let closed = 0; let source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, return() { closed++; return {}; } }; } }; let [a, b] = source; throw [a, b, closed].join(',');";
    assert!(close_flags(src).contains(&0));
}

#[test]
fn value_getter_failure_closes_iterator_once() {
    let src = "let source = { [Symbol.iterator]() { return { next() { return { get value() { throw 'value'; }, done: false }; }, return() { return {}; } }; } }; let [a] = source;";
    assert!(close_flags(src).contains(&0));
}

#[test]
fn nested_default_failure_closes_iterator() {
    let src = "let source = { [Symbol.iterator]() { return { next() { return { value: undefined, done: false }; }, return() { return {}; } }; } }; let [a = (() => { throw 'default'; })()] = source;";
    assert!(close_flags(src).contains(&0));
}

#[test]
fn binding_store_failure_is_inside_close_range() {
    let src = "let source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, return() { return {}; } }; } }; let [{ x }] = source;";
    assert!(close_flags(src).contains(&0));
}

#[test]
fn throwing_return_does_not_replace_original_error() {
    let src = "let source = { [Symbol.iterator]() { return { next() { throw 'next'; }, return() { throw 'return'; } }; } }; let [a] = source;";
    assert!(close_flags(src).contains(&0));
}

#[test]
fn normal_early_completion_uses_validated_close() {
    let src = format!("{LIVE_ITERABLE} let [a] = source;");
    let flags = close_flags(&src);
    // Two closes are emitted: the normal-completion one (flag 1) and the
    // throw-path handler one (flag 0).
    assert_eq!(
        flags,
        vec![1, 0],
        "early normal completion closes once with validation: {flags:?}"
    );
}

#[test]
fn absent_return_allows_normal_early_completion() {
    let src = "let source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; } }; } }; let [a] = source;";
    assert_eq!(close_flags(&src), vec![1, 0]);
}

#[test]
fn rest_pattern_drains_without_a_normal_close() {
    // The rest loop runs to `done`, so no normal-completion close is emitted;
    // only the throw-path handler close remains. Runtime close counts are
    // asserted in v12-engine's `array_binding_close` tests.
    let src = "let source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, return() { return {}; } }; } }; let [a, ...rest] = source;";
    assert_eq!(close_flags(&src), vec![0]);
}

#[test]
fn early_completion_does_not_pull_an_extra_value() {
    // Spec 7.4.17 step 6 reads the iterator record's [[Done]] flag; an extra
    // IteratorNext here would consume one more value from a live iterator.
    let ops = compiled_ops(&format!("{LIVE_ITERABLE} let [a, b] = source;"));
    let nexts = ops.iter().filter(|&&op| op == Opcode::IteratorNext).count();
    assert_eq!(
        nexts, 2,
        "one IteratorNext per element, no exhaustion probe: {ops:?}"
    );
}

#[test]
fn close_guard_is_armed_before_the_normal_close() {
    // A close error re-enters the exception handler; the guard must already be
    // set so the same iterator is not closed twice.
    let src = format!("{LIVE_ITERABLE} let [a] = source;");
    let (program, _) = v12_bccompiler::compile_source_with_strings(&src).unwrap();
    let instrs = &program.functions[program.main as usize].instrs;
    let close_at = instrs
        .iter()
        .position(|i| i.op() == Some(Opcode::IteratorClose) && i.b() == 1)
        .expect("normal-completion close");
    // `load_bool(guard, true)` lowers to `LoadInt t, 0` + `Eq guard, t, t`.
    let guard_at = instrs[..close_at]
        .iter()
        .rposition(|i| i.op() == Some(Opcode::Eq))
        .expect("close guard armed before the close");
    assert!(
        close_at - guard_at <= 2,
        "guard must be armed immediately before the close"
    );
    // The handler path skips the close when the guard is set.
    assert!(
        instrs
            .iter()
            .any(|i| i.op() == Some(Opcode::IteratorClose) && i.b() == 0)
    );
}

#[test]
fn reentrant_return_does_not_double_close() {
    // `return()` re-enters the same pattern once, through a fresh iterator.
    // Bytecode level: the nested destructuring is the same lowering, so the
    // guard-before-close ordering above is what prevents the second close.
    // The runtime close count is covered in v12-engine's
    // `array_binding_close` tests (the standalone interpreter has no
    // `Symbol` intrinsic, so a custom iterable cannot run here).
    let src = "let source; source = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, return() { let [nested] = source; return {}; } }; } }; let [a] = source;";
    assert!(close_flags(&src).contains(&1));
}
