//! The words this plugin shows, in the languages Chilpa offers. The English
//! text is the key; `i18n/strings.json` holds the other languages. Where
//! Chilpa's own catalog already translates a text, the plugin uses the same
//! translation.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;

pub const LANGUAGES: [&str; 7] = ["es", "de", "fr", "pt", "ru", "ja", "nl"];

static CATALOG: LazyLock<HashMap<String, HashMap<String, String>>> = LazyLock::new(|| {
    let value: Value = serde_json::from_str(include_str!("../i18n/strings.json")).expect("the catalog is valid JSON");
    value.as_object().into_iter().flatten().map(|(english, translations)| {
        let translations = translations.as_object().into_iter().flatten()
            .filter_map(|(language, text)| Some((language.clone(), text.as_str()?.to_string())))
            .collect();
        (english.clone(), translations)
    }).collect()
});

/// The two-letter language of a locale such as "es", "es-MX" or "pt_BR".
pub fn language(locale: &str) -> String {
    locale.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase()
}

/// The text in the locale's language, or the English text when it has none.
pub fn tr(locale: &str, english: &str) -> String {
    CATALOG.get(english).and_then(|translations| translations.get(&language(locale)))
        .filter(|text| !text.is_empty()).cloned().unwrap_or_else(|| english.to_string())
}

/// `tr` with `{name}` holes filled.
pub fn tr_with(locale: &str, english: &str, values: &[(&str, &str)]) -> String {
    values.iter().fold(tr(locale, english), |text, (name, value)| text.replace(&format!("{{{name}}}"), value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holes(text: &str) -> Vec<String> {
        let mut found: Vec<String> = regex::Regex::new(r"\{[a-z_]*\}").unwrap().find_iter(text).map(|m| m.as_str().to_string()).collect();
        found.sort();
        found
    }

    #[test]
    fn every_text_has_every_language_with_the_same_holes() {
        assert!(!CATALOG.is_empty());
        for (english, translations) in CATALOG.iter() {
            for language in LANGUAGES {
                let text = translations.get(language).unwrap_or_else(|| panic!("{english:?} has no {language}"));
                assert!(!text.trim().is_empty(), "{english:?} is empty in {language}");
                assert_eq!(holes(text), holes(english), "{english:?} in {language}");
            }
        }
    }

    #[test]
    fn a_locale_picks_its_language_and_english_stays_english() {
        assert_eq!(language("pt_BR"), "pt");
        assert_eq!(tr("en", "Albums"), "Albums");
        assert_eq!(tr("xx", "Albums"), "Albums");
        assert_eq!(tr("es-MX", "Not in the catalog"), "Not in the catalog");
        assert_ne!(tr("de", "Albums"), "Albums");
    }
}
