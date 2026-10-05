//! `String.prototype.indexOf` / `includes` / `lastIndexOf` must return an index
//! into the WHOLE string, not into the suffix searched from `fromIndex`.
//!
//! `utf16_find` (builtins/string.rs) searched with
//! `windows(needle.len()).skip(from).position(..)`. After `skip(from)` the
//! first remaining window already starts AT `from`, so `position` is relative
//! to that point rather than to the haystack. Every call with a non-zero
//! `fromIndex` therefore returned an index that was too small by exactly
//! `from`:
//!
//! ```text
//! "abcdef".indexOf("f", 4)   ->  1   (node: 5)
//! "abcabc".indexOf("bc", 3)  ->  1   (node: 4)
//! ```
//!
//! The bug was invisible to any test that only ever passed `fromIndex` 0, which
//! is why it survived: a substring search that starts at the beginning is
//! correct by accident.
//!
//! This was found as the residue of the W3.2 slice-2 coercion probe, where it
//! was the entire remaining delta after the coercion work landed. That is the
//! argument for comparing against `node` rather than only before/after: the
//! before/after diff for this site was empty, because the coercion change
//! neither caused nor fixed it.
//!
//! Values are compared as scalars. v12's `console.log` renders arrays
//! comma-separated rather than as `[ 1, 2 ]`, so an array-valued comparison
//! against node produces false divergences.
use v12_engine::Engine;

fn eval_display(src: &str) -> String {
    let mut engine = Engine::new();
    match engine.eval(src) {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

/// The regression: a non-zero `fromIndex` must offset the result.
#[test]
fn index_of_with_a_non_zero_from_index_returns_an_absolute_index() {
    assert_eq!(eval_display("'abcdef'.indexOf('f', 4)"), "5");
    assert_eq!(eval_display("'abcabc'.indexOf('bc', 3)"), "4");
}

/// A match *before* `fromIndex` must still not be found.
#[test]
fn a_needle_entirely_before_from_index_is_not_found() {
    assert_eq!(eval_display("'abcdef'.indexOf('a', 1)"), "-1");
    assert_eq!(eval_display("'abc'.indexOf('ab', 1)"), "-1");
}

/// `fromIndex` exactly at a match, and exactly at the end, must both work.
#[test]
fn from_index_on_and_past_the_needle_behaves() {
    assert_eq!(eval_display("'abcdef'.indexOf('f', 5)"), "5");
    assert_eq!(eval_display("'abcdef'.indexOf('f', 6)"), "-1");
}

/// `fromIndex` past the end, and the empty-needle case, are unchanged.
#[test]
fn from_index_past_the_end_and_the_empty_needle_are_unchanged() {
    assert_eq!(eval_display("'abc'.indexOf('', 2)"), "2");
    assert_eq!(eval_display("'abc'.indexOf('a', 1)"), "-1");
    assert_eq!(eval_display("'abc'.indexOf('', 99)"), "3");
}

/// `includes` shares the search helper, so it inherits the same offset.
#[test]
fn includes_with_a_non_zero_from_index_is_correct() {
    assert_eq!(eval_display("'abcdef'.includes('f', 4)"), "true");
    assert_eq!(eval_display("'abcdef'.includes('a', 1)"), "false");
}
