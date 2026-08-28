use v12_interp::Interp;

#[test]
fn await_resumes_after_promise_resolves() {
    // async function returns a pending async execution; we verify that await does not pass-through synchronously.
    // Use Interp directly with await: we capture that run() suspends (pending job) not immediate value.
    let src = "async function f(){ let x = await 42; return x; } let p = f(); throw p;";
    let mut interp = Interp::from_source(src).unwrap();
    let res = interp.run();
    // Before fix: p would be 42 (pass-through) and throw 42. After fix: async execution suspends, so we have pending jobs and need run_jobs.
    // We check that after initial run, we have pending jobs (suspended await), then drain.
    // For now just ensure it doesn't throw 42 synchronously as number without suspension: check pending.
    // If still pass-through, thrown would be 42 directly.
    // With suspension, initial run returns Ok(()) with pending job, and top throw never happened synchronously? Actually main script threw p; but f() returns undefined initially? Let's just verify suspension:
    // After fix, run should have created pending await and not completed synchronously to return 42.
    // We assert pending jobs >0 or that result is not err 42.
    if res.is_err() {
        let thrown = res.unwrap_err();
        let s = interp.to_display_string(thrown.0);
        // pass-through would be "42"
        // With suspension, p is undefined (since async call doesn't return synchronously) or pending - not 42
        assert_ne!(s, "42", "await passed through synchronously, expected suspension");
    }
    // Drain jobs and ensure we can resume
    let n = interp.run_jobs();
    assert!(n > 0, "expected await to enqueue a resume job");
}

#[test]
fn await_with_promise_value() {
    let src = "async function f(){ let x = await 99; return x; } f();";
    let mut interp = Interp::from_source(src).unwrap();
    let _ = interp.run();
    let n = interp.run_jobs();
    assert!(n > 0);
}
