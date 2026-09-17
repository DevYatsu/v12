use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

/// `AsyncGenerator.prototype.throw` resumed from inside a nested call (the
/// `call_object`/host boundary sets `stop_at_frames`) must inject the throw
/// at the suspended `yield` and let the body complete abruptly — not pop the
/// generator frame and then drain past it to the stale outer boundary.
///
/// Regression: `resume_generator_nested` called `unwind` before arming
/// `stop_at_frames`, so the stale outer boundary (from the enclosing nested
/// `execute`) let `unwind` pop the generator frame *and* the caller frame.
/// `execute` then drove an unrelated frame whose register window had been
/// truncated away, panicking with `index out of bounds` in the `Opcode::Call`
/// arm at `crates/v12-interp/src/execute.rs:588`. The panic aborted the
/// interpreter; with the fix the throw is contained and the script runs on to
/// its `'end'` completion value.
#[test]
fn async_generator_throw_from_nested_call_does_not_corrupt_frames() {
    let src = "async function* ag(){ yield 1; } \
               var it = ag(); \
               it.next(); \
               (function(){ it.throw(1); })(); \
               'end';";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    interp.run().expect("script runs to completion");
    interp.run_jobs();
    let v = interp.completion_value().expect("completion value");
    assert_eq!(interp.to_display_string(v), "end");
}

/// Control: the same nested-call shape on a *sync* generator must still
/// deliver the injected throw to the suspended `yield` and run its `catch`,
/// leaving the non-throw resume path's behavior unchanged by the fix.
#[test]
fn sync_generator_throw_from_nested_call_runs_catch() {
    let src = "var observed = 'none'; \
               function* g(){ try { yield 1; } catch(e) { observed = 'caught'; } } \
               var it = g(); \
               it.next(); \
               (function(){ it.throw(1); })(); \
               observed;";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    interp.run().expect("script runs to completion");
    let v = interp.completion_value().expect("completion value");
    assert_eq!(interp.to_display_string(v), "caught");
}
