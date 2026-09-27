//! `super.x` inside an object-literal method (ES §13.2.5.1 `GetValue` on a
//! super reference).
//!
//! The bug these pin: the lowering used to resolve `super` in an
//! object-literal method to the receiver itself, so `{ get x() { return
//! super.x } }` became `this.x` — a self-call whenever the literal owns an
//! `x`, recursing on the Rust stack until the process aborted with
//! `fatal runtime error: stack overflow`. `super` must start the lookup one
//! link *above* the receiver, so the literal's own properties are skipped.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// The crashing Test262 shape (`language/expressions/object/getter-super-prop.js`):
/// the literal owns `x` as an accessor, so the pre-fix `this.x` self-call
/// overflowed. Post-fix it reads the prototype's accessor with `this` as the
/// receiver.
#[test]
fn object_literal_getter_super_reads_the_prototype() {
    assert_eq!(
        eval_display(
            "var proto = { _x: 42, get x() { return 'proto' + this._x; } };
             var object = { get x() { return super.x; } };
             Object.setPrototypeOf(object, proto);
             [object.x, object._x, Object.getPrototypeOf(object)._x].join(',');"
        ),
        "proto42,42,42"
    );
}

/// `super` skips the receiver's own properties: `obj.toString = null` must not
/// shadow `Object.prototype.toString` (`name-super-prop-body.js`).
#[test]
fn object_literal_super_skips_own_properties() {
    assert_eq!(
        eval_display(
            "var obj = { method() { return super.toString; } };
             obj.toString = null;
             obj.method() === Object.prototype.toString"
        ),
        "true"
    );
}

/// The self-recursive shape that overflowed: a method whose prototype has no
/// `x` at all now reads `undefined` instead of calling itself forever.
#[test]
fn object_literal_super_without_a_prototype_property_terminates() {
    assert_eq!(
        eval_display("var o = { get x() { return super.x; } }; String(o.x);"),
        "undefined"
    );
}

/// `Get` starts at the super base, so a property the *receiver* owns is not
/// consulted: `super` never sees the literal's own `x`.
#[test]
fn object_literal_super_ignores_the_receivers_own_property() {
    assert_eq!(
        eval_display("var o = { m() { return super.nope; } }; o.nope = 5; String(o.m());"),
        "undefined"
    );
}

/// An inherited accessor answers with the *receiver* as `this`, not with the
/// object the accessor lives on.
#[test]
fn object_literal_super_getter_sees_the_receiver() {
    assert_eq!(
        eval_display(
            "var p = { get who() { return this.tag; } };
             var o = { tag: 'receiver', m() { return super.who; } };
             Object.setPrototypeOf(o, p);
             o.m();"
        ),
        "receiver"
    );
}

/// `super.x(...)` takes the callee from the super base and the receiver from
/// the method's `this`.
#[test]
fn object_literal_super_call_uses_base_callee_and_receiver() {
    assert_eq!(
        eval_display(
            "var p = { m() { return 'p' + this.tag; } };
             var o = { tag: 'o', m() { return 'o' + super.m(); } };
             Object.setPrototypeOf(o, p);
             o.m();"
        ),
        "opo"
    );
}

/// Method shorthand, not just accessors.
#[test]
fn object_literal_method_super_call() {
    assert_eq!(
        eval_display(
            "var proto = { method(x) { return 'proto' + x; } };
             var object = { method(x) { return super.method(x); } };
             Object.setPrototypeOf(object, proto);
             [object.method(42), proto.method(42)].join(',')"
        ),
        "proto42,proto42"
    );
}

/// An arrow inside an object method inherits the method's `super` binding. The
/// owner lookup walks past the arrow to the method, so this resolves the
/// literal's prototype rather than the class env slot, and the receiver comes
/// from the method's `this` env slot.
#[test]
fn arrow_inside_object_method_inherits_super() {
    assert_eq!(
        eval_display(
            "var proto = { _x: 7, get x() { return this._x; } };
             var object = { m() { var f = () => super.x; return String(f()); } };
             Object.setPrototypeOf(object, proto);
             object.m();"
        ),
        "7"
    );
}

/// The class path is unchanged: `super` in a class method still resolves
/// through the constructor, and the method's own property does not shadow the
/// super base either.
#[test]
fn class_method_super_is_unaffected() {
    assert_eq!(
        eval_display(
            "class P { m() { return 'p' + this._v; } }
             class C extends P { constructor() { super(); this._v = 1; } m() { return 'c' + super.m(); } }
             new C().m()"
        ),
        "cp1"
    );
}

/// `super` inside a class *accessor* currently cannot resolve, because the
/// heap reuses one accessor descriptor for two objects that start from the
/// same shape: `P.prototype`'s `x` and `C.prototype`'s `x` end up as the same
/// function, so `super.x` lands back on the receiver's own getter and cycles.
/// That heap bug is out of scope here — what this pins is that the cycle is
/// now a catchable `RangeError` instead of the process aborting with
/// `fatal runtime error: stack overflow`.
#[test]
fn class_accessor_super_cycle_is_bounded() {
    assert_eq!(
        eval_display(
            "class P { get x() { return 'p'; } }
             class C extends P { get x() { return 'c' + super.x; } }
             try { new C().x; 'no throw' } catch (e) { e instanceof RangeError }"
        ),
        "true"
    );
}

/// `[[GetPrototypeOf]]` has no meaning for a primitive, so a borrowed
/// object method reached with a primitive `this` fails honestly instead of
/// silently reading the primitive's own property.
#[test]
fn object_literal_super_on_a_primitive_receiver_is_a_type_error() {
    assert_eq!(
        eval_display(
            "var o = { m() { return super.toString; } };
             try { o.m.call(5); 'no throw' } catch (e) { e instanceof TypeError }"
        ),
        "true"
    );
}
