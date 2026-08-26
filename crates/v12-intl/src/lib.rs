#![forbid(unsafe_code)]

//! Intl (ECMA-402) and Temporal glue over ICU4X (`icu` meta-crate 2.x) and
//! `temporal_rs`, consumed by `v12-engine`'s `Intl.*` and Temporal builtins.
//!
//! Design constraints: ICU4X runs on **compiled CLDR data**
//! (baked into the binary; no data-provider plumbing), and all Temporal
//! semantics are delegated to `temporal_rs` — v12 owns no calendar math.
//!
//! # ECMA-402 coverage status
//!
//! This is a skeleton wave: the surface below is honest about what exists.
//!
//! **Covered:**
//! - Locale handling ([`locale`]) — BCP-47 parsing/syntax normalization and
//!   UTS #35 canonicalization; hardcoded fallback locale
//!   ([`locale::DEFAULT_LOCALE`] = `"en-US"`).
//! - Decimal number formatting ([`decimal::format_decimal`]) — grouping,
//!   separators, `-u-nu` numbering systems, ES-like special values
//!   (`NaN`, `Infinity`, `-Infinity`, `-0`).
//! - Collation ([`collator::compare`]) — default-strength locale-sensitive
//!   ordering.
//! - Plural selection ([`plural::plural_cardinal`],
//!   [`plural::cardinal_categories`]) — cardinal rules, all six categories,
//!   plus the resolved category set.
//! - ISO date core ([`temporal::IsoDate`], [`temporal::IsoTime`],
//!   [`temporal::IsoDateTime`]) — validated construction, ±day arithmetic,
//!   spec-formatted ISO strings.
//!
//! **Not covered** (engine surfaces these as TODO builtins):
//! - `Intl.DateTimeFormat` patterns/formatting and date-time skeletons.
//! - `Intl.RelativeTimeFormat`, `Intl.ListFormat`, `Intl.Segmenter`,
//!   `Intl.DisplayNames`.
//! - Resolved-options objects and locale negotiation algorithms
//!   (`lookupSupportedLocales`, bestAvailableLocale et al.); likely-subtag
//!   maximize/minimize is not exposed yet either.
//! - Ordinal plural selection (only cardinals are wired).
//! - Calendar systems beyond ISO, time zones, and `ZonedDateTime`.
//! - Formatter caching: every call constructs its ICU formatter; caching per
//!   resolved locale belongs in a later wave once builtins pin down lifetimes.
//!
//! Error convention: [`IntlError`] values map to JS exceptions in
//! `v12-engine` (`InvalidLocale`/`Range` → `RangeError`; `Data` should be
//! unreachable with compiled data).

pub mod collator;
pub mod decimal;
mod error;
pub mod locale;
pub mod plural;
pub mod temporal;

pub use error::IntlError;
