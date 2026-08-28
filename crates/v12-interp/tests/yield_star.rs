use v12_interp::Interp;

#[test]
fn yield_star_delegates_array() {
    let src = "function* g(){ yield* [1,2]; } let it=g(); let a=it.next(); let b=it.next(); let c=it.next(); throw [a.value,b.value,c.done].join(',');";
    let mut interp = Interp::from_source(src).unwrap();
    let thrown = interp.run().unwrap_err();
    assert_eq!(interp.to_display_string(thrown.0), "1,2,true");
}
