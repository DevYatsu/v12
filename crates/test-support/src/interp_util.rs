//! Helpers for tests that drive the real interpreter.

use v12_heap::{GcPolicy, Heap, JsValue};
use v12_interp::{Interp, JSException};

/// Runs `interp`, expecting an uncaught throw; returns the thrown value.
pub fn expect_throw(interp: &mut Interp<'_>) -> JsValue {
    match interp.run() {
        Err(JSException(v)) => v,
        Ok(()) => panic!("expected an uncaught exception"),
    }
}

/// Compiles + runs `src`, returning the thrown value (completion-value trick).
pub fn eval_thrown(src: &str) -> JsValue {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).expect("compile");
    expect_throw(&mut interp)
}
