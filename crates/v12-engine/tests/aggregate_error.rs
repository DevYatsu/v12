//! Spec shape of `AggregateError` (ECMA-262 27.5.5).
//!
//! The two details the family gets wrong most often are pinned here: the
//! `errors` list is drained from an **iterable** (not read as an array-like),
//! and its data property is **non-enumerable** (`CreateNonEnumerableDataPropertyOrThrow`).

use v12_engine::Engine;

/// Evaluates `source` and returns the display string of the thrown value.
///
/// Every snippet here ends in `throw` so the assertion channel is a plain
/// string comparison — a `throw` inside a property getter would otherwise
/// surface as an opaque eval error rather than a mismatch.
fn thrown(source: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(source) {
        Ok(value) => engine.to_display_string(value),
        Err(t) => engine.to_display_string(t),
    }
}

/// Same channel as [`thrown`], under the name the non-throwing reads use.
fn run(source: &str) -> String {
    thrown(source)
}

#[test]
fn errors_is_the_drained_list() {
    assert_eq!(
        run("let e = new AggregateError([1, 2], 'msg'); \
            throw [e.errors.length, e.errors[0], e.errors[1]].join(',');"),
        "2,1,2"
    );
}

#[test]
fn message_and_name_read_back() {
    assert_eq!(
        run("let e = new AggregateError([1, 2], 'msg'); \
            throw [e.message, e.name, String(e)].join('|');"),
        "msg|AggregateError|AggregateError: msg"
    );
}

#[test]
fn is_an_aggregate_error_and_an_error() {
    assert_eq!(
        run("let e = new AggregateError([]); \
            throw [e instanceof AggregateError, e instanceof Error].join(',');"),
        "true,true"
    );
}

#[test]
fn errors_descriptor_is_writable_non_enumerable_configurable() {
    // The spec attribute set for `errors` (and `message`): 27.5.5.1 steps 5-6
    // are `CreateNonEnumerableDataPropertyOrThrow`. `JSON.stringify` of the
    // descriptor is the check, so the `value` key is dropped explicitly.
    let attrs = |key: &str| {
        format!(
            "let d = Object.getOwnPropertyDescriptor(e, '{key}'); \
             throw [d.writable, d.enumerable, d.configurable].join(',');"
        )
    };
    assert_eq!(
        run(&format!(
            "let e = new AggregateError([1], 'm'); {}",
            attrs("errors")
        )),
        "true,false,true"
    );
    assert_eq!(
        run(&format!(
            "let e = new AggregateError([1], 'm'); {}",
            attrs("message")
        )),
        "true,false,true"
    );
    // The whole descriptor, `value` included, as a literal string: the one
    // assertion the brief asks to see spelled out.
    assert_eq!(
        run("let e = new AggregateError([1], 'm'); \
            let d = Object.getOwnPropertyDescriptor(e, 'errors'); \
            throw JSON.stringify([d.writable, d.enumerable, d.configurable]);"),
        "[true,false,true]"
    );
}

#[test]
fn errors_is_not_enumerable() {
    // Explicit non-enumerability, not just a descriptor read: `Object.keys`
    // and `for…in` must not see it.
    assert_eq!(
        run("let e = new AggregateError([1], 'm'); \
            let seen = []; for (let k in e) seen.push(k); \
            throw [Object.keys(e).join('+'), seen.join('+')].join('|');"),
        "|"
    );
}

#[test]
fn errors_survives_an_own_property_rewrite() {
    // `configurable: true` is load-bearing: `delete` really removes it.
    assert_eq!(
        run("let e = new AggregateError([1], 'm'); \
            throw [delete e.errors, Object.prototype.hasOwnProperty.call(e, 'errors')].join(',');"),
        "true,false"
    );
}

#[test]
fn empty_error_list_and_default_message() {
    assert_eq!(
        run("let e = new AggregateError([]); \
            throw [e.errors.length, e.message, e.name].join(',');"),
        "0,,AggregateError"
    );
}

#[test]
fn undefined_message_installs_no_own_message() {
    assert_eq!(
        run(
            "throw [Object.prototype.hasOwnProperty.call(new AggregateError([]), 'message'), \
              Object.prototype.hasOwnProperty.call(new AggregateError([], undefined), 'message')] \
              .join(',');"
        ),
        "false,false"
    );
}

#[test]
fn accepts_a_set() {
    assert_eq!(
        run("let e = new AggregateError(new Set(['a', 'b'])); \
            throw [e.errors.length, e.errors[0], e.errors[1]].join(',');"),
        "2,a,b"
    );
}

