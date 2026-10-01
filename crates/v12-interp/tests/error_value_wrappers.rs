//! `Interp::{type,range,syntax,reference}_error` and the prefix strategy
//! split they exist to introduce.
//!
//! Two facts are pinned here, and they pull in opposite directions:
//!
//! 1. `error_value` stays **permissive** — any `"Head: msg"` head becomes the
//!    `name`, verbatim. Every one of its existing call sites was written
//!    against that, so delegating it to `v12_native::parse_error_text` (which
//!    only honors a prefix in `KNOWN_KINDS`) would silently rename every
//!    thrown error whose head is not one of the seven recognized kinds.
//! 2. `error_value_kind` — and therefore the four wrappers — **validates**,
//!    so an unrecognized head falls back to the wrapper's own kind and stays
//!    inside the `message`.

use v12_heap::{GcPolicy, Heap, JsValue, Kind};
use v12_interp::Interp;

/// A fresh `Interp` over a trivial script, bound to a `Heap` the caller keeps
/// alive for as long as the interpreter. The error objects are built directly
/// on the `Interp` (no JS involved), so the script is only there to give
/// `from_source` something to compile.
macro_rules! interp {
    ($heap:ident, $interp:ident) => {
        let mut $heap = Heap::new(GcPolicy::NoGC);
        #[allow(unused_mut)]
        let mut $interp = Interp::from_source(&mut $heap, "1;").expect("compile");
    };
}

/// `properties[0]`/`[1]` are the name/message strings the display path reads
/// positionally; assert both are strings so a renamed slot is caught here and
/// not silently rendered.
fn assert_name_and_message(interp: &mut Interp, err: JsValue, name: &str, message: &str) {
    let handle = err.as_object().expect("error_value returns an object");
    interp.heap_mut().add_root(err);
    assert_eq!(interp.heap().get(handle).kind, Kind::Error);
    let obj = interp.heap().get(handle);
    assert!(
        obj.properties[0].is_string(),
        "properties[0] must be the name"
    );
    assert!(
        obj.properties[1].is_string(),
        "properties[1] must be the message"
    );
    assert_eq!(interp.to_display_string(err), format!("{name}: {message}"));
}

#[test]
fn the_four_wrappers_set_their_own_name() {
    interp!(heap, interp);
    let e = interp.type_error("nope");
    assert_name_and_message(&mut interp, e, "TypeError", "nope");
    let e = interp.range_error("too big");
    assert_name_and_message(&mut interp, e, "RangeError", "too big");
    let e = interp.syntax_error("bad token");
    assert_name_and_message(&mut interp, e, "SyntaxError", "bad token");
    let e = interp.reference_error("undeclared");
    assert_name_and_message(&mut interp, e, "ReferenceError", "undeclared");
}

#[test]
fn wrappers_honor_a_known_prefix_over_their_own_kind() {
    interp!(heap, interp);
    // A recognized head wins over the wrapper's default kind, and the stored
    // `message` does not repeat it.
    let e = interp.range_error("TypeError: not a number");
    assert_name_and_message(&mut interp, e, "TypeError", "not a number");
    let e = interp.error_value_kind("RangeError", "URIError: malformed");
    assert_name_and_message(&mut interp, e, "URIError", "malformed");
}

#[test]
fn error_value_stays_permissive_for_an_unknown_prefix() {
    // The load-bearing case: `WeirdError` is NOT in `parse_error_text`'s
    // `KNOWN_KINDS`. `error_value` must still name it `WeirdError`…
    interp!(heap, interp);
    let e = interp.error_value("WeirdError: boom");
    assert_name_and_message(&mut interp, e, "WeirdError", "boom");

    // …while the validating path is the one that drops it into the message.
    interp!(heap2, interp2);
    let e = interp2.error_value_kind("TypeError", "WeirdError: boom");
    assert_name_and_message(&mut interp2, e, "TypeError", "WeirdError: boom");
    let e = interp2.type_error("WeirdError: boom");
    assert_name_and_message(&mut interp2, e, "TypeError", "WeirdError: boom");
}

#[test]
fn error_value_without_a_prefix_defaults_to_error() {
    interp!(heap, interp);
    let e = interp.error_value("plain");
    assert_name_and_message(&mut interp, e, "Error", "plain");
    // Only the FIRST `": "` splits, so a colon inside the message survives.
    let e = interp.error_value("TypeError: at 1:2");
    assert_name_and_message(&mut interp, e, "TypeError", "at 1:2");
    // A bare `"Error"` prefix splits into an empty message. `to_display_string`
    // renders a bare name in that case (its own pre-existing contract), so
    // check the message slot directly instead of through the display string.
    let e = interp.error_value("Error: ");
    let handle = e.as_object().expect("object");
    interp.heap_mut().add_root(e);
    assert_eq!(interp.to_display_string(e), "Error");
    let empty = JsValue::string(interp.heap_mut().intern_text(""));
    assert_eq!(interp.heap().get(handle).properties[1], empty);
}
