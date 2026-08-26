//! Decimal number formatting via `icu_decimal` with compiled CLDR data.
//!
//! Special values are formatted explicitly with ES-like spellings:
//! `NaN`, `Infinity`, `-Infinity`, and `-0`.

use crate::error::IntlError;
use crate::locale::parse_locale;
use icu::decimal::DecimalFormatter;
use icu::decimal::input::Decimal;
use icu::decimal::options::DecimalFormatterOptions;

/// Formats a decimal number in the given locale (e.g. grouping for
/// `en-US`: `1234567.891` → `"1,234,567.891"`).
///
/// Non-finite values map to ES-like strings: `NaN`, `Infinity`, `-Infinity`.
/// Negative zero formats as `-0`.
///
/// Note: a formatter is constructed per call; the engine layer is expected to
/// cache formatters per resolved locale once builtins exist.
pub fn format_decimal(value: f64, locale: &str) -> Result<String, IntlError> {
    if value.is_nan() {
        return Ok("NaN".to_string());
    }
    if value == f64::INFINITY {
        return Ok("Infinity".to_string());
    }
    if value == f64::NEG_INFINITY {
        return Ok("-Infinity".to_string());
    }
    if value.is_sign_negative() && value == 0.0 {
        return Ok("-0".to_string());
    }

    let locale = parse_locale(locale)?;
    let formatter = DecimalFormatter::try_new(locale.into(), DecimalFormatterOptions::default())
        .map_err(|e| IntlError::Data(e.to_string()))?;
    let decimal = finite_f64_to_decimal(value)?;
    Ok(formatter.format(&decimal).to_string())
}

/// Converts a finite `f64` into ICU4X's fixed-point [`Decimal`] using the
/// shortest digit string that round-trips.
///
/// Rust's float `Display` produces exactly that (and never uses exponent
/// notation), which is syntax `Decimal::try_from_str` accepts. The `ryu`
/// feature of `fixed_decimal` would do this directly but is not enabled by the
/// `icu` meta-crate.
pub(crate) fn finite_f64_to_decimal(value: f64) -> Result<Decimal, IntlError> {
    debug_assert!(value.is_finite(), "caller must exclude non-finite values");
    Decimal::try_from_str(&value.to_string())
        .map_err(|_| IntlError::Range(format!("{value} not representable as fixed decimal")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_en_us() {
        assert_eq!(
            format_decimal(1234567.891, "en-US").unwrap(),
            "1,234,567.891"
        );
    }

    #[test]
    fn special_floats() {
        assert_eq!(format_decimal(f64::NAN, "en-US").unwrap(), "NaN");
        assert_eq!(format_decimal(f64::INFINITY, "en-US").unwrap(), "Infinity");
        assert_eq!(
            format_decimal(f64::NEG_INFINITY, "en-US").unwrap(),
            "-Infinity"
        );
        assert_eq!(format_decimal(-0.0, "en-US").unwrap(), "-0");
        assert_eq!(format_decimal(0.0, "en-US").unwrap(), "0");
    }

    #[test]
    fn negative_and_fractional() {
        assert_eq!(format_decimal(-9876.5, "en-US").unwrap(), "-9,876.5");
    }

    #[test]
    fn invalid_locale_is_an_error_not_a_crash() {
        assert!(format_decimal(1.0, "not a locale!").is_err());
    }
}