#[test]
fn accepts_a_generator_iterator() {
    // A generator object is not an array: the value must go through
    // `GetIterator` + `IteratorStep`, not an index read.
    assert_eq!(
        run("function* g() { yield 'x'; yield 'y'; } \
            let e = new AggregateError(g()); \
            throw [e.errors.length, e.errors[0], e.errors[1]].join(',');"),
        "2,x,y"
    );
}

#[test]
fn accepts_a_string() {
    assert_eq!(
        run("let e = new AggregateError('ab'); \
            throw [e.errors.length, e.errors[0], e.errors[1]].join(',');"),
        "2,a,b"
    );
}

#[test]
fn accepts_a_bare_iterator_object() {
    assert_eq!(
        run("let n = 0; \
            let it = { next() { n += 1; return { value: n, done: n > 3 }; } }; \
            it[Symbol.iterator] = function () { return it; }; \
            let e = new AggregateError(it); \
            throw [e.errors.length, e.errors[0], e.errors[1], e.errors[2]].join(',');"),
        "3,1,2,3"
    );
}

#[test]
fn undefined_errors_is_a_type_error() {
    // 27.5.5.1 step 3 is `? IteratorToList(errors)` — `GetIterator(undefined)`
    // reads `@@iterator` off `undefined`, which is a `TypeError`. (The text
    // differs from the `@@iterator`-present case; only the class is pinned,
    // and test262 `errors-iterabletolist.js` only asserts the class too.)
    for source in [
        "new AggregateError();",
        "new AggregateError(undefined, 'm');",
    ] {
        let out = thrown(source);
        assert!(
            out.starts_with("TypeError"),
            "{source} should throw a TypeError, got {out}"
        );
    }
}

#[test]
fn a_non_iterable_errors_is_a_type_error() {
    let out = thrown("new AggregateError(42);");
    assert!(out.starts_with("TypeError"), "got {out}");
}

#[test]
fn a_throwing_iterator_propagates() {
    assert_eq!(
        thrown("new AggregateError({ get [Symbol.iterator]() { throw new URIError('boom'); } });"),
        "URIError: boom"
    );
}

#[test]
fn message_is_stringified_before_the_iterable_is_touched() {
    // `built-ins/AggregateError/order-of-args-evaluation.js`: `ToString(message)`
    // runs first, so the two observable effects land in that order.
    assert_eq!(
        run("let seq = []; \
            new AggregateError({ [Symbol.iterator]() { seq.push('iter'); \
              return { next() { seq.push('next'); return { done: true }; } }; } }, \
              { toString() { seq.push('str'); return ''; } }); \
            throw seq.join(',');"),
        "str,iter,next"
    );
}

#[test]
fn length_is_two() {
    assert_eq!(run("throw AggregateError.length;"), "2");
}

#[test]
fn constructor_name() {
    assert_eq!(run("throw AggregateError.name;"), "AggregateError");
}

#[test]
fn prototype_shape() {
    assert_eq!(
        run("let p = AggregateError.prototype; \
            throw [typeof p, Object.getPrototypeOf(p) === Error.prototype, \
              p.name, p.message, p.constructor === AggregateError, \
              Object.getOwnPropertyDescriptor(p, 'name').enumerable, \
              Object.getOwnPropertyDescriptor(p, 'name').writable, \
              Object.getOwnPropertyDescriptor(p, 'name').configurable, \
              Object.prototype.hasOwnProperty.call(p, 'errors')].join(',');"),
        "object,true,AggregateError,,true,false,true,true,false"
    );
}

#[test]
fn instance_constructor_back_reference() {
    assert_eq!(
        run("let e = new AggregateError([]); \
            throw [e.constructor === AggregateError, \
              Object.getOwnPropertyDescriptor(e, 'constructor').enumerable].join(',');"),
        "true,false"
    );
}

#[test]
fn is_constructor_via_reflect() {
    assert_eq!(
        run(
            "let e = Reflect.construct(AggregateError, [[[1, 2]], 'm']); \
            throw [e instanceof AggregateError, e.errors.length, e.errors[0][1], e.message, \
              e.constructor === AggregateError].join(',');"
        ),
        "true,1,2,m,true"
    );
}

#[test]
fn a_proxy_over_it_is_constructible() {
    assert_eq!(
        run("let p = new Proxy(AggregateError, {}); \
            let e = Reflect.construct(p, [[[7]], 'pm']); \
            throw [e instanceof AggregateError, e.errors[0], e.message].join(',');"),
        "true,7,pm"
    );
}

#[test]
fn error_is_error_accepts_it() {
    assert_eq!(
        run("throw [Error.isError(new AggregateError([])), Error.isError({})].join(',');"),
        "true,false"
    );
}

#[test]
fn to_string_falls_back_to_the_bare_name() {
    assert_eq!(
        run("throw String(new AggregateError([]));"),
        "AggregateError"
    );
}
