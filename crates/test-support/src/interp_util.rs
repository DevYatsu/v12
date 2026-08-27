//! Helpers for tests that drive the real interpreter.

use v12_heap::JsValue;
use v12_interp::{Interp, JSException};

/// Runs `interp`, expecting an uncaught throw; returns the thrown value.
pub fn expect_throw(interp: &mut Interp) -> JsValue {
    match interp.run() {
        Err(JSException(v)) => v,
        Ok(()) => panic!("expected an uncaught exception"),
    }
}

/// Compiles + runs `src`, returning the thrown value (completion-value trick).
pub fn eval_thrown(src: &str) -> JsValue {
    let mut interp = Interp::from_source(src).expect("compile");
    expect_throw(&mut interp)
}
