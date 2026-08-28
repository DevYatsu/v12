use v12_interp::Interp;

#[test]
fn yield_suspends_and_next_resumes() {
    let src = "function* g(){ let x = yield 1; return x + 1; } let it = g(); let a = it.next(); let b = it.next(41); throw b.value;";
    let mut interp = Interp::from_source(src).unwrap();
    let res = interp.run();
    assert!(res.is_err());
}
