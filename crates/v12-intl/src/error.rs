//! Error type shared by the v12-intl surface.
//!
//! The engine layer (v12-engine) maps these onto JS exceptions:
//! [`IntlError::InvalidLocale`] and [`IntlError::Range`] become `RangeError`,
//! [`IntlError::Data`] is an internal error that should be unreachable with
//! compiled data for supported locales.

use core::fmt;

/// An error from an Intl or Temporal-support operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntlError {
    /// Malformed BCP-47 locale identifier. Corresponds to the structured
    /// validation failure in ECMA-402 `Intl.Locale` (a `RangeError`).
    InvalidLocale(String),
    /// Value outside the representable range of the corresponding ES operation
    /// (e.g. a date outside the Temporal representable window).
    Range(String),
    /// ICU4X data or construction failure. With `compiled_data`, this should
    /// only occur if CLDR lacks data for the requested operation.
    Data(String),
}

impl fmt::Display for IntlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLocale(id) => write!(f, "invalid locale identifier {id:?}"),
            Self::Range(msg) => write!(f, "value out of range: {msg}"),
            Self::Data(msg) => write!(f, "ICU data error: {msg}"),
        }
    }
}

impl std::error::Error for IntlError {}
