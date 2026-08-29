use v12_heap::{GcPolicy, Heap};
use v12_interp::Interp;

#[test]
fn await_resumes_after_promise_resolves() {
    let src2 = "async function f(){ let x = await 42; return x; } let p = f();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src2).unwrap();
    let res = interp.run();
    if let Err(e) = res {
        panic!("run failed: {}", interp.to_display_string(e.0));
    }
    assert!(interp.pending_jobs() > 0, "expected await to enqueue job FIFO pending={}", interp.pending_jobs());
    let n = interp.run_jobs();
    assert!(n > 0);
    let src3 = "async function f(){ let a = await 1; let b = await 2; return a + b; } f();";
    let mut heap3 = Heap::new(GcPolicy::NoGC);
    let mut interp3 = Interp::from_source(&mut heap3, src3).unwrap();
    let _ = interp3.run();
    assert_eq!(interp3.pending_jobs(), 1);
    let n3 = interp3.run_jobs();
    assert!(n3 >= 1);
}

#[test]
fn await_with_promise_value() {
    let src = "async function f(){ let x = await Promise.resolve(42); return x; } let p = f();";
    let mut heap = Heap::new(GcPolicy::NoGC);
    let mut interp = Interp::from_source(&mut heap, src).unwrap();
    let res = interp.run();
    if let Err(e) = res {
        panic!("run failed: {}", interp.to_display_string(e.0));
    }
    assert!(interp.pending_jobs() > 0, "await Promise.resolve should enqueue job pending={}", interp.pending_jobs());
    let n = interp.run_jobs();
    assert!(n > 0);
}
