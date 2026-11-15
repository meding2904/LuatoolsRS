//! Supported interface languages.
//!
//! The set mirrors the 29 translated locales in the original application plus English, which
//! is the neutral resource. Tags follow BCP-47 and preserve the region variants the build
//! ships (`es-419`, `pt-BR`, `pt-PT`, `zh-Hans`, `zh-Hant`).

use serde::{Deserialize, Serialize};

/// One interface language.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Locale {
    /// English (neutral resources).
    #[default]
    En,
    /// Arabic.
    Ar,
    /// Bulgarian.
    Bg,
    /// Czech.
    Cs,
    /// Danish.
    Da,
    /// German.
    De,
    /// Greek.
    El,
    /// Spanish (Spain).
    Es,
    /// Spanish (Latin America).
    Es419,
    /// Finnish.
    Fi,
    /// French.
    Fr,
    /// Hungarian.
    Hu,
    /// Indonesian.
    Id,
    /// Italian.
    It,
    /// Japanese.
    Ja,
    /// Korean.
    Ko,
    /// Norwegian Bokmal.
    Nb,
    /// Dutch.
    Nl,
    /// Polish.
    Pl,
    /// Portuguese (Brazil).
    PtBr,
    /// Portuguese (Portugal).
    PtPt,
    /// Romanian.
    Ro,
    /// Russian.
    Ru,
    /// Swedish.
    Sv,
    /// Thai.
    Th,
    /// Turkish.
    Tr,
    /// Ukrainian.
    Uk,
    /// Vietnamese.
    Vi,
    /// Chinese (Simplified).
    ZhHans,
    /// Chinese (Traditional).
    ZhHant,
}

impl Locale {
    /// Every supported locale, in menu order.
    pub const ALL: &'static [Self] = &[
        Self::En,
        Self::Ar,
        Self::Bg,
        Self::Cs,
        Self::Da,
        Self::De,
        Self::El,
        Self::Es,
        Self::Es419,
        Self::Fi,
        Self::Fr,
        Self::Hu,
        Self::Id,
        Self::It,
        Self::Ja,
        Self::Ko,
        Self::Nb,
        Self::Nl,
        Self::Pl,
        Self::PtBr,
        Self::PtPt,
        Self::Ro,
        Self::Ru,
        Self::Sv,
        Self::Th,
        Self::Tr,
        Self::Uk,
        Self::Vi,
        Self::ZhHans,
        Self::ZhHant,
    ];

    /// The BCP-47 tag, matching the resource file suffix.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ar => "ar",
            Self::Bg => "bg",
            Self::Cs => "cs",
            Self::Da => "da",
            Self::De => "de",
            Self::El => "el",
            Self::Es => "es",
            Self::Es419 => "es-419",
            Self::Fi => "fi",
            Self::Fr => "fr",
            Self::Hu => "hu",
            Self::Id => "id",
            Self::It => "it",
            Self::Ja => "ja",
            Self::Ko => "ko",
            Self::Nb => "nb",
            Self::Nl => "nl",
            Self::Pl => "pl",
            Self::PtBr => "pt-BR",
            Self::PtPt => "pt-PT",
            Self::Ro => "ro",
            Self::Ru => "ru",
            Self::Sv => "sv",
            Self::Th => "th",
            Self::Tr => "tr",
            Self::Uk => "uk",
            Self::Vi => "vi",
            Self::ZhHans => "zh-Hans",
            Self::ZhHant => "zh-Hant",
        }
    }

    /// The language name written in that language.
    #[must_use]
    pub const fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ar => "العربية",
            Self::Bg => "Български",
            Self::Cs => "Čeština",
            Self::Da => "Dansk",
            Self::De => "Deutsch",
            Self::El => "Ελληνικά",
            Self::Es => "Español",
            Self::Es419 => "Español (Latinoamérica)",
            Self::Fi => "Suomi",
            Self::Fr => "Français",
            Self::Hu => "Magyar",
            Self::Id => "Bahasa Indonesia",
            Self::It => "Italiano",
            Self::Ja => "日本語",
            Self::Ko => "한국어",
            Self::Nb => "Norsk bokmål",
            Self::Nl => "Nederlands",
            Self::Pl => "Polski",
            Self::PtBr => "Português (Brasil)",
            Self::PtPt => "Português (Portugal)",
            Self::Ro => "Română",
            Self::Ru => "Русский",
            Self::Sv => "Svenska",
            Self::Th => "ไทย",
            Self::Tr => "Türkçe",
            Self::Uk => "Українська",
            Self::Vi => "Tiếng Việt",
            Self::ZhHans => "简体中文",
            Self::ZhHant => "繁體中文",
        }
    }

    /// Whether the script is right-to-left.
    #[must_use]
    pub const fn is_rtl(self) -> bool {
        matches!(self, Self::Ar)
    }

    /// Resolves a tag or OS culture name, falling back through base languages.
    ///
    /// Accepts the exact tag (`pt-BR`), its lowercase form, an underscore form (`pt_BR`), a
    /// bare base language (`pt` picks the first regional variant in [`Self::ALL`]), and the
    /// neutral resource aliases `zh`, `zh-CN`, `zh-TW`, `es-419`/`es_419`.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        let normalized = tag.trim().replace('_', "-").to_ascii_lowercase();
        if normalized.is_empty() {
            return None;
        }
        if let Some(exact) = Self::ALL
            .iter()
            .copied()
            .find(|l| l.tag().eq_ignore_ascii_case(&normalized))
        {
            return Some(exact);
        }
        match normalized.as_str() {
            "zh-cn" | "zh-sg" | "zh" => Some(Self::ZhHans),
            "zh-tw" | "zh-hk" | "zh-mo" => Some(Self::ZhHant),
            "es-419" => Some(Self::Es419),
            _ => {
                let base = normalized.split('-').next().unwrap_or_default();
                Self::ALL.iter().copied().find(|l| {
                    l.tag()
                        .split('-')
                        .next()
                        .is_some_and(|t| t.eq_ignore_ascii_case(base))
                })
            }
        }
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.tag())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_locales_have_unique_tags() {
        let tags: HashSet<_> = Locale::ALL.iter().map(|l| l.tag()).collect();
        assert_eq!(tags.len(), Locale::ALL.len());
        assert_eq!(Locale::ALL.len(), 30);
    }

    #[test]
    fn tags_round_trip() {
        for locale in Locale::ALL {
            assert_eq!(Locale::from_tag(locale.tag()), Some(*locale));
        }
    }

    #[test]
    fn os_culture_names_resolve_to_regional_variants() {
        assert_eq!(Locale::from_tag("pt_BR"), Some(Locale::PtBr));
        assert_eq!(Locale::from_tag("zh-CN"), Some(Locale::ZhHans));
        assert_eq!(Locale::from_tag("zh-TW"), Some(Locale::ZhHant));
        assert_eq!(Locale::from_tag("es-419"), Some(Locale::Es419));
        assert_eq!(Locale::from_tag("fr-FR"), Some(Locale::Fr));
        assert_eq!(Locale::from_tag("pt"), Some(Locale::PtBr));
        assert_eq!(Locale::from_tag("xx"), None);
    }

    #[test]
    fn arabic_is_the_only_rtl_locale() {
        let rtl: Vec<_> = Locale::ALL.iter().filter(|l| l.is_rtl()).collect();
        assert_eq!(rtl, vec![&Locale::Ar]);
    }
}
