use v12_interp::Interp;

#[test]
fn next_returns_iterator_result() {
    let src = "function* g(){ yield 1; yield 2; } let it=g(); let r1=it.next(); let r2=it.next(); let r3=it.next(); throw [r1.value, r1.done, r2.done, r3.done].join(',');";
    let mut interp = Interp::from_source(src).unwrap();
    let thrown = interp.run().unwrap_err();
    assert_eq!(interp.to_display_string(thrown.0), "1,false,false,true");
}
