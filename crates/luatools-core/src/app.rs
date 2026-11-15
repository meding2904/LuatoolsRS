//! Application-level view models rendered by the UI.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AppId, DepotId, ManifestId};

/// Whether an application is present on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum InstalledState {
    /// Not installed, or not visible in any library folder.
    #[default]
    NotInstalled,
    /// Installed in a Steam library.
    Installed {
        /// Absolute path to the install directory.
        path: PathBuf,
        /// Size on disk in bytes.
        size_on_disk: u64,
        /// Build id from the app manifest, when known.
        build_id: Option<String>,
    },
}

impl InstalledState {
    /// Returns true when the app is installed.
    #[must_use]
    pub const fn is_installed(&self) -> bool {
        matches!(self, Self::Installed { .. })
    }

    /// Returns the install directory, when installed.
    #[must_use]
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Self::NotInstalled => None,
            Self::Installed { path, .. } => Some(path),
        }
    }

    /// Returns the size on disk in bytes, or zero when not installed.
    #[must_use]
    pub const fn size_on_disk(&self) -> u64 {
        match self {
            Self::NotInstalled => 0,
            Self::Installed { size_on_disk, .. } => *size_on_disk,
        }
    }
}

/// One application row in the library list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppEntry {
    /// Steam app id.
    pub id: AppId,
    /// Display name.
    pub name: String,
    /// Installation state.
    pub installed: InstalledState,
    /// Cover image URL, when the API provided one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// Release date as a display string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// Developer names, comma separated for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    /// Genres, used by the filter chips.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Last played timestamp from the local config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<DateTime<Utc>>,
    /// Total playtime in minutes.
    #[serde(default)]
    pub playtime_minutes: u32,
}

impl AppEntry {
    /// Builds an uninstalled entry with the minimum required fields.
    #[must_use]
    pub fn new(id: AppId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            installed: InstalledState::NotInstalled,
            cover_url: None,
            release_date: None,
            developer: None,
            genres: Vec::new(),
            last_played: None,
            playtime_minutes: 0,
        }
    }

    /// Case-insensitive match used by the search field.
    #[must_use]
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let needle = needle.to_lowercase();
        self.name.to_lowercase().contains(&needle) || self.id.to_string().contains(&needle)
    }
}

/// One depot inside an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotEntry {
    /// Depot id.
    pub id: DepotId,
    /// Human-readable depot name, falling back to the id.
    pub name: String,
    /// Download size in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Manifest currently pinned for this depot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<ManifestId>,
    /// Whether the depot is optional DLC content.
    #[serde(default)]
    pub optional: bool,
    /// Whether the user enabled this depot in the UI.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

const fn default_true() -> bool {
    true
}

/// Everything the app detail pane needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppDetails {
    /// The library row this detail belongs to.
    pub app: AppEntry,
    /// Depots belonging to the app.
    #[serde(default)]
    pub depots: Vec<DepotEntry>,
    /// Steam launch options string currently configured.
    #[serde(default)]
    pub launch_options: String,
    /// Install directory name from the app manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<String>,
    /// Whether the local library was found on disk.
    #[serde(default)]
    pub library_found: bool,
}

impl AppDetails {
    /// Builds details for an app with no depots.
    #[must_use]
    pub fn new(app: AppEntry) -> Self {
        Self {
            app,
            depots: Vec::new(),
            launch_options: String::new(),
            install_dir: None,
            library_found: true,
        }
    }

    /// Sum of the known depot sizes.
    #[must_use]
    pub fn total_depot_size(&self) -> u64 {
        self.depots.iter().filter_map(|d| d.size).sum()
    }
}

/// A snapshot of every library the scanner found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySnapshot {
    /// Applications, sorted by name.
    #[serde(default)]
    pub apps: Vec<AppEntry>,
    /// Steam root that was scanned, when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_root: Option<PathBuf>,
    /// When the scan finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at: Option<DateTime<Utc>>,
}

impl LibrarySnapshot {
    /// Returns the app with the given id.
    #[must_use]
    pub fn app(&self, id: AppId) -> Option<&AppEntry> {
        self.apps.iter().find(|a| a.id == id)
    }

    /// Number of installed apps.
    #[must_use]
    pub fn installed_count(&self) -> usize {
        self.apps
            .iter()
            .filter(|a| a.installed.is_installed())
            .count()
    }

    /// Filters by search text and installation state.
    pub fn filtered<'a>(
        &'a self,
        needle: &'a str,
        installed_only: bool,
    ) -> impl Iterator<Item = &'a AppEntry> + 'a {
        self.apps
            .iter()
            .filter(move |a| (!installed_only || a.installed.is_installed()) && a.matches(needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_library() -> LibrarySnapshot {
        let mut cs2 = AppEntry::new(AppId::new(730), "Counter-Strike 2");
        cs2.installed = InstalledState::Installed {
            path: PathBuf::from("/games/cs2"),
            size_on_disk: 30_000_000_000,
            build_id: Some("17800000".to_owned()),
        };
        let dota = AppEntry::new(AppId::new(570), "Dota 2");
        LibrarySnapshot {
            apps: vec![cs2, dota],
            steam_root: Some(PathBuf::from("/steam")),
            scanned_at: None,
        }
    }

    #[test]
    fn search_matches_name_and_id_case_insensitively() {
        let lib = sample_library();
        let hits: Vec<_> = lib.filtered("COUNTER", false).map(|a| a.id.get()).collect();
        assert_eq!(hits, vec![730]);
        let by_id: Vec<_> = lib.filtered("570", false).map(|a| a.id.get()).collect();
        assert_eq!(by_id, vec![570]);
    }

    #[test]
    fn installed_only_filter_excludes_missing_apps() {
        let lib = sample_library();
        let hits: Vec<_> = lib.filtered("", true).map(|a| a.name.as_str()).collect();
        assert_eq!(hits, vec!["Counter-Strike 2"]);
        assert_eq!(lib.installed_count(), 1);
    }

    #[test]
    fn details_total_sums_known_depot_sizes_only() {
        let mut details = AppDetails::new(AppEntry::new(AppId::new(1), "x"));
        details.depots = vec![
            DepotEntry {
                id: DepotId::new(2),
                name: "a".into(),
                size: Some(100),
                manifest_id: None,
                optional: false,
                enabled: true,
            },
            DepotEntry {
                id: DepotId::new(3),
                name: "b".into(),
                size: None,
                manifest_id: None,
                optional: false,
                enabled: true,
            },
        ];
        assert_eq!(details.total_depot_size(), 100);
    }
}
