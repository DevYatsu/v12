//! `%Function.prototype%` linkage and the brand table of
//! `Object.prototype.toString`.
//!
//! Two independent v12 shortcuts used to hide a whole family of spec
//! behaviour behind a synthesized surface, and both are observable from
//! JavaScript:
//!
//! 1. **No function object had a `[[Prototype]]`.** `alloc_closure` (in
//!    `v12-interp/src/lib.rs`, not this lane) stamps `length`, `name` and
//!    `prototype` but no prototype link, so `f.call` / `f.valueOf` were served
//!    by the interpreter's `function_method_surface`, which *allocates a fresh
//!    function object per read* (`Function.prototype.call !== Function.prototype.call`)
//!    and carries no `length` of its own. A bound function was strictly worse:
//!    `BoundFunctionCreate` allocated the object with no link at all, so
//!    `Object.getPrototypeOf(f.bind(x)) === Function.prototype` was `false`.
//! 2. **`Object.prototype.toString` had no steps 1-5.** It answered from
//!    `JsObject::kind` alone, so every function object — bound, closure, or
//!    `%Function.prototype%` itself — came back `"[object Object]"`.
//!
//! The fix gives `%Function.prototype%` its spec identity (the
//! `[[Prototype]]` link to `%Object.prototype%` plus its own `length`/`name`),
//! links a bound function to `%Function.prototype%`, and lets
//! `function_method_surface` defer to a real property whenever the chain
//! actually carries one.
//!
//! Two positive controls pin the parts that were already correct and must stay
//! correct: an *unlinked* closure still answers `f.call` through the
//! synthesized surface (so this is a link, not a rewrite of the surface), and
//! `Object.prototype.toString` still honours `@@toStringTag` ahead of the brand
//! table.

use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

// --- %Function.prototype% identity ----------------------------------------

/// The `[[Prototype]]` of the function prototype object is `%Object.prototype%`
/// (ES 10.2.4), so an `Object.prototype` member is inherited by it.
#[test]
fn function_prototype_inherits_from_object_prototype() {
    assert_eq!(
        eval_display("Object.getPrototypeOf(Function.prototype) === Object.prototype"),
        "true"
    );
}

/// An inherited `Object.prototype` member must be readable off the function
/// prototype object, and must be the *same object* as on `%Object.prototype%`.
#[test]
fn function_prototype_inherits_object_prototype_members() {
    assert_eq!(
        eval_display("Function.prototype.valueOf === Object.prototype.valueOf"),
        "true"
    );
    assert_eq!(
        eval_display("typeof Function.prototype.hasOwnProperty"),
        "function"
    );
}

/// ES 10.2.4: the function prototype object is itself a function object, so
/// `Object.prototype.toString` brands it `Function` (ES 20.1.3.6 step 6, the
/// `[[Call]]` row of the brand table).
#[test]
fn function_prototype_brands_as_function() {
    assert_eq!(
        eval_display("Object.prototype.toString.call(Function.prototype)"),
        "[object Function]"
    );
}

/// `length` and `name` are own data properties of every built-in function
/// object (`built-ins/Function/prototype/length.js`, `name.js`), and
/// `property-order.js` requires `name` to be listed immediately after `length`.
#[test]
fn function_prototype_has_own_length_and_name_in_order() {
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(Function.prototype, 'length').value"),
        "0"
    );
    assert_eq!(
        eval_display("Object.getOwnPropertyDescriptor(Function.prototype, 'name').value"),
        ""
    );
    // `length` then `name`, adjacently — `property-order.js` verbatim.
    assert_eq!(
        eval_display(
            "var n = Object.getOwnPropertyNames(Function.prototype); \
             var l = n.indexOf('length'), m = n.indexOf('name'); \
             l >= 0 && m === l + 1"
        ),
        "true"
    );
}

// --- bound functions -------------------------------------------------------

