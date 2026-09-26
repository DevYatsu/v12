//! `Interp::error_value` must survive a collection between allocating the
//! error object and installing its properties.
//!
//! `error_value` roots the fresh object with `heap.add_root`, but every
//! `gc_protect` — its own, and the one `define_own_data_attrs` takes before it
//! touches the receiver — clears the transient root set before collecting. The
//! slot was therefore unrooted at the collection, and the `heap.get(obj)` that
//! follows dereferenced a dead slot (`stale object handle: index N refers to a
//! dead slot`). `heap.gc_stress(Some(1))` — the same knob as the CLI's
//! `--expose-gc` — collects at every safepoint, so these cases reproduce it
//! deterministically on the first call.

use v12_heap::{GcPolicy, Heap, Kind};
use v12_interp::{Interp, JSException};

/// A heap that collects at every safepoint (`--expose-gc` in the CLI).
fn stress_heap() -> Heap {
    let mut heap = Heap::new(GcPolicy::NoGC);
    heap.gc_stress(Some(1));
    heap
}

#[test]
fn error_value_returns_usable_error_under_collection() {
    let mut heap = stress_heap();
    let mut interp = Interp::from_source(&mut heap, "1;").expect("compile");
    let err = interp.error_value("TypeError: boom");
    let handle = err.as_object().expect("error_value returns an object");
    assert_eq!(interp.heap().get(handle).kind, Kind::Error);
    // `error_value` hands back an unrooted value (rooting it is the caller's
    // job), so root it before reading it back through a decoding path.
    interp.heap_mut().add_root(err);
    assert_eq!(interp.to_display_string(err), "TypeError: boom");
    assert!(
        interp.heap().collections() > 0,
        "the stress cadence actually collected during error_value"
    );
}

#[test]
fn thrown_type_error_is_usable_under_collection() {
    // End-to-end: the property-read TypeError that every engine-generated error
    // goes through, with no promise involved.
    let mut heap = stress_heap();
    let mut interp = Interp::from_source(&mut heap, "null.foo;").expect("compile");
    let thrown = match interp.run() {
        Ok(()) => panic!("`null.foo` must throw"),
        Err(JSException(v)) => v,
    };
    interp.heap_mut().add_root(thrown);
    assert_eq!(
        interp.to_display_string(thrown),
        "TypeError: Cannot read properties of null (reading 'foo')"
    );
    // The interpreter is still usable after the throw.
    let mut after = Interp::from_source(&mut heap, "'still alive';").expect("compile");
    after.run().expect("unrelated script still runs");
    assert!(!after.completion_value().expect("completion").is_undefined());
}
