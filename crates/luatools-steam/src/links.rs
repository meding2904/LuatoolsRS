//! Steam link and `luatools://` protocol parsing.
//!
//! [`parse_steam_link`] recognises the URL shapes users drag onto the app: SteamDB app
//! pages, store pages, community hubs, `steam://` verbs, and LuaTools' own protocol.
//! Depot and package links are intentionally rejected: installing a depot id or a package
//! id as an app id would silently fetch the wrong content.

use luatools_core::AppId;

/// Largest accepted app id, exclusive.
///
/// `steam://rungameid/` also carries composite 64-bit ids for non-Steam shortcuts; those
/// are not app ids and are rejected by this bound.
pub const MAX_APP_ID: u32 = 2_000_000_000;

/// A link that names a Steam app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamLink {
    /// A community hub or discussion link.
    App(AppId),
    /// A `store.steampowered.com/app/<id>` link.
    Store(AppId),
    /// A `steamdb.info/app/<id>` link.
    SteamDb(AppId),
    /// A `steam://` verb or a `luatools://` protocol URL.
    Protocol {
        /// Lowercase verb, for example `install`, `store`, `run`, or `manage`.
        action: String,
        /// Referenced app.
        app: AppId,
        /// True for `luatools://install/silent/<id>`.
        silent: bool,
    },
}

impl SteamLink {
    /// Returns the app id named by this link.
    #[must_use]
    pub const fn app_id(&self) -> AppId {
        match self {
            Self::App(app) | Self::Store(app) | Self::SteamDb(app) => *app,
            Self::Protocol { app, .. } => *app,
        }
    }

    /// Returns true when this is a silent install request.
    #[must_use]
    pub const fn is_silent(&self) -> bool {
        matches!(self, Self::Protocol { silent: true, .. })
    }
}

/// Parses a Steam or LuaTools link.
///
/// Patterns are tried in the C# order: SteamDB, store, community, then `steam://`.
/// `luatools://` is checked first because it is app-specific. Ids must be greater than
/// zero and less than [`MAX_APP_ID`]. Bare numbers are not accepted here.
#[must_use]
pub fn parse_steam_link(text: &str) -> Option<SteamLink> {
    if text.trim().is_empty() {
        return None;
    }

    if let Some(index) = find_ci(text, "luatools://") {
        let rest = text.get(index + "luatools://".len()..)?;
        if let Some(link) = parse_luatools(rest) {
            return Some(link);
        }
    }

    if let Some(digits) = scan_needle(text, "steamdb.info/app/")
        && let Some(app) = parse_id(digits)
    {
        return Some(SteamLink::SteamDb(app));
    }

    if let Some(digits) = scan_store(text)
        && let Some(app) = parse_id(digits)
    {
        return Some(SteamLink::Store(app));
    }

    if let Some(digits) = scan_needle(text, "steamcommunity.com/app/")
        && let Some(app) = parse_id(digits)
    {
        return Some(SteamLink::App(app));
    }

    if let Some((action, digits)) = scan_steam_protocol(text) {
        return parse_id(digits).map(|app| SteamLink::Protocol {
            action,
            app,
            silent: false,
        });
    }

    None
}

/// Extracts an app id from loose text.
///
/// A recognised [`parse_steam_link`] shape wins, then any `/app/<id>` path, then the first
/// standalone run of five or more digits. The five-digit floor deliberately rejects short
/// incidental numbers such as a year.
#[must_use]
pub fn extract_app_id(text: &str) -> Option<AppId> {
    if let Some(link) = parse_steam_link(text) {
        return Some(link.app_id());
    }

    let mut search_from = 0usize;
    while let Some(index) = find_ci(&text[search_from..], "/app/") {
        let absolute = search_from + index + "/app/".len();
        let digits = leading_digits(text.get(absolute..)?);
        if !digits.is_empty()
            && let Some(app) = parse_id(digits)
        {
            return Some(app);
        }
        search_from = absolute;
    }

    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index - start < 5 {
            continue;
        }

        let before_ok = match text
            .get(..start)
            .and_then(|prefix| prefix.chars().next_back())
        {
            Some(ch) => !ch.is_alphanumeric() && ch != '_',
            None => true,
        };
        let after_ok = match text.get(index..).and_then(|suffix| suffix.chars().next()) {
            Some(ch) => !ch.is_alphanumeric() && ch != '_',
            None => true,
        };
        if before_ok
            && after_ok
            && let Some(app) = text.get(start..index).and_then(parse_id)
        {
            return Some(app);
        }
    }

    None
}

