//! Page modules. Each page owns its transient state and a `show` function.

pub mod add;
pub mod builds;
pub mod downloads;
pub mod home;
pub mod manage;
pub mod onboarding;
pub mod plugin;
pub mod settings;

use luatools_i18n::Key;

/// The pages reachable from the navigation rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Page {
    /// Dashboard with library and download summaries.
    #[default]
    Home,
    /// Store search and featured catalogue.
    Add,
    /// Browse the installed and owned apps.
    Builds,
    /// Download queue and history.
    Downloads,
    /// Per-app management: depots, launch options, folders.
    Manage,
    /// Steam store plugin.
    Plugin,
    /// Application settings.
    Settings,
}

impl Page {
    /// Pages shown in the navigation rail, top to bottom.
    pub const NAV: &'static [Self] = &[
        Self::Home,
        Self::Add,
        Self::Builds,
        Self::Downloads,
        Self::Manage,
        Self::Plugin,
        Self::Settings,
    ];

    /// Navigation label.
    #[must_use]
    pub const fn label(self) -> Key {
        match self {
            Self::Home => Key::Nav_Home,
            Self::Add => Key::Nav_Add,
            Self::Builds => Key::Nav_Builds,
            Self::Downloads => Key::Nav_Downloads,
            Self::Manage => Key::Nav_Manage,
            Self::Plugin => Key::Plugin_Title,
            Self::Settings => Key::Nav_Settings,
        }
    }

    /// Stable identifier used for persistence and tests.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Add => "add",
            Self::Builds => "builds",
            Self::Downloads => "downloads",
            Self::Manage => "manage",
            Self::Plugin => "plugin",
            Self::Settings => "settings",
        }
    }

    /// Resolves an identifier previously returned by [`Self::id`].
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::NAV.iter().copied().find(|p| p.id() == id)
    }

    /// Resolves a backend navigation target.
    #[must_use]
    pub const fn from_nav(target: luatools_core::NavTarget) -> Self {
        match target {
            luatools_core::NavTarget::Home => Self::Home,
            luatools_core::NavTarget::Add => Self::Add,
            luatools_core::NavTarget::Builds => Self::Builds,
            luatools_core::NavTarget::Downloads => Self::Downloads,
            luatools_core::NavTarget::Manage => Self::Manage,
            luatools_core::NavTarget::Plugin => Self::Plugin,
            luatools_core::NavTarget::Settings => Self::Settings,
        }
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
