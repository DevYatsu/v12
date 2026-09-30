//! Promise prototype/static surface: the ordinary installed properties must
//! be the ones script observes (`Object.getOwnPropertyNames`,
//! descriptors, `Symbol.species`/`Symbol.toStringTag`), and must win over the
//! interpreter's synthesized promise reads — including when they are deleted.

use v12_engine::Engine;

/// Evaluates `source` and returns the thrown value's display string.
fn display(source: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(source).expect_err("source must throw");
    engine.to_display_string(value)
}

#[test]
fn promise_constructor_and_prototype_own_property_names() {
    assert_eq!(
        display(
            "throw [Object.getOwnPropertyNames(Promise).sort().join(','), \
             Object.getOwnPropertyNames(Promise.prototype).sort().join(',')].join(' | ');"
        ),
        // `allKeyed` / `allSettledKeyed` are part of the surface (27.2.4) and are
        // installed by `install_keyed_promise_combinators`.
        "all,allKeyed,allSettled,allSettledKeyed,any,length,name,prototype,race,reject,resolve,\
         try,withResolvers \
         | catch,constructor,finally,then"
    );
}

#[test]
fn promise_own_property_symbols_are_species_and_to_string_tag() {
    assert_eq!(
        display(
            "let ctor = Object.getOwnPropertySymbols(Promise); \
             let proto = Object.getOwnPropertySymbols(Promise.prototype); \
             throw [ctor.length, ctor[0] === Symbol.species, \
             proto.length, proto[0] === Symbol.toStringTag].join(',');"
        ),
        "1,true,1,true"
    );
}

#[test]
fn promise_statics_and_methods_are_callable_with_basic_results() {
    assert_eq!(
        display(
            "throw [typeof Promise, typeof Promise.resolve, typeof Promise.reject, \
             typeof Promise.all, typeof Promise.race, \
             typeof Promise.prototype.then, typeof Promise.prototype.catch, \
             typeof Promise.prototype.finally, \
             Promise.resolve(5) instanceof Promise, \
             Promise.reject(5) instanceof Promise, \
             Promise.resolve(5).then(function () {}) instanceof Promise, \
             Promise.resolve(5).catch(function () {}) instanceof Promise, \
             Promise.resolve(5).finally(function () {}) instanceof Promise].join(',');"
        ),
        "function,function,function,function,function,function,function,function,true,true,true,true,true"
    );
}

#[test]
fn promise_surface_arity_and_names_match_the_spec() {
    assert_eq!(
        display(
            "throw [Promise.length, Promise.name, \
             Promise.resolve.length, Promise.resolve.name, \
             Promise.reject.length, Promise.reject.name, \
             Promise.prototype.then.length, Promise.prototype.then.name, \
             Promise.prototype.catch.length, Promise.prototype.catch.name, \
             Promise.prototype.finally.length, Promise.prototype.finally.name].join(',');"
        ),
        "1,Promise,1,resolve,1,reject,2,then,1,catch,1,finally"
    );
}

#[test]
fn promise_prototype_has_object_prototype_and_constructor_identity() {
    assert_eq!(
        display(
            "throw [Promise.prototype.constructor === Promise, \
             Object.getPrototypeOf(Promise.prototype) === Object.prototype].join(',');"
        ),
        "true,true"
    );
}

#[test]
fn species_and_to_string_tag_are_canonical_per_realm() {
    // `Promise[Symbol.species] === Promise` now holds through the getter, which
    // returns its receiver — so the descriptor is an *accessor*: there is no
    // `value` to read. (`Promise.prototype[@@toStringTag]` is still the data
    // property it has always been.)
    assert_eq!(
        display(
            "let tag = Symbol.toStringTag; \
             throw [Promise[Symbol.species] === Promise, \
             Promise.prototype[tag] === 'Promise', \
             Promise.prototype[Symbol.toStringTag] === 'Promise', \
             typeof Object.getOwnPropertyDescriptor(Promise, Symbol.species).get, \
             Object.getOwnPropertyDescriptor(Promise, Symbol.species).set === undefined, \
             Object.getOwnPropertyDescriptor(Promise, Symbol.species).value === undefined, \
             Object.getOwnPropertyDescriptor(Promise.prototype, tag).value === 'Promise'].join(',');"
        ),
        "true,true,true,function,true,true,true"
    );
}

