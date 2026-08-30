use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

#[test]
fn yield_suspends_and_next_resumes() {
    let src = "function* g(){ let x = yield 1; return x + 1; } let it = g(); let a = it.next(); let b = it.next(41); throw b.value;";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).unwrap();
    let res = interp.run();
    assert!(res.is_err());
    let v = match res {
        Err(e) => e.0,
        Ok(()) => panic!("expected throw"),
    };
    assert_eq!(interp.to_display_string(v), "42");
}
