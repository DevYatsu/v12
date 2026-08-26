#![forbid(unsafe_code)]

//! ES-semantics-facing wrapper over the [`regress`] regex engine.
//!
//! # All offsets are UTF-16 code units
//!
//! **Every offset this crate produces or accepts — [`Match::start`],
//! [`Match::end`], every [`Span`], and `start_index_utf16` — is an index in
//! UTF-16 code units into the input**, exactly like JavaScript string
//! indices. This holds for both input paths:
//!
//! - Primary: `&[u16]` slices via [`CompiledRegex::exec`]. Offsets are
//!   code units by construction; lone surrogates are legal elements.
//! - Convenience: `&str` via [`CompiledRegex::exec_str`]. The string is
//!   converted to UTF-16 internally (one allocation per call), so offsets are
//!   still code units, never byte offsets. Prefer the UTF-16 path on hot paths.
//!
//! A regex matching one astral-plane character under the `u` flag therefore
//! spans 2 offset units; without `u`, matching operates per code unit,
//! mirroring JS's UCS-2 vs Unicode mode split.
//!
//! # Layering
//!
//! This crate is a *matching* layer only. The engine's RegExp built-in
//! implements the object semantics on top: `lastIndex` handling, `global`/`sticky` advancement and
//! zero-width guards, `has_indices` result shaping, species, `exec` side
//! effects. None of that lives here. In particular:
//!
//! - The caller drives iteration: call [`CompiledRegex::exec`] with a start
//!   index, then advance it yourself (for global: to `match.end()`, or
//!   `start + 1` on empty matches).
//! - Sticky is not anchored here either: call `exec(input, last_index)` and
//!   check `match.start() == last_index`.
//!
//! # Flag coverage
//!
//! [`Flags`] mirrors the eight ES flags. Mapping onto regress 0.12:
//!
//! | ES flag | regress | Effect |
//! |---|---|---|
//! | `i` ignore_case | `icase` | full Unicode case folding |
//! | `m` multiline   | `multiline` | `^`/`$` match at line terminators |
//! | `s` dot_all     | `dot_all` | `.` matches line terminators |
//! | `u` unicode     | `unicode` | code-point mode, strict syntax |
//! | `v` unicode_sets| `unicode_sets` | as `u` plus set operations |
//! | `g` global      | *(none)* | recorded only; no match effect (correct) |
//! | `y` sticky      | *(none)* | recorded only; no match effect (correct) |
//! | `d` has_indices | *(none)* | recorded only; indices are derived by the caller from [`Match`] |
//!
//! `u` and `v` together are rejected at compile time with a
//! [`CompileError`] (a SyntaxError in ES). regress silently ignores unknown
//! flags, but since mapping happens here, no flag can be silently dropped.
//!
//! # Known gaps (regress-side, documented not fixed)
//!
//! - regress's parser does not implement Annex B legacy-syntax quirks fully
//!   (e.g. legacy octal escapes); patterns relying on them may compile or
//!   behave differently than a spec-exact engine.
//! - Outside `u`/`v` mode, regress accepts some class-set syntax that a
//!   spec-exact engine would read as legacy nested classes with literal
//!   hyphens (e.g. `[[a-z]--[aeiou]]`) — it compiles here, but the v-flag
//!   set-operation *meaning* only applies under `unicode_sets`.
//! - regress 0.12's parse error type carries a message but *no source
//!   offset*, so [`CompileError::position`] is always `None` for now; the
//!   field exists so callers don't break when regress restores positions.
//! - No Unicode normalization is performed (matches JS; normalize first).
//! - At most 65,535 capture groups (regress limit).

#![deny(missing_docs)]

use std::fmt;
use std::sync::Arc;

/// Half-open span `[start, end)` of **UTF-16 code-unit** offsets into an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    /// Inclusive start offset (code units).
    pub start: usize,
    /// Exclusive end offset (code units).
    pub end: usize,
}

impl Span {
    /// Length of the span in code units.
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// True if the span matched the empty string.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl From<std::ops::Range<usize>> for Span {
    #[inline]
    fn from(r: std::ops::Range<usize>) -> Self {
        Span {
            start: r.start,
            end: r.end,
        }
    }
}

