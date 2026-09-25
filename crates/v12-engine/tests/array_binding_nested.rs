//! Nested array binding patterns: the known-array fast path and the general
//! iterable fallback that must stay reachable for arbitrary sources.
//!
//! The standalone `v12-interp` has no `Symbol` intrinsic, so the *general*
//! iterable half of this only runs under the engine realm; the bytecode shape
//! of both halves is asserted in v12-interp's `binding_iterables` tests.

use v12_engine::Engine;

fn display_of(src: &str) -> String {
    let mut engine = Engine::new();
    let value = engine.eval(src).expect_err("source must throw");
    engine.to_display_string(value)
}

#[test]
fn nested_pattern_over_a_literal_element_uses_the_fixed_path() {
    assert_eq!(
        display_of(
            "let [a, [b, c], ...rest] = [1, [2, 3], 4, 5]; \
                    throw [a, b, c, rest.length].join(',');"
        ),
        "1,2,3,2"
    );
}

#[test]
fn deeply_nested_literal_elements_use_the_fixed_path() {
    assert_eq!(
        display_of("let [[[a]], [b]] = [[[7]], [8, 9]]; throw [a, b].join(',');"),
        "7,8"
    );
}

#[test]
fn nested_pattern_over_a_rest_array_uses_the_fixed_path() {
    // `CopyArrayRest` builds the rest array, so a pattern over it is a
    // known-array source.
    assert_eq!(
        display_of("let [a, ...[b, c]] = [1, 2, 3]; throw [a, b, c].join(',');"),
        "1,2,3"
    );
}

#[test]
fn nested_pattern_over_an_unknown_element_falls_back_to_the_iterator() {
    // Element 1 is a `new Set(...)`, not an array literal: the fact does not
    // hold, so the general iterable lowering must run.
    assert_eq!(
        display_of("let [a, [b, c]] = [1, new Set([2, 3])]; throw [a, b, c].join(',');"),
        "1,2,3"
    );
    assert_eq!(
        display_of("let [[a, b]] = [new Set([7, 8])]; throw [a, b].join(',');"),
        "7,8"
    );
}

#[test]
fn nested_pattern_over_a_default_result_falls_back_to_the_iterator() {
    // The element is `undefined`, so the default supplies the value: which of
    // the two candidates ran is a runtime fact, so the default's array-ness
    // cannot be assumed and the general iterable lowering runs.
    assert_eq!(
        display_of("let [[a, b] = [5, 6]] = [undefined]; throw [a, b].join(',');"),
        "5,6"
    );
}

#[test]
fn both_candidates_known_arrays_keep_the_fixed_path() {
    // Raw value and default are both spread-free array literals, so the
    // fact survives the default. (The bytecode shape is asserted in
    // v12-interp's `binding_iterables`.)
    assert_eq!(
        display_of("let [[a, b] = [5, 6]] = [[7, 8]]; throw [a, b].join(',');"),
        "7,8"
    );
}

#[test]
fn elision_and_holes_keep_their_positions() {
    assert_eq!(
        display_of("let [a, [b] = [4, 5]] = [1, , ]; throw [a, b].join(',');"),
        "1,4"
    );
}

#[test]
fn non_iterable_nested_source_still_throws_type_error() {
    // A number in the element position: neither path may silently succeed.
    for src in [
        "let [a, [b]] = [1, 2];",
        "let [a, [b]] = [1, , ];", // hole -> undefined -> not coercible
        "let [[[a]] = [1, 2]] = [[7, 8]];", // 7 reaches a nested pattern
    ] {
        let thrown = display_of(src);
        assert!(thrown.contains("TypeError"), "{src} -> {thrown}");
    }
}
