//! Localized strings for the 29 LuaTools locales.
//!
//! Keys are typed ([`Key`]); the English text is baked into the enum so it doubles as the
//! fallback and as test data. Per-locale tables are generated from `locales/*.resx` by
//! `build.rs`, which emits a sorted `(resource_name, text)` slice for each locale.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod keys;

pub use keys::Key;

use luatools_core::Locale;

/// The tables emitted by `build.rs` into `$OUT_DIR/tables.rs`.
mod tables {
    include!(concat!(env!("OUT_DIR"), "/tables.rs"));
}

/// A locale-specific view over the translation tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Translator {
    locale: Locale,
}

impl Default for Translator {
    fn default() -> Self {
        Self::new(Locale::default())
    }
}

impl Translator {
    /// Creates a translator for `locale`.
    #[must_use]
    pub const fn new(locale: Locale) -> Self {
        Self { locale }
    }

    /// The active locale.
    #[must_use]
    pub const fn locale(self) -> Locale {
        self.locale
    }

    /// Switches the locale in place.
    pub fn set_locale(&mut self, locale: Locale) {
        self.locale = locale;
    }

    /// Returns the translation for `key`, falling back to English.
    #[must_use]
    pub fn t(self, key: Key) -> &'static str {
        lookup(table(self.locale), key)
    }

    /// Replaces `{0}`, `{1}`, ... with the supplied arguments.
    ///
    /// Unknown placeholders are left untouched so a missing argument is visible in the UI
    /// instead of silently truncating the sentence.
    #[must_use]
    pub fn format(self, key: Key, args: &[&str]) -> String {
        format_template(self.t(key), args)
    }
}