#[test]
fn promise_surface_descriptors_match_the_ordinary_builtin_shape() {
    // The `@@species` slot is the one accessor here: `get` is a function,
    // `set` is `undefined`, and — because an accessor descriptor has no
    // `writable` field at all — the third field prints empty.
    assert_eq!(
        display(
            "let data = function (d) { return [typeof d.get, typeof d.set, \
             d.writable, d.enumerable, d.configurable].join('/'); }; \
             throw [data(Object.getOwnPropertyDescriptor(Promise, 'prototype')), \
             data(Object.getOwnPropertyDescriptor(Promise.prototype, 'then')), \
             data(Object.getOwnPropertyDescriptor(Promise.prototype, 'catch')), \
             data(Object.getOwnPropertyDescriptor(Promise.prototype, 'finally')), \
             data(Object.getOwnPropertyDescriptor(Promise.prototype, 'constructor')), \
             data(Object.getOwnPropertyDescriptor(Promise, 'resolve')), \
             data(Object.getOwnPropertyDescriptor(Promise, 'reject')), \
             data(Object.getOwnPropertyDescriptor(Promise, 'all')), \
             data(Object.getOwnPropertyDescriptor(Promise, 'race')), \
              data(Object.getOwnPropertyDescriptor(Promise, Symbol.species)), \
              data(Object.getOwnPropertyDescriptor(Promise.prototype, Symbol.toStringTag)), \
              Object.keys(Promise.prototype).length, \
              Object.keys(Promise).length].join(' ');"
        ),
        "undefined/undefined/false/false/false \
         undefined/undefined/true/false/true \
         undefined/undefined/true/false/true \
         undefined/undefined/true/false/true \
         undefined/undefined/true/false/true \
         undefined/undefined/true/false/true \
         undefined/undefined/true/false/true \
         undefined/undefined/true/false/true \
         undefined/undefined/true/false/true \
         function/undefined//false/true \
         undefined/undefined/false/false/true \
         0 0"
    );
}

#[test]
fn installed_then_and_catch_drive_reaction_scheduling() {
    let mut engine = Engine::new();
    let src = "globalThis.__log = []; \
               Promise.resolve(1).then(function (v) { globalThis.__log.push('f' + v); }) \
                                .catch(function () { globalThis.__log.push('r'); }); \
               Promise.reject('boom').then(function () { globalThis.__log.push('never'); }) \
                                   .catch(function (e) { globalThis.__log.push('c' + e); });";
    engine.eval(src).expect("eval");
    engine.run_jobs();
    let value = engine.eval("globalThis.__log.join(',')").expect("read");
    assert_eq!(engine.to_display_string(value), "f1,cboom");
}

#[test]
fn ordinary_promise_statics_override_and_deletion_win() {
    assert_eq!(
        display(
            "let original = Promise.resolve; let seen = ''; \
             Promise.resolve = function () { return 'static'; }; \
             seen += Promise.resolve(1); \
             delete Promise.resolve; seen += ':' + typeof Promise.resolve; \
             Promise.resolve = original; \
             seen += ':' + (Promise.resolve(1) instanceof Promise); \
             let rejectOriginal = Promise.reject; \
             Promise.reject = function () { return 'rejected'; }; \
             seen += ':' + Promise.reject(2); \
             delete Promise.reject; seen += ':' + typeof Promise.reject; \
             Promise.reject = rejectOriginal; throw seen;"
        ),
        "static:undefined:true:rejected:undefined"
    );
}

#[test]
fn ordinary_promise_prototype_methods_override_and_deletion_win() {
    assert_eq!(
        display(
            "let originalThen = Promise.prototype.then; let seen = ''; \
             Promise.prototype.then = function () { return 'proto'; }; \
             seen += Promise.resolve(1).then(); \
             delete Promise.prototype.then; seen += ':' + typeof Promise.resolve(1).then; \
             Promise.prototype.then = originalThen; \
             seen += ':' + (typeof Promise.resolve(1).then); \
             let originalCatch = Promise.prototype.catch; \
             Promise.prototype.catch = function () { return 'catch'; }; \
             seen += ':' + Promise.reject(1).catch(); \
             delete Promise.prototype.catch; seen += ':' + typeof Promise.reject(1).catch; \
             Promise.prototype.catch = originalCatch; throw seen;"
        ),
        "proto:undefined:function:catch:undefined"
    );
}

#[test]
fn installed_then_rejects_non_promise_receivers() {
    assert_eq!(
        display(
            "let out = ''; \
             try { Promise.prototype.then.call({}); } catch (e) { out += e instanceof TypeError ? 't' : 'x'; } \
             try { Promise.prototype.catch.call({}, function () {}); } catch (e) { out += e instanceof TypeError ? 'c' : 'x'; } \
             try { Promise.prototype.finally.call({}); } catch (e) { out += e instanceof TypeError ? 'f' : 'x'; } \
             try { Promise.prototype.then.call(Promise.prototype); } catch (e) { out += e instanceof TypeError ? 'p' : 'x'; } \
             throw out;"
        ),
        "tcfp"
    );
}