/// The eight ECMAScript regular-expression flags.
///
/// Matching-affecting flags are forwarded to regress; `global`, `sticky` and
/// `has_indices` have no effect on matching and are carried for the caller
/// (the engine's RegExp builtins layer owns their mechanics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    /// `g`: global. Recorded only; iteration/advance logic is the caller's job.
    pub global: bool,
    /// `i`: case-insensitive matching (Unicode-aware folding).
    pub ignore_case: bool,
    /// `m`: `^` and `$` additionally match at line terminators.
    pub multiline: bool,
    /// `s`: `.` also matches line terminators.
    pub dot_all: bool,
    /// `u`: Unicode mode (code-point semantics, strict syntax).
    pub unicode: bool,
    /// `v`: UnicodeSets mode (as `u` plus set operations in classes).
    pub unicode_sets: bool,
    /// `d`: has_indices. Recorded only; the caller derives index arrays
    /// from the returned [`Match`].
    pub has_indices: bool,
    /// `y`: sticky. Recorded only; anchoring is the caller's job (compare
    /// [`Match::start`] against the requested start index).
    pub sticky: bool,
}

impl Flags {
    /// Canonical ES flag-string form, `"dgimsuvy"` order (the order used by
    /// `RegExp.prototype.flags`). Flags that are unset are omitted.
    #[must_use]
    pub fn as_flag_string(&self) -> String {
        let mut s = String::with_capacity(8);
        if self.has_indices {
            s.push('d');
        }
        if self.global {
            s.push('g');
        }
        if self.ignore_case {
            s.push('i');
        }
        if self.multiline {
            s.push('m');
        }
        if self.dot_all {
            s.push('s');
        }
        if self.unicode {
            s.push('u');
        }
        if self.unicode_sets {
            s.push('v');
        }
        if self.sticky {
            s.push('y');
        }
        s
    }

    fn to_regress(self) -> regress::Flags {
        regress::Flags {
            icase: self.ignore_case,
            multiline: self.multiline,
            dot_all: self.dot_all,
            unicode: self.unicode,
            unicode_sets: self.unicode_sets,
            // Keep regress's optimizer on; `no_opt` is a debugging knob we
            // deliberately do not surface.
            no_opt: false,
        }
    }
}

/// Error from [`compile`].
///
/// Wraps the regress parser error message. Note regress 0.12 does not report
/// the failure offset in its error type, so [`position`](CompileError::position)
/// is currently always `None`.
#[derive(Debug, Clone)]
pub struct CompileError {
    /// Human-readable description of the syntax problem.
    pub message: String,
    /// Reserved for a future regress that reports error offsets; always
    /// `None` with regress 0.12.
    pub position: Option<usize>,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompileError {}

#[derive(Debug)]
struct Inner {
    regex: regress::Regex,
    source: Box<str>,
}

/// A compiled pattern, ready to be executed against UTF-16 inputs.
///
/// Cheaply clonable (internals behind an [`Arc`]). Caching compiled regexes
/// across executions of user code is the *caller's* responsibility; this
/// crate does no caching.
#[derive(Debug, Clone)]
pub struct CompiledRegex {
    inner: Arc<Inner>,
    flags: Flags,
}

/// Compile `pattern` (given as Rust text) with `flags`.
pub fn compile(pattern: &str, flags: Flags) -> Result<CompiledRegex, CompileError> {
    if flags.unicode && flags.unicode_sets {
        return Err(CompileError {
            // Spec: it is a SyntaxError for a RegularExpressionFlags to
            // contain both `u` and `v`.
            message: "the 'u' and 'v' flags are mutually exclusive".to_owned(),
            position: None,
        });
    }
    let regex =
        regress::Regex::with_flags(pattern, flags.to_regress()).map_err(|e| CompileError {
            message: e.text,
            position: None,
        })?;
    Ok(CompiledRegex {
        inner: Arc::new(Inner {
            regex,
            source: Box::from(pattern),
        }),
        flags,
    })
}

impl CompiledRegex {
    /// The flags this regex was compiled with (including the recorded-only
    /// `global`/`sticky`/`has_indices` bits).
    #[inline]
    #[must_use]
    pub fn flags(&self) -> &Flags {
        &self.flags
    }

    /// The original pattern text.
    #[inline]
    #[must_use]
    pub fn source(&self) -> &str {
        &self.inner.source
    }

