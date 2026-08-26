//! Locale-sensitive string comparison via `icu_collator` (compiled data).

use crate::error::IntlError;
use crate::locale::parse_locale;
use core::cmp::Ordering;
use icu::collator::Collator;
use icu::collator::options::CollatorOptions;

/// Compares two strings under the collation rules of `locale`.
///
/// Uses default collator options (tertiary strength, the ICU root default),
/// which is a reasonable approximation of ES default
/// `Intl.Collator` usage sensitivity for this skeleton wave.
pub fn compare(a: &str, b: &str, locale: &str) -> Result<Ordering, IntlError> {
    let locale = parse_locale(locale)?;
    let collator = Collator::try_new(locale.into(), CollatorOptions::default())
        .map_err(|e| IntlError::Data(e.to_string()))?;
    Ok(collator.compare(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ordering() {
        assert_eq!(compare("a", "b", "en-US").unwrap(), Ordering::Less);
        assert_eq!(compare("b", "a", "en-US").unwrap(), Ordering::Greater);
        assert_eq!(compare("a", "a", "en-US").unwrap(), Ordering::Equal);
    }

    #[test]
    fn locale_sensitive_ordering() {
        // In German phonebook-ish tradition "ä" sorts near "a"; in Swedish it
        // sorts after "z". Default-strength German treats ä as a-umlaut.
        assert_eq!(
            compare("Ärger", "Zebra", "de").unwrap(),
            Ordering::Less,
            "ä should sort before z in German"
        );
        // In Swedish, "ö" sorts after "z".
        assert_eq!(
            compare("öl", "zoo", "sv").unwrap(),
            Ordering::Greater,
            "ö should sort after z in Swedish"
        );
    }

    #[test]
    fn invalid_locale_is_an_error_not_a_crash() {
        assert!(compare("a", "b", "not a locale!").is_err());
    }
}