/// Binary-searches a sorted locale table for `key`, falling back to the English text.
///
/// Taking the table as an argument keeps the fallback path testable without a locale whose
/// resource file is incomplete.
fn lookup(table: &'static [(&'static str, &'static str)], key: Key) -> &'static str {
    if let Ok(index) = table.binary_search_by_key(&key.resource_name(), |(name, _)| *name)
        && let Some((_, text)) = table.get(index)
    {
        return text;
    }
    key.english()
}

/// Substitutes positional placeholders in `template`.
fn format_template(template: &str, args: &[&str]) -> String {
    if args.is_empty() {
        return template.to_owned();
    }
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        let Some(close) = tail.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let index = tail[..close].parse::<usize>().ok();
        match index.and_then(|i| args.get(i)) {
            Some(value) => {
                out.push_str(value);
                rest = &tail[close + 1..];
            }
            None => {
                out.push('{');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The sorted translation table for a locale.
///
/// English is served from the key enum itself and needs no table.
fn table(locale: Locale) -> &'static [(&'static str, &'static str)] {
    tables::table(locale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Placeholder indices used in `text`, e.g. `{0}` and `{12}`.
    fn placeholders(text: &str) -> BTreeSet<&str> {
        let mut found = BTreeSet::new();
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            let tail = &rest[open + 1..];
            let Some(close) = tail.find('}') else {
                break;
            };
            let inner = &tail[..close];
            if !inner.is_empty() && inner.bytes().all(|byte| byte.is_ascii_digit()) {
                found.insert(inner);
            }
            rest = &tail[close + 1..];
        }
        found
    }

    #[test]
    fn english_is_used_when_no_table_exists() {
        let t = Translator::new(Locale::En);
        assert_eq!(t.t(Key::Nav_Home), "Home");
    }

    #[test]
    fn every_key_has_english_text() {
        for key in Key::ALL {
            assert!(
                !key.english().is_empty(),
                "{} has no text",
                key.resource_name()
            );
        }
    }

    #[test]
    fn generated_tables_are_wired_for_every_locale() {
        assert!(table(Locale::En).is_empty());
        for &locale in Locale::ALL {
            if locale == Locale::En {
                continue;
            }
            assert!(
                !table(locale).is_empty(),
                "{} has no generated table",
                locale.tag()
            );
        }
        assert_eq!(table(Locale::De).len(), Key::ALL.len());
        assert_eq!(
            Translator::new(Locale::De).t(Key::Nav_Home),
            "Start",
            "German table is not used"
        );
    }

    #[test]
    fn translated_tables_are_sorted_and_unique() {
        for &locale in Locale::ALL {
            if locale == Locale::En {
                continue;
            }
            let entries = table(locale);
            for pair in entries.windows(2) {
                assert!(
                    pair[0].0 < pair[1].0,
                    "{} table is not sorted at `{}` / `{}`",
                    locale.tag(),
                    pair[0].0,
                    pair[1].0
                );
            }
        }
    }

    #[test]
    fn every_locale_has_the_english_key_set() {
        let english: BTreeSet<&str> = Key::ALL.iter().map(|key| key.resource_name()).collect();
        let mut counts = Vec::with_capacity(Locale::ALL.len());
        let mut problems = Vec::new();
        for &locale in Locale::ALL {
            let entries = table(locale);
            counts.push(format!("{}={}", locale.tag(), entries.len()));
            if locale == Locale::En {
                continue;
            }
            if entries.len() != Key::ALL.len() {
                problems.push(format!(
                    "{}: {} keys, expected {}",
                    locale.tag(),
                    entries.len(),
                    Key::ALL.len()
                ));
            }
            for &(name, _) in entries {
                if !english.contains(name) {
                    problems.push(format!("{}: extra key `{name}`", locale.tag()));
                }
            }
            for key in Key::ALL {
                if entries
                    .binary_search_by_key(&key.resource_name(), |(name, _)| *name)
                    .is_err()
                {
                    problems.push(format!(
                        "{}: missing key `{}`",
                        locale.tag(),
                        key.resource_name()
                    ));
                }
            }
        }
        println!("locale key counts: {}", counts.join(", "));
        assert!(
            problems.is_empty(),
            "locale key sets differ from English ({} key(s) expected):\n{}",
            Key::ALL.len(),
            problems.join("\n")
        );
    }

    #[test]
    fn every_key_resolves_to_non_empty_text_in_every_locale() {
        let mut problems = Vec::new();
        for &locale in Locale::ALL {
            let translator = Translator::new(locale);
            for key in Key::ALL {
                if translator.t(*key).trim().is_empty() {
                    problems.push(format!(
                        "{}: `{}` resolves to empty text",
                        locale.tag(),
                        key.resource_name()
                    ));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn placeholders_match_english_for_every_translated_key() {
        let mut problems = Vec::new();
        for &locale in Locale::ALL {
            if locale == Locale::En {
                continue;
            }
            let entries = table(locale);
            for key in Key::ALL {
                let Ok(index) =
                    entries.binary_search_by_key(&key.resource_name(), |(name, _)| *name)
                else {
                    continue;
                };
                let translated = placeholders(entries[index].1);
                let english = placeholders(key.english());
                if translated != english {
                    problems.push(format!(
                        "{}: `{}` has {:?}, English has {:?}",
                        locale.tag(),
                        key.resource_name(),
                        translated,
                        english
                    ));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn placeholders_are_substituted_by_index() {
        let t = Translator::new(Locale::En);
        // No placeholders: arguments are ignored.
        assert_eq!(t.format(Key::Nav_Home, &["a", "b"]), "Home");
        assert_eq!(t.format(Key::Downloads_Of, &["3", "7"]), "3 of 7");
    }

    #[test]
    fn translated_reordered_placeholders_follow_their_indices() {
        let t = Translator::new(Locale::Ja);
        assert_eq!(
            t.format(Key::Manage_PageLabel, &["2", "5"]),
            "5 ページ中 2 ページ目"
        );
    }

    #[test]
    fn unknown_placeholders_survive_formatting() {
        let t = Translator::new(Locale::En);
        assert_eq!(t.format(Key::Downloads_Of, &["3"]), "3 of {1}");
        assert_eq!(t.format(Key::Downloads_Of, &[]), "{0} of {1}");
    }

    #[test]
    fn formatting_a_template_handles_reordered_and_unknown_placeholders() {
        assert_eq!(format_template("{1} / {0}", &["a", "b"]), "b / a");
        assert_eq!(format_template("{0}-{2}", &["a"]), "a-{2}");
        assert_eq!(format_template("no args", &[]), "no args");
        assert_eq!(format_template("{0", &["a"]), "{0");
    }

    #[test]
    fn missing_translation_falls_back_to_english_per_key() {
        // A table containing only one key: the other key must fall back.
        let partial: &'static [(&'static str, &'static str)] = &[("Nav_Add", "Hinzufügen")];
        assert_eq!(lookup(partial, Key::Nav_Add), "Hinzufügen");
        assert_eq!(lookup(partial, Key::Nav_Home), "Home");
        assert_eq!(lookup(&[], Key::Nav_Home), "Home");
    }

    #[test]
    fn xml_entities_are_decoded_at_build_time() {
        assert_eq!(
            Translator::new(Locale::De).t(Key::Mode_CloseSteamContinue),
            "Steam schließen & fortfahren"
        );
        for &locale in Locale::ALL {
            for &(_, text) in table(locale) {
                assert!(
                    !text.contains("&amp;") && !text.contains("&#"),
                    "{} still contains a raw XML entity: {text}",
                    locale.tag()
                );
            }
        }
    }

    #[test]
    fn locale_switching_changes_the_table() {
        let mut translator = Translator::new(Locale::En);
        assert_eq!(translator.t(Key::Nav_Home), "Home");
        translator.set_locale(Locale::De);
        assert_eq!(translator.locale(), Locale::De);
        assert_eq!(translator.t(Key::Nav_Home), "Start");
    }
}
