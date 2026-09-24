use v12_engine::Engine;

fn result(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(value) => format!("threw: {}", engine.to_display_string(value)),
    }
}

#[test]
fn direct_eval_mutates_caller_register_and_reads_lexical_binding() {
    assert_eq!(
        result("function f(a) { eval('a = 3'); return a; } f(1)"),
        "3"
    );
    assert_eq!(
        result("function f() { let x = 1; return eval('x'); } f()"),
        "1"
    );
    assert_eq!(
        result(
            "function nested() { let x = 1; return eval(\"let y = 2; eval('x + y')\"); } nested()"
        ),
        "3"
    );
    assert_eq!(
        result(
            "function f() { return eval(\"'use strict'; missing = 1\"); } try { f(); } catch (e) { e.name; }"
        ),
        "ReferenceError"
    );
    assert_eq!(
        result("function f(){ var x='caller'; return eval(\"var x='eval'; eval('x')\"); } f()"),
        "eval"
    );
}

#[test]
fn direct_eval_closure_survives_caller_return() {
    assert_eq!(
        result(
            "function h() { let x = 1; return eval('(function () { return x; })'); } const c = h(); c()"
        ),
        "1"
    );
}

#[test]
fn direct_eval_observes_this_new_target_and_arguments() {
    assert_eq!(
        result("const r = { x: 1 }; function f() { return eval('this.x'); } f.call(r)"),
        "1"
    );
    assert_eq!(
        result("function C() { return eval('new.target'); } new C() === C"),
        "true"
    );
    assert_eq!(
        result("function f(a) { return eval('arguments[0]'); } f(7)"),
        "7"
    );
}

#[test]
fn strict_caller_rejects_undeclared_direct_eval_assignment() {
    assert_eq!(
        result(
            "function f() { 'use strict'; return eval('missing = 1'); } try { f(); } catch (e) { e.name; }"
        ),
        "ReferenceError"
    );
}

#[test]
fn indirect_and_alias_eval_remain_global_scoped() {
    assert_eq!(
        result("function f() { let x = 1; return (0, eval)('typeof x'); } f()"),
        "undefined"
    );
    assert_eq!(
        result("function f() { let x = 1; const e = eval; return e('typeof x'); } f()"),
        "undefined"
    );
}
