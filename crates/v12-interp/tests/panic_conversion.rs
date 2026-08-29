use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

#[test]
fn await_outside_async_throws_not_panic() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, "await 1").unwrap();
    let err = interp.run().unwrap_err();
    assert!(interp.to_display_string(err.0).contains("await outside async"));
}