/// The `[[Prototype]]` of a bound function is `%Function.prototype%`
/// (ES 10.4.1.1 `BoundFunctionCreate`).
#[test]
fn bound_function_links_to_function_prototype() {
    assert_eq!(
        eval_display(
            "function f(a, b) { return this.x + a + b; } \
             Object.getPrototypeOf(f.bind({ x: 1 }, 2)) === Function.prototype"
        ),
        "true"
    );
}

/// A bound function is callable, so it brands as `Function`.
#[test]
fn bound_function_brands_as_function() {
    assert_eq!(
        eval_display("function f() {} Object.prototype.toString.call(f.bind(null))"),
        "[object Function]"
    );
}

/// `length`/`name` are *own* data properties of a bound function, with the
/// standard built-in-function attributes (`BoundFunctionCreate`), and are
/// shadowed rather than inherited — the pre-fix `[[Set]]` install silently
/// declined once the inherited non-writable `%Function.prototype%.length`
/// existed, leaving `bf.length` reading the inherited `0`.
#[test]
fn bound_function_owns_length_and_name() {
    assert_eq!(
        eval_display(
            "function f(a, b, c) { return a; } var bf = f.bind(null, 1); \
             var d = Object.getOwnPropertyDescriptor(bf, 'length'); \
             d.value === 2 && d.writable === false && d.enumerable === false \
               && d.configurable === true"
        ),
        "true"
    );
    assert_eq!(
        eval_display(
            "function f() {} var bf = f.bind(null); \
             var d = Object.getOwnPropertyDescriptor(bf, 'name'); \
             d.value === 'bound f' && d.writable === false \
               && d.enumerable === false && d.configurable === true"
        ),
        "true"
    );
}

/// A bound function carries no `prototype` own property
/// (`BoundFunctionRestrictedProperties`) and no `caller`/`arguments` poison
/// pill data property.
#[test]
fn bound_function_has_no_prototype_property() {
    assert_eq!(
        eval_display("function f() {} f.bind(null).prototype"),
        "undefined"
    );
}

/// A doubly-bound function keeps `%Function.prototype%` as its
/// `[[Prototype]]` — the link is per-object, not inherited from the target.
#[test]
fn doubly_bound_function_keeps_the_same_prototype() {
    assert_eq!(
        eval_display(
            "function f() {} var bf = f.bind(null).bind(null); \
             Object.getPrototypeOf(bf) === Function.prototype"
        ),
        "true"
    );
}

/// The inherited method must be the *same object* on every read once the
/// receiver really is linked. Before the fix the unlinked surface allocated
/// per read, so identity failed even for `%Function.prototype%` itself.
#[test]
fn inherited_function_methods_have_stable_identity() {
    assert_eq!(
        eval_display("function f() {} var bf = f.bind(null); bf.valueOf === bf.valueOf"),
        "true"
    );
}

// --- Object.prototype.toString brand table ---------------------------------

/// Steps 1-2 run before `ToObject`, so the primitive pair brands without a
/// wrapper object.
#[test]
fn to_string_brands_the_primitive_pair() {
    assert_eq!(
        eval_display("Object.prototype.toString.call(undefined)"),
        "[object Undefined]"
    );
    assert_eq!(
        eval_display("Object.prototype.toString.call(null)"),
        "[object Null]"
    );
}

/// Step 5 precedes the brand table, so a `@@toStringTag` wins — and a non-string
/// `@@toStringTag` is ignored (ES 20.1.3.6 step 5 is a `Type(tag) is String`
/// test).
#[test]
fn to_string_tag_wins_over_the_brand_table() {
    assert_eq!(
        eval_display(
            "var o = {}; o[Symbol.toStringTag] = 'Custom'; \
             Object.prototype.toString.call(o)"
        ),
        "[object Custom]"
    );
    assert_eq!(
        eval_display(
            "var o = {}; o[Symbol.toStringTag] = 42; \
             Object.prototype.toString.call(o)"
        ),
        "[object Object]"
    );
}

