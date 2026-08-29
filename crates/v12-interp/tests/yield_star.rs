use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

#[test]
fn yield_star_delegates_array() {
    let src = "function* g(){ yield* [1,2]; } let it=g(); let a=it.next(); let b=it.next(); let c=it.next(); throw [a.value,b.value,c.done].join(',');";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).unwrap();
    let thrown = interp.run().unwrap_err();
    assert_eq!(interp.to_display_string(thrown.0), "1,2,true");
}

#[test]
fn yield_star_delegates_generator() {
    let src = "function* inner(){yield 1;} function* outer(){yield* inner();} let it=outer(); let a=it.next(); let b=it.next(); throw [a.value,b.done].join(',');";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).unwrap();
    let thrown = interp.run().unwrap_err();
    assert_eq!(interp.to_display_string(thrown.0), "1,true");
}
