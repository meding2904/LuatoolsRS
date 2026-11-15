//! Page modules. Each page owns its transient state and a `show` function.

pub mod builds;
pub mod downloads;
pub mod home;
pub mod manage;
pub mod onboarding;
pub mod settings;

use luatools_i18n::Key;

/// The pages reachable from the navigation rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Page {
    /// Dashboard with library and download summaries.
    #[default]
    Home,
    /// Browse the installed and owned apps.
    Builds,
    /// Download queue and history.
    Downloads,
    /// Per-app management: depots, launch options, folders.
    Manage,
    /// Application settings.
    Settings,
}

impl Page {
    /// Pages shown in the navigation rail, top to bottom.
    pub const NAV: &'static [Self] = &[
        Self::Home,
        Self::Builds,
        Self::Downloads,
        Self::Manage,
        Self::Settings,
    ];

    /// Navigation label.
    #[must_use]
    pub const fn label(self) -> Key {
        match self {
            Self::Home => Key::Nav_Home,
            Self::Builds => Key::Nav_Builds,
            Self::Downloads => Key::Nav_Downloads,
            Self::Manage => Key::Nav_Manage,
            Self::Settings => Key::Nav_Settings,
        }
    }

    /// Stable identifier used for persistence and tests.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Builds => "builds",
            Self::Downloads => "downloads",
            Self::Manage => "manage",
            Self::Settings => "settings",
        }
    }

    /// Resolves an identifier previously returned by [`Self::id`].
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::NAV.iter().copied().find(|p| p.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ids_are_unique_and_round_trip() {
        let mut ids: Vec<_> = Page::NAV.iter().map(|p| p.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Page::NAV.len());
        for page in Page::NAV {
            assert_eq!(Page::from_id(page.id()), Some(*page));
        }
        assert_eq!(Page::from_id("nope"), None);
    }
}
