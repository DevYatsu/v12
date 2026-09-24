use test_support::{eval_src, value_to_string};
use v12_bccompiler::compile_source;

fn result(source: &str) -> String {
    value_to_string(&eval_src(source))
}

#[test]
fn annex_b_collision_matrix_preserves_caller_bindings() {
    assert_eq!(
        result("return function (a) { { function a() {} } return a; }(1);"),
        "1"
    );
    assert_eq!(
        result("return function (a = 2) { { function a() {} } return a; }();"),
        "2"
    );
    assert_eq!(
        result("return function ({a}) { { function a() {} } return a; }({a: 3});"),
        "3"
    );
    assert!(compile_source("function f(...a) { { function a() {} } return a[0]; }").is_ok());
    assert!(
        compile_source("function f() { { function arguments() {} } return arguments[0]; }").is_ok()
    );
}

#[test]
fn annex_b_nested_blocks_and_declaration_order_are_scoped() {
    assert_eq!(
        result(
            "return function () { let f = 1; { function f() {} } { function g() { return 1; } } return f + ':' + g(); }();"
        ),
        "1:1"
    );
    assert_eq!(
        result(
            "return function () { { function f() { return 1; } function f() { return 2; } } return f(); }();"
        ),
        "2"
    );
}

#[test]
fn annex_b_control_flow_containers_update_function_scope() {
    for source in [
        "function f() { for (var i = 0; i < 1; i++) { function g() {} } return g; }",
        "function f() { for (var k in {x: 1}) { function g() {} } return g; }",
        "function f() { for (var k of [1]) { function g() {} } return g; }",
        "function f() { try { throw 1; } catch (e) { function g() {} } return g; }",
        "function f() { switch (1) { case 1: function g() {} } return g; }",
    ] {
        assert!(compile_source(source).is_ok(), "{source}");
    }
}

#[test]
fn annex_b_existing_var_and_updates_keep_initialization_order() {
    assert_eq!(
        result("return function () { var f = 1; { function f() { return 6; } } return f(); }();"),
        "6"
    );
    assert_eq!(
        result("return function () { var f; { function f() { return 7; } } return f(); }();"),
        "7"
    );
    assert_eq!(
        result(
            "return function () { var f = 1; { f = 8; function f() { return 9; } } return f; }();"
        ),
        "function"
    );
    assert_eq!(
        result("return function () { { function f() { return 9; } f = 8; } return f; }();"),
        "8"
    );
}

#[test]
fn annex_b_nested_functions_do_not_share_block_plans() {
    assert_eq!(
        result(
            "return function () { { function f() { return 1; } } function g() { return typeof f; } return g(); }();"
        ),
        "function"
    );
    assert_eq!(
        result(
            "return function () { function outer() { { function f() { return 10; } } return f(); } return outer(); }();"
        ),
        "10"
    );
}

#[test]
fn annex_b_strict_rejects_block_functions() {
    let error = compile_source("function f() { 'use strict'; { function g() {} } }")
        .expect_err("strict block function must be rejected");
    assert!(error.message.contains("strict"), "{}", error.message);
}

#[test]
fn annex_b_eval_reachable_block_binding_promotes_storage() {
    let program =
        compile_source("function outer() { var f = 1; { function f() {} } return eval('f'); }")
            .expect("eval-reachable function compiles");
    let outer = program
        .functions
        .iter()
        .find(|function| {
            function
                .name_hint
                .as_deref()
                .is_some_and(|name| name.ends_with(":outer"))
        })
        .expect("outer function");
    let f = outer
        .eval_metadata
        .as_ref()
        .expect("eval metadata")
        .bindings
        .iter()
        .find(|(name, _)| name == "f")
        .expect("f binding");
    assert!(matches!(f.1, v12_bytecode::BindingLocation::Env(_)));
}
#[test]
fn annex_b_nested_block_function_keeps_outer_binding() {
    assert_eq!(
        result(
            "return function g() { { function f() { return 1; } { function f() { return 2; } } } return f(); }();"
        ),
        "1"
    );
}

#[test]
fn annex_b_parameter_collision_keeps_function_binding() {
    assert_eq!(
        value_to_string(&eval_src(
            "return (function (f) {\n  var before = f;\n  if (true) function f() {} else function _f() {}\n  return before + ':' + f;\n})(123);"
        )),
        "123:123"
    );
}

#[test]
fn annex_b_outer_lexical_binding_survives_block_function() {
    assert_eq!(
        value_to_string(&eval_src(
            "return (function () {\n  let f = 123;\n  { function f() {} }\n  return f;\n})();"
        )),
        "123"
    );
}
