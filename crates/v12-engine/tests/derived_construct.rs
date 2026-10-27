//! Class construction per ES §10.2.2 and §15.7.14: `super()` is a real
//! `[[Construct]]` of the parent with `new.target`, a derived constructor's
//! `this` starts uninitialized, and `IsConstructor` is a property fixed when
//! the function object is created.
//!
//! The previous lowering called the parent on a pre-made `this`, so built-in
//! parents (`Map`, `Array`, `Promise`) never produced their exotic instance,
//! `this` before `super()` was observable, and three hand-maintained
//! `IsConstructor` lists disagreed (`class X extends Map` threw "not a
//! constructor"). These tests pin the spec behaviour of each.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

fn throws(src: &str, ctor: &str) -> bool {
    eval_display(&format!(
        "try {{ {src}; 'no throw' }} catch (e) {{ e instanceof {ctor} }}"
    )) == "true"
}

#[test]
fn builtin_parents_construct_their_own_exotic_instance() {
    assert_eq!(
        eval_display(
            "class M extends Map { two() { return this.size * 2; } } \
             var m = new M([[1, 2]]); [m instanceof M, m.get(1), m.two()].join()"
        ),
        "true,2,2"
    );
    assert_eq!(
        eval_display("class A extends Array {} var a = new A(1, 2, 3); [a.length, Array.isArray(a), a instanceof A].join()"),
        "3,true,true"
    );
    assert_eq!(
        eval_display("class N extends Number { m() { return 1; } } var n = new N(3); [n instanceof N, n.m()].join()"),
        "true,1"
    );
}

#[test]
fn derived_this_binding_rules() {
    assert!(throws("class D extends Object { constructor() { this.a = 1; super(); } } new D()", "ReferenceError"));
    assert!(throws("class D extends Object { constructor() { super(); super(); } } new D()", "ReferenceError"));
    assert!(throws("class D extends Object { constructor() {} } new D()", "ReferenceError"));
    assert!(throws("class D extends Object { constructor() { super(); return 1; } } new D()", "TypeError"));
    assert_eq!(
        eval_display("var o = {}; class D extends Object { constructor() { return o; } } new D() === o"),
        "true"
    );
}

#[test]
fn class_constructors_require_new() {
    assert!(throws("class C {} C()", "TypeError"));
}

#[test]
fn is_constructor_is_fixed_at_creation() {
    assert_eq!(
        eval_display(
            "function isC(f) { try { Reflect.construct(String, [], f); return true; } catch (e) { return false; } } \
             [isC(function () {}), isC(class {}), isC(() => 1), isC({ m() {} }.m), \
              isC(function* () {}), isC(async function () {}), isC(Math.max), \
              isC(function () {}.bind()), isC((() => 1).bind()), isC(new Proxy(function () {}, {})), \
              isC(Map), isC(Symbol), isC(new Function('')), isC((class { m() {} }).prototype.m)].join()"
        ),
        "true,true,false,false,false,false,false,true,false,true,true,true,true,false"
    );
    // Methods own no `prototype`; generators do.
    assert_eq!(
        eval_display("['prototype' in { m() {} }.m, 'prototype' in function* () {}].join()"),
        "false,true"
    );
    assert!(throws("new Symbol()", "TypeError"));
}

#[test]
fn super_property_set_calls_the_setter_with_the_receiver() {
    assert_eq!(
        eval_display(
            "var sv, o = Object.freeze({}); \
             class A { constructor() { return o; } set foo(v) { sv = v; } } \
             class B extends A { constructor() { super(); super.foo = 14; } } \
             new B(); sv"
        ),
        "14"
    );
}

#[test]
fn class_expression_name_is_scoped_to_the_class() {
    assert_eq!(
        eval_display("var A = class B { m() { return B; } }; [typeof B, new A().m() === A].join()"),
        "undefined,true"
    );
}

/// Extra actuals must not reach the callee's register window: the register
/// past the locals is the compiler's reserved `undefined` source.
#[test]
fn extra_arguments_do_not_clobber_the_undefined_register() {
    assert_eq!(
        eval_display("new (function () { this.a = undefined; })(5).a === undefined"),
        "true"
    );
    assert_eq!(
        eval_display("class P { constructor() { this.n = arguments.length; this.u = undefined; } } \
                      var p = new P(1, 2); [p.n, p.u === undefined].join()"),
        "2,true"
    );
}
