//! Map/Set live-storage and iterator regressions.

use v12_engine::Engine;

fn display(source: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(source).expect_err("source must throw");
    engine.to_display_string(value)
}

#[test]
fn map_delete_reinsert_appends_and_display_skips_tombstone() {
    assert_eq!(
        display(
            "let m = new Map(); m.set('a', 1); m.set('b', 2); \
             m.delete('a'); m.set('a', 3); throw m;"
        ),
        "Map(2) {b => 2, a => 3}"
    );
}

#[test]
fn set_delete_reinsert_appends_and_display_skips_tombstone() {
    assert_eq!(
        display(
            "let s = new Set(); s.add(1); s.add(2); s.delete(1); s.add(1); \
             let out = ''; for (const v of s) out += v; throw out;"
        ),
        "21"
    );
}

#[test]
fn map_iterator_skips_tombstones() {
    assert_eq!(
        display(
            "let m = new Map(); m.set('a', 1); m.set('b', 2); m.delete('a'); \
             let out = ''; for (const [k, v] of m) out += k + v; throw out;"
        ),
        "b2"
    );
}

#[test]
fn collection_for_each_observes_appends_and_skips_tombstones() {
    assert_eq!(
        display(
            "let s = new Set(); s.add(1); s.add(2); let seen = ''; \
             s.forEach(function (v) { seen += v; if (v === 1 && seen.length === 1) { s.delete(1); s.delete(2); s.add(3); s.add(1); } }); \
             throw seen;"
        ),
        "131"
    );
    assert_eq!(
        display(
            "let m = new Map(); m.set('a', 1); let seen = ''; \
             m.forEach(function (v, k) { seen += k; if (k === 'a' && seen.length === 1) { m.delete(k); m.set(k, 2); } }); \
             throw seen;"
        ),
        "aa"
    );
}

#[test]
fn set_entries_yields_canonical_array_pairs() {
    assert_eq!(
        display(
            "let s = new Set(); s.add(4); let pair = s.entries().next().value; \
             throw [pair[0], pair[1], pair instanceof Array, \
             Object.getPrototypeOf(pair) === Array.prototype].join(',');"
        ),
        "4,4,true,true"
    );
}

#[test]
fn map_and_set_use_same_value_zero_for_nan() {
    assert_eq!(
        display(
            "let m = new Map(); m.set(NaN, 7); let s = new Set(); s.add(NaN); s.add(NaN); \
             throw [m.has(NaN), m.get(NaN), s.has(NaN), s.size].join(',');"
        ),
        "true,7,true,1"
    );
}

#[test]
fn map_and_set_prototypes_and_constructors_have_real_identity() {
    assert_eq!(
        display(
            "let m = new Map(); let s = new Set(); \
             throw [m instanceof Map, Object.getPrototypeOf(m) === Map.prototype, \
             Map.prototype.constructor === Map, Object.getPrototypeOf(Map.prototype) === Object.prototype, \
             s instanceof Set, Object.getPrototypeOf(s) === Set.prototype, \
             Set.prototype.constructor === Set, Object.getPrototypeOf(Set.prototype) === Object.prototype].join(',');"
        ),
        "true,true,true,true,true,true,true,true"
    );
}

#[test]
fn collection_prototype_descriptors_match_the_ordinary_surface() {
    assert_eq!(
        display(
            "let mapSize = Object.getOwnPropertyDescriptor(Map.prototype, 'size'); \
             let setSize = Object.getOwnPropertyDescriptor(Set.prototype, 'size'); \
             let mapGet = Object.getOwnPropertyDescriptor(Map.prototype, 'get'); \
             let mapTag = Object.getOwnPropertyDescriptor(Map.prototype, Symbol.toStringTag); \
             let setTag = Object.getOwnPropertyDescriptor(Set.prototype, Symbol.toStringTag); \
             throw [typeof mapSize.get, mapSize.get.name, mapSize.get.length, mapSize.set, \
             mapSize.enumerable, mapSize.configurable, \
             typeof setSize.get, setSize.get.name, setSize.get.length, \
             mapGet.writable, mapGet.enumerable, mapGet.configurable, mapGet.value.name, mapGet.value.length, \
             mapTag.value, mapTag.writable, mapTag.enumerable, mapTag.configurable, \
             setTag.value, setTag.writable, setTag.enumerable, setTag.configurable].join(',');"
        ),
        "function,get size,0,,false,true,function,get size,0,true,false,true,get,1,Map,false,false,true,Set,false,false,true"
    );
}

#[test]
fn canonical_symbols_are_shared_by_collections_and_reflect() {
    assert_eq!(
        display(
            "let iterator = Symbol.iterator; let tag = Symbol.toStringTag; \
             throw [iterator === Symbol.iterator, \
             iterator === Object.getOwnPropertyDescriptor(Symbol, 'iterator').value, \
             tag === Symbol.toStringTag, \
             tag === Object.getOwnPropertyDescriptor(Symbol, 'toStringTag').value, \
             Map.prototype[Symbol.iterator] === Map.prototype.entries, \
             Set.prototype[Symbol.iterator] === Set.prototype.values, \
             Map.prototype[Symbol.toStringTag] === 'Map', \
             Set.prototype[Symbol.toStringTag] === 'Set', \
             Reflect[Symbol.toStringTag] === 'Reflect'].join(',');"
        ),
        "true,true,true,true,true,true,true,true,true"
    );
}

