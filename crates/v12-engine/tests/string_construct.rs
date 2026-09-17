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
fn string_symbol_call_descriptive_construct_throws() {
    // ES 22.1.1.1: the call form returns SymbolDescriptiveString (v1 has
    // opaque symbols, so `Symbol()`); only `new String(symbol)` throws.
    assert_eq!(eval_display("String(Symbol('x'))"), "Symbol()");
    assert_eq!(
        eval_display("try { new String(Symbol('x')); 'no'; } catch (e) { 'threw'; }"),
        "threw"
    );
}

#[test]
fn string_primitives_unchanged() {
    assert_eq!(eval_display("String()"), "");
    assert_eq!(eval_display("String(null)"), "null");
    assert_eq!(eval_display("String(12)"), "12");
    assert_eq!(eval_display("String(true)"), "true");
    assert_eq!(eval_display("String('x')"), "x");
}

#[test]
fn number_invokes_object_value_of() {
    // `Number(x)` uses the default-hint ToPrimitive (`valueOf` first), so a
    // user `valueOf` is honored; previously every object yielded NaN.
    assert_eq!(eval_display("Number({ valueOf() { return 42; } })"), "42");
    assert_eq!(
        eval_display("new Number({ valueOf() { return 42; } })"),
        "42"
    );
    // `toString` is consulted only when `valueOf` yields no primitive.
    assert_eq!(eval_display("Number({ toString() { return '7'; } })"), "7");
    assert_eq!(eval_display("Number('5')"), "5");
    assert_eq!(eval_display("Number()"), "0");
}
