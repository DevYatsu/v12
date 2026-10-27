//! Private Names (ES §6.2.12): each `#name` is a fresh value per evaluation of
//! its class body, resolved lexically, and every private access is a brand
//! check against the object's `[[PrivateElements]]`.
//!
//! The previous lowering keyed private storage by the name's *text* with a
//! constant class id of 0, evaluated field initializers once at class
//! definition and cloned them onto instances, and compiled `this.#m()` as a
//! string-keyed property call. So two classes declaring `#x` shared a slot,
//! derived instances never received their private elements, accessors were
//! stored as plain values, and every private method call threw "callee is
//! not a function". These tests pin the spec behaviour of each of those.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

fn throws_type_error(src: &str) -> bool {
    eval_display(&format!(
        "try {{ {src}; 'no throw' }} catch (e) {{ e instanceof TypeError }}"
    )) == "true"
}

#[test]
fn private_method_call_binds_this() {
    assert_eq!(
        eval_display("class C { #m() { return this.v; } v = 7; t() { return this.#m(); } } new C().t()"),
        "7"
    );
}

#[test]
fn private_method_is_shared_and_not_writable() {
    assert_eq!(
        eval_display("class C { #m() {} same(o) { return this.#m === o.#m; } } new C().same(new C())"),
        "true"
    );
    assert!(throws_type_error("class C { #m() {} w() { this.#m = 1; } } new C().w()"));
}

#[test]
fn private_accessors_call_getter_and_setter() {
    assert_eq!(
        eval_display(
            "class C { #s = 0; get #a() { return this.#s; } set #a(v) { this.#s = v * 2; } \
             t() { this.#a = 5; this.#a += 1; return this.#a; } } new C().t()"
        ),
        "22"
    );
    assert!(throws_type_error("class C { get #g() { return 1; } w() { this.#g = 2; } } new C().w()"));
    assert!(throws_type_error("class C { set #s(v) {} r() { return this.#s; } } new C().r()"));
}

#[test]
fn field_initializers_run_per_instance_in_order() {
    assert_eq!(
        eval_display(
            "var n = 0; class C { #a = ++n; #b = this.#a * 10; get() { return this.#b; } } \
             new C(); new C().get()"
        ),
        "20"
    );
}

#[test]
fn brand_check_rejects_foreign_objects() {
    assert!(throws_type_error("class C { #x = 1; static r(o) { return o.#x; } } C.r({})"));
    // A proxy is not its target: no private elements are forwarded.
    assert!(throws_type_error(
        "class C { #x = 1; static r(o) { return o.#x; } } C.r(new Proxy(new C(), {}))"
    ));
}

#[test]
fn each_class_evaluation_mints_distinct_names() {
    assert!(throws_type_error(
        "function mk() { return class { #v = 0; static r(o) { return o.#v; } }; } \
         var A = mk(), B = mk(); A.r(new B())"
    ));
}

#[test]
fn private_in_tests_presence_and_rejects_primitives() {
    assert_eq!(
        eval_display("class C { #x; static has(o) { return #x in o; } } [C.has(new C()), C.has({})].join()"),
        "true,false"
    );
    assert!(throws_type_error("class C { #x; static has(o) { return #x in o; } } C.has(1)"));
}

#[test]
fn derived_instances_receive_base_and_own_private_elements() {
    assert_eq!(
        eval_display(
            "class A { #x = 1; x() { return this.#x; } } \
             class D extends A { #y = 2; sum() { return this.x() + this.#y; } } new D().sum()"
        ),
        "3"
    );
}

#[test]
fn static_private_methods_and_fields_live_on_the_constructor() {
    assert_eq!(
        eval_display("class S { static #c = 41; static #inc() { return ++S.#c; } static go() { return S.#inc(); } } S.go()"),
        "42"
    );
}

#[test]
fn nested_classes_resolve_names_lexically() {
    assert_eq!(
        eval_display(
            "class O { #o = 'outer'; inner() { var self = this; \
             return new (class { #i = 'inner'; get() { return self.#o + '/' + this.#i; } })().get(); } } \
             new O().inner()"
        ),
        "outer/inner"
    );
    // The inner class's own `#x` shadows the outer one.
    assert!(throws_type_error(
        "class O { #x = 1; t() { return new (class { #x = 2; r(o) { return o.#x; } })().r(this); } } new O().t()"
    ));
}

#[test]
fn private_references_work_through_arrows_chains_and_destructuring() {
    assert_eq!(
        eval_display("class Q { #q = 3; f() { return (() => () => this.#q)()(); } } new Q().f()"),
        "3"
    );
    assert_eq!(
        eval_display("class P { #p = 9; f(o) { return o?.#p; } } [new P().f(new P()), new P().f(null)].join()"),
        "9,"
    );
    assert_eq!(
        eval_display("class D { #a; #b; s(v) { [this.#a, this.#b] = v; return this.#a + this.#b; } } new D().s([1, 2])"),
        "3"
    );
    assert_eq!(
        eval_display("class U { #n = 1; f() { var r = this.#n++; return r + ',' + this.#n; } } new U().f()"),
        "1,2"
    );
}

/// A function with more than 255 registers emits the wide forms of every
/// private op and of the class-scope env accesses around them.
#[test]
fn private_names_survive_wide_register_operands() {
    let mut src = String::new();
    for i in 0..40 {
        src.push_str(&format!(
            "class C{i} {{ #v = {i}; get() {{ return this.#v; }} }} var r{i} = new C{i}().get();\n"
        ));
    }
    src.push_str("r39");
    assert_eq!(eval_display(&src), "39");
}