#[test]
fn canonical_symbol_properties_are_non_writable_non_enumerable_non_configurable() {
    assert_eq!(
        display(
            "let i = Object.getOwnPropertyDescriptor(Symbol, 'iterator'); \
             let t = Object.getOwnPropertyDescriptor(Symbol, 'toStringTag'); \
             throw [i.writable, i.enumerable, i.configurable, \
             t.writable, t.enumerable, t.configurable].join(',');"
        ),
        "false,false,false,false,false,false"
    );
}

#[test]
fn ordinary_overrides_and_deletions_do_not_resurrect_collection_methods() {
    assert_eq!(
        display(
            "let m = new Map(); const originalGet = Map.prototype.get; \
             Map.prototype.get = function () { return 'prototype'; }; \
             let prototypeOverride = m.get(); delete Map.prototype.get; \
             let afterPrototypeDelete = typeof m.get; \
             m.set = function () { return 'own'; }; let ownOverride = m.set(); delete m.set; \
             Map.prototype.get = originalGet; \
             throw [prototypeOverride, afterPrototypeDelete, ownOverride, \
             typeof m.get, m.get('missing')].join(',');"
        ),
        "prototype,undefined,own,function,"
    );
}

#[test]
fn collection_methods_reject_prototype_objects_without_internal_slots() {
    assert_eq!(
        display(
            "let out = ''; try { Map.prototype.clear.call(Map.prototype); } catch (e) { out += e instanceof TypeError ? 'c' : 'x'; } \
             try { Map.prototype.delete.call(Map.prototype); } catch (e) { out += e instanceof TypeError ? 'd' : 'x'; } \
             try { Set.prototype.entries.call(Set.prototype); } catch (e) { out += e instanceof TypeError ? 'e' : 'x'; } \
             try { Set.prototype.values.call(Set.prototype); } catch (e) { out += e instanceof TypeError ? 'v' : 'x'; } \
             throw out;"
        ),
        "cdev"
    );
}

#[test]
fn map_and_set_constructors_consume_iterables() {
    assert_eq!(
        display(
            "let m = new Map([['a', 1], ['b', 2]]); let s = new Set([1, 2, 2]); \
             let n = 0; let custom = { [Symbol.iterator]: function () { return { next: function () { n++; return n === 1 ? { value: ['x', 3], done: false } : { done: true }; } }; } }; \
             let cm = new Map(custom); throw [m.size, s.size, cm.get('x')].join(',');"
        ),
        "2,2,3"
    );
}

#[test]
fn constructors_use_overridden_adders_and_reject_non_callable_adders() {
    assert_eq!(
        display(
            "let calls = 0; let original = Set.prototype.add; \
             Set.prototype.add = function (v) { calls++; return original.call(this, v); }; \
             let s = new Set([4, 5]); Set.prototype.add = 1; let out = ''; \
             try { new Set([6]); } catch (e) { out = e instanceof TypeError ? 't' : 'x'; } \
             Set.prototype.add = original; throw [s.size, calls, out].join(',');"
        ),
        "2,2,t"
    );
}

#[test]
fn empty_map_and_set_construction_does_not_read_adders() {
    assert_eq!(
        display(
            "let oldSet = Map.prototype.set; let oldAdd = Set.prototype.add; \
             Map.prototype.set = 1; Set.prototype.add = 1; \
             let m0 = new Map(); let mu = new Map(undefined); let mn = new Map(null); \
             let s0 = new Set(); let su = new Set(undefined); let sn = new Set(null); \
             Map.prototype.set = oldSet; Set.prototype.add = oldAdd; \
             throw [m0.size, mu.size, mn.size, s0.size, su.size, sn.size].join(',');"
        ),
        "0,0,0,0,0,0"
    );
}

#[test]
fn map_constructor_rejects_non_entry_values_and_closes_on_failures() {
    assert_eq!(
        display(
            "let out = ''; try { new Map([1]); } catch (e) { out += e instanceof TypeError ? 'e' : 'x'; } \
             let closed = 0; let iterable = { [Symbol.iterator]: function () { return { next: function () { return { value: ['k', 1], done: false }; }, return: function () { closed++; return {}; } }; } }; \
             let old = Map.prototype.set; Map.prototype.set = function () { throw new Error('adder'); }; \
             try { new Map(iterable); } catch (e) { out += e.message; } Map.prototype.set = old; \
             let nextClosed = 0; let bad = { [Symbol.iterator]: function () { return { next: function () { throw new Error('next'); }, return: function () { nextClosed++; return {}; } }; } }; \
             try { new Map(bad); } catch (e) { out += e.message; } throw [out, closed, nextClosed].join(',');"
        ),
        "eaddernext,1,1"
    );
}

#[test]
fn constructor_iterator_closes_generators_and_accepts_callable_proxies() {
    assert_eq!(
        display(
            "let closed = false; function* values() { try { yield 1; yield 2; } finally { closed = true; } } \
             let old = Set.prototype.add; Set.prototype.add = function () { throw new Error('stop'); }; \
             try { new Set(values()); } catch (e) {} Set.prototype.add = old; \
             let proxy = new Proxy(function (v) { return Reflect.apply(old, this, arguments); }, { apply: function (target, receiver, args) { return Reflect.apply(target, receiver, args); } }); \
             Set.prototype.add = proxy; let s = new Set([9]); throw [closed, s.has(9)].join(',');"
        ),
        "true,true"
    );
}

#[test]
fn collection_size_is_an_accessor_on_the_prototype() {
    assert_eq!(
        display(
            "let m = new Map(); m.set('a', 1); m.set('b', 2); m.delete('a'); \
             let s = new Set(); s.add(1); s.add(2); s.add(1); \
             throw [m.size, s.size, m.size, s.size].join(',');"
        ),
        "1,2,1,2"
    );
}