fn parse_luatools(rest: &str) -> Option<SteamLink> {
    let rest = rest.split(['?', '#']).next()?;
    let (action, path) = rest.split_once('/')?;
    let action = action.trim().to_ascii_lowercase();
    if !matches!(action.as_str(), "game" | "install" | "manage" | "fix") {
        return None;
    }

    let mut id = path.trim_start_matches('/');
    let mut silent = false;
    if action == "install"
        && let Some(stripped) = strip_prefix_ci(id, "silent/")
    {
        silent = true;
        id = stripped;
    }

    let app = parse_id(id)?;
    Some(SteamLink::Protocol {
        action,
        app,
        silent,
    })
}

fn scan_store(text: &str) -> Option<&str> {
    let needle = "store.steampowered.com/";
    let mut search_from = 0usize;
    while let Some(index) = find_ci(text.get(search_from..)?, needle) {
        let absolute = search_from + index + needle.len();
        let after_host = text.get(absolute..)?;
        let after_gate = strip_prefix_ci(after_host, "agecheck/").unwrap_or(after_host);
        if let Some(after_app) = strip_prefix_ci(after_gate, "app/") {
            let digits = leading_digits(after_app);
            if !digits.is_empty() {
                return Some(digits);
            }
        }
        search_from = absolute;
    }
    None
}

fn scan_steam_protocol(text: &str) -> Option<(String, &str)> {
    let needle = "steam://";
    let mut search_from = 0usize;
    while let Some(index) = find_ci(text.get(search_from..)?, needle) {
        let absolute = search_from + index + needle.len();
        let rest = text.get(absolute..)?;
        for action in ["store", "install", "run", "rungameid"] {
            if let Some(after_action) = strip_prefix_ci(rest, action)
                && let Some(after_slash) = after_action.strip_prefix('/')
            {
                let digits = leading_digits(after_slash);
                if !digits.is_empty() {
                    return Some((action.to_owned(), digits));
                }
            }
        }
        search_from = absolute;
    }
    None
}

/// Finds the first occurrence of `needle` that is followed by digits.
///
/// A matching host with no digits (for example `steamdb.info/depot/`) lets the scan
/// continue, mirroring the C# regex engine looking for a complete match.
fn scan_needle<'a>(text: &'a str, needle: &str) -> Option<&'a str> {
    let mut search_from = 0usize;
    while let Some(index) = find_ci(text.get(search_from..)?, needle) {
        let absolute = search_from + index + needle.len();
        let digits = leading_digits(text.get(absolute..)?);
        if !digits.is_empty() {
            return Some(digits);
        }
        search_from = absolute;
    }
    None
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() < prefix.len() {
        return None;
    }
    if text
        .as_bytes()
        .get(..prefix.len())?
        .eq_ignore_ascii_case(prefix.as_bytes())
    {
        text.get(prefix.len()..)
    } else {
        None
    }
}

fn leading_digits(text: &str) -> &str {
    let count = text
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    text.get(..count).unwrap_or_default()
}

