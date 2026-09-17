//! Class instance field initialization regressions (ES `DefineField` /
//! `InitializeInstanceElements`): a field with no initializer is still an own
//! property (`undefined`), derived-class fields initialize only after
//! `super()` returns, and declaration order is preserved.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

#[test]
fn no_initializer_field_is_own_and_undefined() {
    assert_eq!(
        eval_display(
            "class B { a; b = 1; } var o = new B(); [Object.prototype.hasOwnProperty.call(o, 'a'), typeof o.a, o.b].join(',')"
        ),
        "true,undefined,1"
    );
}

#[test]
fn derived_class_fields_initialize_after_super() {
    assert_eq!(
        eval_display(
            "class P { constructor() { this.p = 1; } } class D extends P { c; d = 2; constructor() { super(); } } var q = new D(); [Object.prototype.hasOwnProperty.call(q, 'c'), q.c === undefined, Object.prototype.hasOwnProperty.call(q, 'd'), q.d, q.p].join(',')"
        ),
        "true,true,true,2,1"
    );
}

#[test]
fn field_declaration_order_is_preserved() {
    assert_eq!(
        eval_display("class C { b = 1; a = 2; c; } Object.keys(new C()).join(',')"),
        "b,a,c"
    );
}

#[test]
fn derived_fields_visible_after_super_statement() {
    assert_eq!(
        eval_display(
            "class P { constructor() { this.p = 1; } } class D extends P { x = 2; constructor() { super(); this.y = this.x; } } var d = new D(); [d.x, d.y].join(',')"
        ),
        "2,2"
    );
}
