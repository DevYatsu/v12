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