fn parse_id(text: &str) -> Option<AppId> {
    let value: u64 = text.parse().ok()?;
    if value > 0 && value < u64::from(MAX_APP_ID) {
        u32::try_from(value).ok().map(AppId::new)
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_luatools_protocol_including_silent_installs() {
        assert_eq!(
            parse_steam_link("luatools://install/silent/730"),
            Some(SteamLink::Protocol {
                action: "install".to_owned(),
                app: AppId::new(730),
                silent: true,
            })
        );
        assert_eq!(
            parse_steam_link("LUATOOLS://Install/SILENT/730?ref=x"),
            Some(SteamLink::Protocol {
                action: "install".to_owned(),
                app: AppId::new(730),
                silent: true,
            })
        );
        assert_eq!(
            parse_steam_link("luatools://manage/570"),
            Some(SteamLink::Protocol {
                action: "manage".to_owned(),
                app: AppId::new(570),
                silent: false,
            })
        );
        assert!(parse_steam_link("luatools://install/silent/").is_none());
        assert!(parse_steam_link("luatools://bogus/730").is_none());
        assert_eq!(
            extract_app_id("luatools://install/silent/730"),
            Some(AppId::new(730))
        );
    }

    #[test]
    fn accepts_every_documented_steam_url_shape() {
        let cases = [
            (
                "https://steamdb.info/app/648800/",
                SteamLink::SteamDb(AppId::new(648_800)),
            ),
            (
                "https://steamdb.info/app/648800/depots/",
                SteamLink::SteamDb(AppId::new(648_800)),
            ),
            (
                "https://store.steampowered.com/app/648800/Raft/",
                SteamLink::Store(AppId::new(648_800)),
            ),
            (
                "store.steampowered.com/app/648800",
                SteamLink::Store(AppId::new(648_800)),
            ),
            (
                "https://store.steampowered.com/agecheck/app/648800/",
                SteamLink::Store(AppId::new(648_800)),
            ),
            (
                "https://steamcommunity.com/app/648800/discussions/",
                SteamLink::App(AppId::new(648_800)),
            ),
            (
                "steam://store/648800",
                SteamLink::Protocol {
                    action: "store".to_owned(),
                    app: AppId::new(648_800),
                    silent: false,
                },
            ),
            (
                "steam://rungameid/220",
                SteamLink::Protocol {
                    action: "rungameid".to_owned(),
                    app: AppId::new(220),
                    silent: false,
                },
            ),
        ];

        for (text, expected) in cases {
            assert_eq!(parse_steam_link(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn rejects_depot_package_composite_and_bare_inputs() {
        for text in [
            "https://steamdb.info/depot/648801/",
            "https://steamdb.info/sub/12345/",
            "648800",
            "app 648800",
            "steam://rungameid/13548193523847888896",
            "https://example.com/app/648800/",
            "https://steamdb.info/",
            "",
            "   ",
        ] {
            assert_eq!(parse_steam_link(text), None, "{text}");
        }
    }

    #[test]
    fn steamdb_wins_over_an_earlier_store_link() {
        let text = "https://store.steampowered.com/app/220/ https://steamdb.info/app/648800/";
        assert_eq!(
            parse_steam_link(text),
            Some(SteamLink::SteamDb(AppId::new(648_800)))
        );
        assert_eq!(
            parse_steam_link("https://steamdb.info/app/648800/ https://steamdb.info/app/220/"),
            Some(SteamLink::SteamDb(AppId::new(648_800)))
        );
    }

    #[test]
    fn enforces_the_app_id_bounds() {
        assert_eq!(
            parse_steam_link("steam://store/1"),
            Some(SteamLink::Protocol {
                action: "store".to_owned(),
                app: AppId::new(1),
                silent: false,
            })
        );
        assert_eq!(
            extract_app_id("app id 1999999999"),
            Some(AppId::new(1_999_999_999))
        );
        assert_eq!(extract_app_id("app id 2000000000"), None);
        assert!(parse_steam_link("steam://store/0").is_none());
    }

    #[test]
    fn extracts_ids_from_app_paths_and_standalone_tokens() {
        assert_eq!(
            extract_app_id("see https://example.com/app/730/notes"),
            Some(AppId::new(730))
        );
        assert_eq!(
            extract_app_id("Raft is 648800 in the store"),
            Some(AppId::new(648_800))
        );
        assert_eq!(extract_app_id("build 2024 is not an app"), None);
        assert_eq!(extract_app_id("abc12345"), None);
        assert_eq!(extract_app_id("13548193523847888896"), None);
    }
}