    /// Execute against a UTF-16 input, searching from `start_index_utf16`
    /// (a **code-unit** offset) towards the end of the input, returning the
    /// first match.
    ///
    /// The input indexer is chosen to match JS string semantics for the
    /// pattern's mode: with `u`/`v`, matching steps by Unicode code point
    /// (an astral character is one element); without them, matching steps
    /// per code unit (UCS-2 semantics), so `.` consumes one surrogate half.
    /// Offsets are code units either way.
    ///
    /// Lookbehind may inspect input before `start_index_utf16`; this is why
    /// the start index is a parameter rather than the caller slicing input
    /// (mirrors regress's `find_from` / JS `lastIndex` mechanics).
    ///
    /// A `start_index_utf16` beyond the input length yields `None` (no panic),
    /// mirroring JS where `lastIndex > length` fails the match.
    ///
    /// There is no anchoring here: sticky semantics require the caller to
    /// check `match.start() == start_index_utf16`.
    pub fn exec(&self, input: &[u16], start_index_utf16: usize) -> Option<Match> {
        // regress's `find_from_utf16` always steps by code point regardless of
        // flags; per-code-unit stepping lives in its UCS-2 path, so the JS
        // u/non-u input-mode split is made here.
        let m = if self.flags.unicode || self.flags.unicode_sets {
            self.inner
                .regex
                .find_from_utf16(input, start_index_utf16)
                .next()
        } else {
            self.inner
                .regex
                .find_from_ucs2(input, start_index_utf16)
                .next()
        }?;
        Some(Match::from_regress(m))
    }

    /// Convenience overload of [`exec`](CompiledRegex::exec) for Rust strings.
    ///
    /// The input is converted to UTF-16 (one allocation per call) so all
    /// offsets — including `start_index_utf16` — remain code-unit based,
    /// identical to the UTF-16 path. Use [`exec`](CompiledRegex::exec) to
    /// avoid the conversion cost.
    pub fn exec_str(&self, input: &str, start_index_utf16: usize) -> Option<Match> {
        let mut units = Vec::with_capacity(input.len());
        units.extend(input.encode_utf16());
        self.exec(&units, start_index_utf16)
    }
}

/// One successful match: overall span plus capture-group spans.
///
/// All offsets are **UTF-16 code units** into the original input (see crate
/// root docs). A group that did not participate in the match (e.g. it sat in
/// an untaken alternation branch) yields `None`.
#[derive(Debug, Clone)]
pub struct Match {
    start: usize,
    end: usize,
    /// Capture group *i* (1-based) is stored at index `i - 1`.
    groups: Vec<Option<Span>>,
    /// Named groups whose capture participated, deduplicated by name (for
    /// duplicate named groups, the participating alternative wins).
    named: Box<[(Box<str>, Span)]>,
}

impl Match {
    fn from_regress(m: regress::Match) -> Match {
        let groups = m
            .captures
            .iter()
            .map(|r| {
                r.as_ref().map(|r| Span {
                    start: r.start,
                    end: r.end,
                })
            })
            .collect();
        let named: Vec<(Box<str>, Span)> = m
            .named_groups()
            .filter_map(|(name, range)| {
                range.map(|r| {
                    (
                        Box::from(name),
                        Span {
                            start: r.start,
                            end: r.end,
                        },
                    )
                })
            })
            .collect();
        Match {
            start: m.range.start,
            end: m.range.end,
            groups,
            named: named.into(),
        }
    }

    /// Start of the whole match (inclusive, code units).
    #[inline]
    #[must_use]
    pub fn start(&self) -> usize {
        self.start
    }

    /// End of the whole match (exclusive, code units).
    #[inline]
    #[must_use]
    pub fn end(&self) -> usize {
        self.end
    }

    /// Span of the whole match.
    #[inline]
    #[must_use]
    pub fn span(&self) -> Span {
        Span {
            start: self.start,
            end: self.end,
        }
    }

    /// Number of capture groups declared in the pattern (excluding the whole
    /// match). Unnamed groups count too.
    #[inline]
    #[must_use]
    pub fn capture_count(&self) -> usize {
        self.groups.len()
    }

    /// Indexed group access, JS-style: index `0` is the whole match, index
    /// `1..=capture_count()` are the capture groups in declaration order.
    /// Returns `None` for out-of-range indexes or non-participating groups.
    #[inline]
    #[must_use]
    pub fn group(&self, index: usize) -> Option<Span> {
        if index == 0 {
            Some(self.span())
        } else {
            self.groups.get(index - 1).copied().flatten()
        }
    }

    /// Named group access. With duplicate named groups, returns the span of
    /// the participating alternative (regress resolves duplicates the same
    /// way when producing matches).
    #[inline]
    #[must_use]
    pub fn group_by_name(&self, name: &str) -> Option<Span> {
        self.named
            .iter()
            .find(|(n, _)| n.as_ref() == name)
            .map(|(_, span)| *span)
    }
}
