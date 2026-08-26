//! Plural category selection via `icu_plurals` (cardinal rules, compiled data).

use crate::decimal::finite_f64_to_decimal;
use crate::error::IntlError;
use crate::locale::parse_locale;
use icu::plurals::{PluralCategory, PluralRuleType, PluralRules, PluralRulesOptions};

/// A CLDR plural category: `zero`, `one`, `two`, `few`, `many`, or `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    /// CLDR "zero".
    Zero,
    /// CLDR "one".
    One,
    /// CLDR "two".
    Two,
    /// CLDR "few".
    Few,
    /// CLDR "many".
    Many,
    /// CLDR "other" (always present).
    Other,
}

impl Category {
    /// All six categories in canonical CLDR order.
    pub const ALL: [Category; 6] = [
        Category::Zero,
        Category::One,
        Category::Two,
        Category::Few,
        Category::Many,
        Category::Other,
    ];
}

impl From<PluralCategory> for Category {
    fn from(value: PluralCategory) -> Self {
        match value {
            PluralCategory::Zero => Category::Zero,
            PluralCategory::One => Category::One,
            PluralCategory::Two => Category::Two,
            PluralCategory::Few => Category::Few,
            PluralCategory::Many => Category::Many,
            PluralCategory::Other => Category::Other,
        }
    }
}

/// A set of plural categories (e.g. the resolved set for a locale).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Categories(u8);

impl Categories {
    /// The empty set.
    pub const EMPTY: Categories = Categories(0);

    /// A single-element set.
    pub const fn of(category: Category) -> Categories {
        Categories(1 << category_bit(category))
    }

    /// Whether `category` is in this set.
    pub const fn contains(self, category: Category) -> bool {
        self.0 & (1 << category_bit(category)) != 0
    }

    /// Inserts a category into the set.
    pub fn insert(&mut self, category: Category) {
        self.0 |= 1 << category_bit(category);
    }

    /// Iterates the members in canonical CLDR order.
    pub fn iter(self) -> impl Iterator<Item = Category> {
        let bits = self.0;
        Category::ALL
            .into_iter()
            .filter(move |c| bits & (1 << category_bit(*c)) != 0)
    }
}

const fn category_bit(category: Category) -> u8 {
    match category {
        Category::Zero => 0,
        Category::One => 1,
        Category::Two => 2,
        Category::Few => 3,
        Category::Many => 4,
        Category::Other => 5,
    }
}

/// Selects the cardinal plural category for `value` under `locale`.
///
/// Selection runs on the number's shortest-roundtrip decimal form (matching ES
/// `ResolvePlural`, where `1.0` selects `one` because it formats as `"1"`).
/// Non-finite values select [`Category::Other`], matching `Intl.PluralRules`.
pub fn plural_cardinal(value: f64, locale: &str) -> Result<Category, IntlError> {
    let locale = parse_locale(locale)?;
    let rules = cardinal_rules(&locale)?;
    if !value.is_finite() {
        return Ok(Category::Other);
    }
    let decimal = finite_f64_to_decimal(value)?;
    Ok(rules.category_for(&decimal).into())
}

/// Returns the set of plural categories with cardinal rules in `locale`
/// (the future source for `Intl.PluralRules.prototype.resolvedOptions`).
pub fn cardinal_categories(locale: &str) -> Result<Categories, IntlError> {
    let locale = parse_locale(locale)?;
    let rules = cardinal_rules(&locale)?;
    let mut set = Categories::EMPTY;
    for category in rules.categories() {
        set.insert(category.into());
    }
    Ok(set)
}

fn cardinal_rules(locale: &icu::locale::Locale) -> Result<PluralRules, IntlError> {
    let options = PluralRulesOptions::default().with_type(PluralRuleType::Cardinal);
    PluralRules::try_new(locale.clone().into(), options).map_err(|e| IntlError::Data(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_us_one_other() {
        assert_eq!(plural_cardinal(1.0, "en-US").unwrap(), Category::One);
        assert_eq!(plural_cardinal(2.0, "en-US").unwrap(), Category::Other);
        assert_eq!(plural_cardinal(0.0, "en-US").unwrap(), Category::Other);
    }

    #[test]
    fn fraction_selects_other_in_en_us() {
        assert_eq!(plural_cardinal(1.5, "en-US").unwrap(), Category::Other);
        // 1.0 formats as "1", so it still selects one (ES ResolvePlural parity).
        assert_eq!(plural_cardinal(1.0, "en-US").unwrap(), Category::One);
    }

    #[test]
    fn non_finite_selects_other() {
        assert_eq!(plural_cardinal(f64::NAN, "en-US").unwrap(), Category::Other);
        assert_eq!(
            plural_cardinal(f64::INFINITY, "en-US").unwrap(),
            Category::Other
        );
    }

    #[test]
    fn locale_with_more_categories() {
        // Russian cardinals use one/few/many/other.
        let set = cardinal_categories("ru").unwrap();
        assert!(set.contains(Category::One));
        assert!(set.contains(Category::Few));
        assert!(set.contains(Category::Many));
        assert!(set.contains(Category::Other));
        assert!(!set.contains(Category::Zero));
        assert_eq!(plural_cardinal(21.0, "ru").unwrap(), Category::One);
        assert_eq!(plural_cardinal(3.0, "ru").unwrap(), Category::Few);
        assert_eq!(plural_cardinal(5.0, "ru").unwrap(), Category::Many);
    }

    #[test]
    fn invalid_locale_is_an_error_not_a_crash() {
        assert!(plural_cardinal(1.0, "not a locale!").is_err());
    }
}
