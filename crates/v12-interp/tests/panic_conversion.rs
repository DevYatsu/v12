use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

#[test]
fn await_outside_async_throws_not_panic() {
    // `await` outside an async function is an early error: oxc reports it
    // as a parse diagnostic, which the driver surfaces instead of
    // compiling a truncation (previously this compiled clean and the
    // interpreter's `Await` arm threw "await outside async" at runtime).
    // Either way the input throws rather than panicking.
    let mut heap = Heap::new(GcPolicy::NoGC);
    let err = match Interp::from_source(&mut heap, "await 1") {
        Ok(_) => panic!("top-level await in a script must fail compilation"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("await"), "unexpected error message: {err}");
}
