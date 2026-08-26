//! Locale handling: BCP-47 parsing, syntax normalization, and canonicalization.
//!
//! The default fallback locale is [`DEFAULT_LOCALE`] (`"en-US"`), matching the
//! engine's single-realm v1 behavior until host locale negotiation exists.

use crate::error::IntlError;
use icu::locale::{Locale, LocaleCanonicalizer};

/// Fallback locale used when the engine has no resolved locale.
///
/// ECMA-402 defaults to the host's locales; v1 hardcodes `en-US`.
pub const DEFAULT_LOCALE: &str = "en-US";

/// Parses a BCP-47 locale identifier with ICU4X syntactic normalization
/// (case and hyphen normalization are applied by the parser).
///
/// This is structured validation only; alias resolution is
/// [`canonicalize_locale`]. The engine surfaces parse failures as `RangeError`,
/// matching `new Intl.Locale(...)`.
pub fn parse_locale(id: &str) -> Result<Locale, IntlError> {
    id.parse::<Locale>()
        .map_err(|e| IntlError::InvalidLocale(format!("{id}: {e}")))
}

/// Canonicalizes a BCP-47 identifier per UTS #35 LDML 3 (alias/variant rules),
/// returning the canonical string.
///
/// Likely-subtag maximization/minimization is *not* applied here; ECMA-402
/// locale negotiation needs it later (documented gap in lib.rs coverage).
pub fn canonicalize_locale(id: &str) -> Result<String, IntlError> {
    let mut locale = parse_locale(id)?;
    let canonicalizer = LocaleCanonicalizer::new_common();
    let _ = canonicalizer.canonicalize(&mut locale);
    Ok(locale.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_with_case_normalization() {
        assert_eq!(parse_locale("EN-us").unwrap().to_string(), "en-US");
        assert_eq!(
            parse_locale("zh-hans-cn").unwrap().to_string(),
            "zh-Hans-CN"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_locale("not a locale!").is_err());
        assert!(parse_locale("").is_err());
    }

    #[test]
    fn canonicalizes_aliases() {
        // "iw" is the legacy code for Hebrew ("he").
        assert_eq!(canonicalize_locale("iw").unwrap(), "he");
        assert_eq!(canonicalize_locale("en-US").unwrap(), "en-US");
    }
}
