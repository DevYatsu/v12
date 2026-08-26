//! Integration tests for the `v12-regex` wrapper.
//!
//! All asserted offsets are UTF-16 code-unit indices (see crate docs).

use v12_regex::{Flags, Span, compile};

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// --- Variable-width lookbehind with captures ------------------------------

#[test]
fn variable_width_lookbehind_captures_greedily_backwards() {
    // V8/JSC agree: greedy `a*` inside the lookbehind captures "aa".
    let re = compile(
        r"(?<=(a*))b",
        Flags {
            unicode: true,
            ..Flags::default()
        },
    )
    .unwrap();
    let input = utf16("xaab");
    let m = re.exec(&input, 0).expect("should match at 'b'");
    assert_eq!(m.span(), Span { start: 3, end: 4 });
    assert_eq!(
        m.group(1),
        Some(Span { start: 1, end: 3 }),
        "lookbehind capture"
    );
}

#[test]
fn fixed_width_lookbehind_two_captures_test262_lore() {
    let re = compile(r"(?<=(a)(b))", Flags::default()).unwrap();
    let input = utf16("ab");
    let m = re.exec(&input, 0).expect("zero-width match after 'ab'");
    assert_eq!(
        m.span(),
        Span { start: 2, end: 2 },
        "lookbehind alone is zero-width"
    );
    assert_eq!(m.group(1), Some(Span { start: 0, end: 1 }));
    assert_eq!(m.group(2), Some(Span { start: 1, end: 2 }));
    assert_eq!(m.capture_count(), 2);
}

#[test]
fn lookbehind_sees_before_start_index() {
    // The reason start-index execution exists: slicing the input would break
    // lookbehind; passing a start index preserves context.
    let re = compile(r"(?<=a)b", Flags::default()).unwrap();
    let ab = utf16("ab");
    // Search starts at 0 but the 'b' found at 1 still sees its 'a'.
    assert_eq!(
        re.exec(&ab, 0).map(|m| m.span()),
        Some(Span { start: 1, end: 2 })
    );
    assert_eq!(
        re.exec(&ab, 1).map(|m| m.span()),
        Some(Span { start: 1, end: 2 }),
        "lookbehind inspects index 0 despite start=1"
    );
    assert!(
        re.exec(&utf16("xb"), 0).is_none(),
        "'b' preceded by 'x' fails the assertion"
    );
}

#[test]
fn out_of_range_start_yields_none_not_panic() {
    let re = compile(r"a", Flags::default()).unwrap();
    let input = utf16("abab");
    assert!(re.exec(&input, 4).is_none());
    assert!(re.exec(&input, 10_000).is_none());
}

// --- Backreferences --------------------------------------------------------

#[test]
fn backreference_matches_same_text() {
    let re = compile(r"(a)\1", Flags::default()).unwrap();
    assert_eq!(
        re.exec(&utf16("aa"), 0).map(|m| m.span()),
        Some(Span { start: 0, end: 2 })
    );
    assert!(re.exec(&utf16("ab"), 0).is_none());
}

