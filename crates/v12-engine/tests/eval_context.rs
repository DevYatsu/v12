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
fn direct_eval_argument_early_errors_suppress_side_effects() {
    for source in [
        "function f(arguments) { eval(\"marker = 1; var arguments = 'param'\"); } f(1)",
        "function f() { let arguments; eval(\"marker = 1; var arguments = 'lexical'\"); } f()",
        "function f() { var arguments; eval(\"marker = 1; var arguments = 'var'\"); } f()",
    ] {
        let value = result(&format!(
            "try {{ {source} }} catch (e) {{ e.name + ':' + typeof marker }}"
        ));
        assert_eq!(value, "SyntaxError:undefined");
    }
}

#[test]
fn nested_arrow_rejects_var_arguments_from_non_simple_owner() {
    let value =
        result("function outer(a = 1) { return (() => eval(\"var arguments; 1\"))(); } outer()");
    assert!(value.starts_with("threw: SyntaxError"), "{value}");
}

#[test]
fn eval_owned_vars_route_to_caller_or_eval_local_storage() {
    assert_eq!(
        result("((a = 1) => eval(\"var arguments; 1\"))() + ':' + (typeof globalThis.arguments)"),
        "1:undefined"
    );
    assert_eq!(
        result(
            "function f() { return eval(\"var arguments; 1\"); } f() + ':' + (typeof globalThis.arguments)"
        ),
        "1:undefined"
    );
    assert_eq!(
        result("function f() { return eval(\"var n = 1; eval('n = 2; n')\"); } f()"),
        "2"
    );
    assert_eq!(
        result("function f() { var x = 1; eval(\"var x = 2; x\"); return x; } f()"),
        "2"
    );
    assert_eq!(
        result("function f() { eval(\"var n = 1\"); return typeof globalThis.n; } f()"),
        "undefined"
    );
}

#[test]
fn nested_eval_contexts_keep_env_and_caller_bindings() {
    assert_eq!(
        result(
            "function f() { let x = 1; var y = 2; return eval(\"var n = 3; eval('n = 4; n + x + y')\"); } f()"
        ),
        "7"
    );
    assert_eq!(
        result(
            "function first() { return (() => eval(\"var stale = 1; stale\"))(); } function second() { return (() => eval(\"typeof stale\"))(); } first() + ':' + second()"
        ),
        "1:undefined"
    );
}

#[test]
fn thrown_eval_frame_context_does_not_leak_to_later_frame() {
    assert_eq!(
        result(
            "function first() { try { return (() => eval(\"throw new Error('x')\"))(); } catch (e) { return 'caught'; } } function second() { return (() => eval(\"typeof stale\"))(); } first() + ':' + second()"
        ),
        "caught:undefined"
    );
}

#[test]
fn top_level_and_indirect_eval_keep_global_var_scope() {
    assert_eq!(
        result("eval(\"var topLevelProbe = 7\"); topLevelProbe"),
        "7"
    );
    assert_eq!(
        result("eval(\"var ownGlobal = 1\"); globalThis.ownGlobal"),
        "1"
    );
    assert_eq!(
        result(
            "function existingFunction() { return 1; } eval(\"function existingFunction() { return 2; }\"); existingFunction()"
        ),
        "2"
    );
    assert_eq!(
        result("var existingVar = 1; eval(\"var existingVar = 2\"); existingVar"),
        "2"
    );
    assert_eq!(
        result(
            "((a = 1) => { eval('var arrowLocal = 1'); return arrowLocal + ':' + (typeof globalThis.arrowLocal); })()"
        ),
        "1:undefined"
    );
    assert_eq!(
        result("try { eval('return 1'); } catch (e) { e.name; }"),
        "SyntaxError"
    );
    assert_eq!(
        result("function f() { return (0, eval)(\"var indirectProbe = 2; indirectProbe\"); } f()"),
        "2"
    );
    assert_eq!(
        result(
            "function f() { return (0, eval)(\"var indirectProbe = 2; indirectProbe\") + ':' + (typeof globalThis.indirectProbe); } f()"
        ),
        "2:number"
    );
}

#[test]
fn direct_eval_keeps_legal_argument_and_eval_bindings() {
    assert_eq!(result("((a = 1) => eval(\"var arguments; 1\"))()"), "1");
    assert_eq!(
        result("function f() { eval(\"var eval = 1\"); return eval; } f()"),
        "1"
    );
    assert_eq!(
        result(
            "function f() { 'use strict'; try { eval(\"var eval = 1\"); } catch (e) { return e.name; } } f()"
        ),
        "SyntaxError"
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