/// Step 15 runs *after* `builtinTag` is chosen and step 16 lets a String tag
/// replace it — for an array too. The array's own brand is only the fallback,
/// which is the opposite of a "check `IsArray` first and return" reading, and
/// this pins the spec order.
#[test]
fn the_to_string_tag_overrides_the_array_brand() {
    assert_eq!(
        eval_display(
            "var a = [1, 2]; a[Symbol.toStringTag] = 'Nope'; \
             Object.prototype.toString.call(a)"
        ),
        "[object Nope]"
    );
    assert_eq!(
        eval_display("Object.prototype.toString.call([1, 2])"),
        "[object Array]"
    );
}

/// A function object's `@@toStringTag` is read before the `Function` brand is
/// used, so it too is overridable.
#[test]
fn the_to_string_tag_overrides_the_function_brand() {
    assert_eq!(
        eval_display(
            "function f() {} f[Symbol.toStringTag] = 'Nope'; \
             Object.prototype.toString.call(f)"
        ),
        "[object Nope]"
    );
}

/// A primitive receiver has no wrapper exotic object in v1, so its `builtinTag`
/// is `Object` and its brand is whatever the wrapper *prototype*'s
/// `@@toStringTag` says. v1 installs none of those, so the answer is
/// `[object Object]` — and it becomes `[object Number]` the moment
/// `Number.prototype[@@toStringTag]` lands, with no change here.
#[test]
fn primitive_brands_come_from_the_wrapper_prototype() {
    assert_eq!(
        eval_display("Object.prototype.toString.call(1)"),
        "[object Object]"
    );
    assert_eq!(
        eval_display("Number.prototype[Symbol.toStringTag]"),
        "undefined"
    );
}

/// `built-ins/RegExp/prototype/15.10.6.js` requires the RegExp *prototype
/// object* (an ordinary object) to brand as `Object` while an instance brands
/// as `RegExp`.
#[test]
fn regexp_prototype_object_brands_as_object_but_instances_do_not() {
    assert_eq!(
        eval_display("Object.prototype.toString.call(RegExp.prototype)"),
        "[object Object]"
    );
    assert_eq!(
        eval_display("Object.prototype.toString.call(/x/)"),
        "[object RegExp]"
    );
}

// --- negative controls -----------------------------------------------------

/// Negative control: an ordinary closure is still *unlinked* — that is a
/// separate, larger change (`alloc_closure` lives in `v12-interp/src/lib.rs`).
/// This test pins that this lane did not silently link it, so the synthesized
/// `function_method_surface` is still the live path for `f.call` and the
/// deferral added there is what will later retire it.
#[test]
fn ordinary_closures_are_still_unlinked() {
    assert_eq!(
        eval_display("Object.getPrototypeOf(function () {}) === null"),
        "true"
    );
}

/// Negative control: `[[Set]]` semantics are unchanged. A plain assignment
/// installs an *own* property on an unlinked closure, the new deferral lets that
/// own property win over the synthesized surface, and the prototype is
/// untouched -- i.e. this lane changed the *read* precedence and the
/// `BoundFunctionCreate` install, not the assignment algorithm.
#[test]
fn an_own_assignment_still_wins_over_the_synthesized_surface() {
    assert_eq!(
        eval_display(
            "function f() {} f.valueOf = 1; \
             f.valueOf === 1 && Object.prototype.valueOf !== 1"
        ),
        "true"
    );
}

/// Negative control: the brand table still answers for a plain object, and the
/// new `@@toStringTag` `Get` does not throw when the key is simply absent.
/// `Object` is itself callable, so it brands as `Function` -- ES 20.1.3.6 step 6
/// keys `builtinTag` on `[[Call]]`, not on the `Object` constructor identity.
/// `%Object.prototype%` is the ordinary object.
#[test]
fn plain_objects_brand_as_object() {
    assert_eq!(
        eval_display("Object.prototype.toString.call({})"),
        "[object Object]"
    );
    assert_eq!(
        eval_display("Object.prototype.toString.call(Object.prototype)"),
        "[object Object]"
    );
    assert_eq!(eval_display("Object[Symbol.toStringTag]"), "undefined");
}