#[test]
fn backreference_is_case_insensitive_under_i() {
    // JS: /(a)\1/i.test("aA") === true
    let re = compile(
        r"(a)\1",
        Flags {
            ignore_case: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert_eq!(
        re.exec(&utf16("aA"), 0).map(|m| m.span()),
        Some(Span { start: 0, end: 2 }),
        "backrefs must respect the i flag like JS"
    );
}

// --- Named groups, including duplicates ------------------------------------

#[test]
fn named_groups_year_month() {
    let re = compile(r"(?<year>\d{4})-(?<month>\d{2})", Flags::default()).unwrap();
    let input = utf16("2024-05 and more");
    let m = re.exec(&input, 0).unwrap();
    assert_eq!(m.span(), Span { start: 0, end: 7 });
    assert_eq!(m.group_by_name("year"), Some(Span { start: 0, end: 4 }));
    assert_eq!(m.group_by_name("month"), Some(Span { start: 5, end: 7 }));
    assert_eq!(m.group(1), m.group_by_name("year"));
    assert_eq!(m.group(2), m.group_by_name("month"));
    assert_eq!(m.group(3), None);
    assert_eq!(m.group_by_name("nope"), None);
}

#[test]
fn duplicate_named_groups_resolve_to_participating_alternative() {
    let re = compile(
        r"(?:(?<x>a)|(?<x>b))",
        Flags {
            unicode: true,
            ..Flags::default()
        },
    )
    .unwrap();
    let on_a = re.exec(&utf16("!a!"), 0).unwrap();
    assert_eq!(on_a.group_by_name("x"), Some(Span { start: 1, end: 2 }));
    assert_eq!(on_a.group(1), Some(Span { start: 1, end: 2 }));
    assert_eq!(on_a.group(2), None);

    let on_b = re.exec(&utf16("!b!"), 0).unwrap();
    assert_eq!(
        on_b.group_by_name("x"),
        Some(Span { start: 1, end: 2 }),
        "second branch wins here"
    );
    assert_eq!(on_b.group(1), None);
    assert_eq!(on_b.group(2), Some(Span { start: 1, end: 2 }));
}

// --- v flag: set operations -------------------------------------------------

#[test]
fn v_flag_class_subtraction() {
    let re = compile(
        r"[[a-z]--[aeiou]]",
        Flags {
            unicode_sets: true,
            ..Flags::default()
        },
    )
    .unwrap();
    for good in ["c", "z"] {
        assert!(re.exec_str(good, 0).is_some(), "{good} is a consonant");
    }
    for bad in ["e", "9", "_"] {
        assert!(re.exec_str(bad, 0).is_none(), "{bad} must not match");
    }
}

#[test]
fn v_flag_class_string_literal_q() {
    // \q{ab|c}: string-valued class alternative, v-only syntax.
    let re = compile(
        r"^[\q{ab|c}]$",
        Flags {
            unicode_sets: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert!(re.exec_str("ab", 0).is_some());
    assert!(re.exec_str("c", 0).is_some());
    assert!(re.exec_str("d", 0).is_none());
    assert!(re.exec_str("ba", 0).is_none());
}

#[test]
fn v_only_syntax_rejected_without_v() {
    // `\q{...}` (string-valued class alternative) exists only in v mode; under
    // `u` it is a SyntaxError, so regress must reject it too.
    assert!(
        compile(
            r"^[\q{ab}]$",
            Flags {
                unicode: true,
                ..Flags::default()
            }
        )
        .is_err()
    );
    // NOTE: `[[a-z]--[aeiou]]` is *not* asserted to fail without `v`: in
    // legacy class grammar that pattern is valid nested classes plus literal
    // hyphens. See the crate docs ("Known gaps").
}

// --- Case insensitivity x Unicode mode --------------------------------------

#[test]
fn unicode_case_folding_needs_i() {
    let micro = "\u{00B5}"; // MICRO SIGN
    let mu = "\u{03BC}"; // GREEK SMALL LETTER MU

    let sensitive = compile(micro, Flags::default()).unwrap();
    assert!(sensitive.exec_str(mu, 0).is_none());

    let folded = compile(
        micro,
        Flags {
            ignore_case: true,
            unicode: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert!(folded.exec_str(mu, 0).is_some(), "i+u folds µ with μ");
}

#[test]
fn ascii_case_insensitive_without_unicode() {
    let re = compile(
        r"ABC",
        Flags {
            ignore_case: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert_eq!(
        re.exec(&utf16("xabcx"), 0).map(|m| m.span()),
        Some(Span { start: 1, end: 4 })
    );
}

// --- Multiline anchors -------------------------------------------------------

#[test]
fn multiline_anchors_match_at_line_terminators() {
    let plain = compile(r"^b", Flags::default()).unwrap();
    assert!(plain.exec(&utf16("a\nb"), 0).is_none());

    let multi = compile(
        r"^b",
        Flags {
            multiline: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert_eq!(
        multi.exec(&utf16("a\nb"), 0).map(|m| m.span()),
        Some(Span { start: 2, end: 3 })
    );

    let dollar = compile(
        r"a$",
        Flags {
            multiline: true,
            ..Flags::default()
        },
    )
    .unwrap();
    assert_eq!(
        dollar.exec(&utf16("a\nb"), 0).map(|m| m.span()),
        Some(Span { start: 0, end: 1 })
    );
}

// --- UTF-16 indexing specifics ----------------------------------------------

#[test]
fn astral_character_spans_two_code_units_with_u() {
    let clef = "\u{1D11E}"; // MUSICAL SYMBOL G CLEF, U+1D11E
    let re = compile(
        clef,
        Flags {
            unicode: true,
            ..Flags::default()
        },
    )
    .unwrap();
    let units = utf16(clef); // surrogate pair: 2 code units
    assert_eq!(units.len(), 2);
    assert_eq!(
        re.exec(&units, 0).map(|m| m.span()),
        Some(Span { start: 0, end: 2 })
    );
}

#[test]
fn dot_matches_single_code_unit_without_u() {
    let clef = "\u{1D11E}";
    let dot = compile(".", Flags::default()).unwrap();
    // Without `u`, matching operates per code unit (UCS-2 semantics).
    assert_eq!(
        dot.exec(&utf16(clef), 0).map(|m| m.span()),
        Some(Span { start: 0, end: 1 })
    );
}

#[test]
fn lone_surrogate_input_does_not_panic() {
    let dot_a = compile(".a", Flags::default()).unwrap();
    // [unpaired high surrogate, 'a']
    let input: Vec<u16> = vec![0xD800, 'a' as u16];
    assert_eq!(
        dot_a.exec(&input, 0).map(|m| m.span()),
        Some(Span { start: 0, end: 2 })
    );
}

// --- str convenience path -----------------------------------------------------

#[test]
fn exec_str_matches_utf16_path_and_offsets_are_code_units() {
    let re = compile(r"(?<year>\d{4})-(?<month>\d{2})", Flags::default()).unwrap();
    let text = "x 2024-05";
    let via_str = re.exec_str(text, 0).unwrap();
    let via_u16 = re.exec(&utf16(text), 0).unwrap();
    assert_eq!(via_str.span(), via_u16.span());
    assert_eq!(
        via_str.start(),
        2,
        "offsets are UTF-16 code units even for &str input"
    );
    assert_eq!(via_str.group_by_name("year"), via_u16.group_by_name("year"));
}

// --- Caller-owned global/sticky mechanics ------------------------------------

#[test]
fn flags_recorded_but_behaviorally_inert() {
    let flags = Flags {
        global: true,
        sticky: true,
        has_indices: true,
        ..Flags::default()
    };
    let re = compile(r"ab", flags).unwrap();
    // Recorded for the caller (engine layer reads these to drive lastIndex /
    // build indices arrays); matching itself is unaffected.
    assert!(re.flags().global);
    assert!(re.flags().sticky);
    assert!(re.flags().has_indices);
    assert_eq!(re.flags().as_flag_string(), "dgy");
    assert!(re.exec(&utf16("ab"), 0).is_some());
}

#[test]
fn caller_drives_global_iteration() {
    let re = compile(
        r"ab",
        Flags {
            global: true,
            ..Flags::default()
        },
    )
    .unwrap();
    let input = utf16("ababcab");

    // This loop shape is exactly what the engine's RegExp builtins layer will
    // implement around `lastIndex`; it lives here only as a usage example.
    let mut spans = Vec::new();
    let mut last_index = 0;
    while let Some(m) = re.exec(&input, last_index) {
        spans.push(m.span());
        // Zero-width guard from the spec's RegExpBuiltinExec: advance by one
        // on empty matches so global iteration terminates.
        last_index = if m.end() == m.start() {
            m.end() + 1
        } else {
            m.end()
        };
    }
    assert_eq!(
        spans,
        vec![
            Span { start: 0, end: 2 },
            Span { start: 2, end: 4 },
            Span { start: 5, end: 7 },
        ]
    );
}

#[test]
fn caller_checks_sticky_anchor() {
    let re = compile(
        r"b",
        Flags {
            sticky: true,
            ..Flags::default()
        },
    )
    .unwrap();
    let input = utf16("ab");
    // Sticky = match must START exactly at the requested index.
    let anchored_fail = re.exec(&input, 0).filter(|m| m.start() == 0);
    assert!(anchored_fail.is_none());
    let anchored_hit = re.exec(&input, 1).filter(|m| m.start() == 1);
    assert_eq!(
        anchored_hit.map(|m| m.span()),
        Some(Span { start: 1, end: 2 })
    );
}

// --- Empty matches -------------------------------------------------------------

#[test]
fn empty_pattern_zero_width_match() {
    let re = compile(r"(?:)", Flags::default()).unwrap();
    let m = re
        .exec(&utf16(""), 0)
        .expect("empty pattern matches empty input");
    assert_eq!(m.span(), Span { start: 0, end: 0 });
    assert_eq!(m.capture_count(), 0);
}

// --- Compilation failures --------------------------------------------------------

#[test]
fn unbalanced_group_is_compile_error() {
    let err = compile(r"(unclosed", Flags::default()).expect_err("must fail");
    assert!(
        err.position.is_none(),
        "regress 0.12 reports no offsets (documented gap)"
    );
    assert!(!err.message.is_empty());
    // Display/std::error::Error plumbing works.
    assert!(err.to_string().contains("nclosed") || !err.to_string().is_empty());
}

#[test]
fn u_and_v_flags_are_mutually_exclusive() {
    let err = compile(
        "a",
        Flags {
            unicode: true,
            unicode_sets: true,
            ..Flags::default()
        },
    )
    .expect_err("ES SyntaxError equivalent");
    assert!(err.message.contains('u') && err.message.contains('v'));
}
