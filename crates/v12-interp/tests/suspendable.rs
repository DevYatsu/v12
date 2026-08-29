use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

#[test]
fn suspend_resume_round_trip() {
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(
        &mut heap,
        "function* g(){ let x = yield 1; return x; } let it=g(); it.next(); let r=it.next(41); throw r.value;",
    )
    .unwrap();
    let thrown = interp.run().unwrap_err();
    assert_eq!(interp.to_display_string(thrown.0), "41");
}
