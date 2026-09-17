//! `String(x)` coercion regressions: the constructor must perform ES
//! `ToString` (string-hint `ToPrimitive`), invoking a user `toString`, for
//! both `String(x)` and `new String(x)`.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

#[test]
fn string_invokes_object_to_string() {
    assert_eq!(
        eval_display("String({ toString() { return 'CUSTOM'; } })"),
        "CUSTOM"
    );
}

#[test]
fn string_uses_string_hint_order() {
    // ES `ToString` runs `OrdinaryToPrimitive` with hint "string":
    // `toString` first, then `valueOf`. The default hint is the reverse.
    assert_eq!(
        eval_display("String({ toString() { return 't'; }, valueOf() { return 'v'; } })"),
        "t"
    );
}

#[test]
fn new_string_invokes_object_to_string() {
    assert_eq!(
        eval_display("new String({ toString() { return 'CUSTOM'; } })"),
        "CUSTOM"
    );
}

#[test]
fn string_symbol_throws_type_error() {
    assert_eq!(
        eval_display("try { String(Symbol('x')); 'no'; } catch (e) { 'threw'; }"),
        "threw"
    );
    assert_eq!(
        eval_display("try { new String(Symbol('x')); 'no'; } catch (e) { 'threw'; }"),
        "threw"
    );
}

#[test]
fn string_primitives_unchanged() {
    assert_eq!(eval_display("String()"), "undefined");
    assert_eq!(eval_display("String(null)"), "null");
    assert_eq!(eval_display("String(12)"), "12");
    assert_eq!(eval_display("String(true)"), "true");
    assert_eq!(eval_display("String('x')"), "x");
}
