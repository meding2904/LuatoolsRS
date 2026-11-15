//! Steam launch options and the durable launch-edit overlay.
//!
//! Steam stores the entries behind a game's Play button under `config.launch.<n>` in
//! `appinfo.vdf`. This module models one entry, reads the entries out of an appinfo body,
//! and persists staged edits in a JSON file so they can be restored after Steam rewrites
//! its cache.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use luatools_core::{AppId, Error, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::binary_vdf::Bvdf;

/// One entry behind Steam's Play button.
///
/// Unused optional fields are empty strings. Steam omits them from appinfo entirely rather
/// than writing blank values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct LaunchOption {
    /// Executable to run, relative to the install directory.
    pub executable: String,
    /// Command-line arguments.
    pub arguments: String,
    /// Working directory, usually empty.
    pub working_dir: String,
    /// Human-readable label shown by Steam.
    pub description: String,
    /// Steam option type (`default`, `option1`, ...).
    ///
    /// The C# model calls this `Type`; it is renamed to `option_type` in Rust.
    #[serde(rename = "Type")]
    pub option_type: String,
    /// Operating systems this entry applies to (`windows`, `linux,macos`, ...).
    pub os_list: String,
    /// CPU architecture restriction, usually empty.
    pub os_arch: String,
    /// Beta branch key required for this entry.
    pub beta_key: String,
    /// DLC ownership marker carried by some entries.
    pub owns_dlc: String,
}

impl Default for LaunchOption {
    fn default() -> Self {
        Self {
            executable: String::new(),
            arguments: String::new(),
            working_dir: String::new(),
            description: String::new(),
            option_type: "default".to_owned(),
            os_list: String::new(),
            os_arch: String::new(),
            beta_key: String::new(),
            owns_dlc: String::new(),
        }
    }
}

impl LaunchOption {
    /// Reads one entry out of its `config.launch.<n>` table.
    ///
    /// A missing `type` becomes an empty string, matching the C# `FromTable` behaviour (the
    /// `"default"` value is only a constructor default).
    #[must_use]
    pub fn from_table(table: &Bvdf) -> Self {
        let config = table.get_table("config");
        Self {
            executable: table.get_text("executable").unwrap_or_default(),
            arguments: table.get_text("arguments").unwrap_or_default(),
            working_dir: table.get_text("workingdir").unwrap_or_default(),
            description: table.get_text("description").unwrap_or_default(),
            option_type: table.get_text("type").unwrap_or_default(),
            os_list: config
                .and_then(|value| value.get_text("oslist"))
                .unwrap_or_default(),
            os_arch: config
                .and_then(|value| value.get_text("osarch"))
                .unwrap_or_default(),
            beta_key: config
                .and_then(|value| value.get_text("BetaKey"))
                .unwrap_or_default(),
            owns_dlc: config
                .and_then(|value| value.get_text("ownsdlc"))
                .unwrap_or_default(),
        }
    }

    /// A short label built from the executable, arguments, and OS list.
    ///
    /// Values are joined with two spaces, a middle dot, and two spaces.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = Vec::with_capacity(3);
        if !self.executable.trim().is_empty() {
            parts.push(self.executable.trim());
        }
        if !self.arguments.trim().is_empty() {
            parts.push(self.arguments.trim());
        }
        if !self.os_list.trim().is_empty() {
            parts.push(self.os_list.trim());
        }
        parts.join("  ·  ")
    }

    /// Returns true when this option applies to `os`.
    ///
    /// An empty OS list matches every platform. Tokens are comma- or semicolon-separated
    /// and compared ASCII case-insensitively. `win`, `mac`, and `osx` are accepted as
    /// aliases for `windows` and `macos`.
    #[must_use]
    pub fn applies_to_os(&self, os: &str) -> bool {
        let list = self.os_list.trim();
        if list.is_empty() {
            return true;
        }

        let current = normalize_os(os);
        list.split([',', ';'])
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .any(|token| normalize_os(token) == current)
    }

    /// Returns true when this option applies to the platform the crate is running on.
    #[must_use]
    pub fn applies_to_current_os(&self) -> bool {
        self.applies_to_os(std::env::consts::OS)
    }
}

/// Filters launch options to those matching `os`, preserving input order.
#[must_use]
pub fn filter_for_os<'a>(options: &'a [LaunchOption], os: &str) -> Vec<&'a LaunchOption> {
    options
        .iter()
        .filter(|option| option.applies_to_os(os))
        .collect()
}

fn normalize_os(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "win" | "windows" => "windows".to_owned(),
        "mac" | "macos" | "osx" | "darwin" => "macos".to_owned(),
        "linux" | "steamos" => "linux".to_owned(),
        other => other.to_owned(),
    }
}

/// Reads every table entry under `config.launch`, in file order.
///
/// Non-table properties are ignored. A missing `config` or `launch` table yields an empty
/// list.
#[must_use]
pub fn read_launch_options(app_body: &Bvdf) -> Vec<LaunchOption> {
    let Some(launch) = app_body
        .get_table("config")
        .and_then(|config| config.get_table("launch"))
    else {
        return Vec::new();
    };

    launch
        .items()
        .iter()
        .filter_map(|property| property.as_table().map(LaunchOption::from_table))
        .collect()
}

/// One game's staged launch-option edit plus the snapshot needed to undo it.
///
/// List order is authoritative. The C# JSON also carries each entry's `Index` and
/// `SourceIndex`, but the crate-level [`LaunchOption`] model intentionally does not, so
/// those two fields are not written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LaunchMod {
    /// Steam app id this record belongs to.
    pub app_id: AppId,
    /// appinfo change number when the original snapshot was taken.
    #[serde(default)]
    pub change_number: u32,
    /// Launch entries as Steam shipped them, captured before the first edit.
    #[serde(default)]
    pub original: Vec<LaunchOption>,
    /// Launch entries the user wants Steam to use.
    #[serde(default)]
    pub desired: Vec<LaunchOption>,
    /// When the desired list was last updated.
    #[serde(default = "utc_now")]
    pub saved_at: DateTime<Utc>,
}

fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// Durable JSON overlay for launch-option edits.
///
/// The file is a top-level JSON array of [`LaunchMod`] values with PascalCase property
/// names, matching the C# `launchmods.json`. Writes are atomic: content goes to a `.tmp`
/// file, the previous file is copied to `.bak`, and the temporary file is renamed into
/// place. An empty store removes the main file.
#[derive(Debug)]
pub struct LaunchModStore {
    path: PathBuf,
    mods: Mutex<IndexMap<AppId, LaunchMod>>,
}

impl LaunchModStore {
    /// Opens (or creates) a store at `path`.
    ///
    /// A corrupt or unreadable file is moved aside to `<path>.corrupt` and the store starts
    /// empty, matching the C# recovery behaviour. A file containing JSON `null` is treated
    /// as empty without quarantine.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let mods = match load(&path) {
            Ok(mods) => mods,
            Err(error) => {
                warn!(
                    path = %path.display(),
                    %error,
                    "launch mod store is unreadable; quarantining it"
                );
                quarantine(&path);
                IndexMap::new()
            }
        };
        Self {
            path,
            mods: Mutex::new(mods),
        }
    }

    /// Path of the backing JSON file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the staged desired list, when this app has an edit.
    ///
    /// # Errors
    ///
    /// Returns an error when the internal lock was poisoned.
    pub fn staged(&self, app: AppId) -> Result<Option<Vec<LaunchOption>>> {
        Ok(self.lock()?.get(&app).map(|entry| entry.desired.clone()))
    }

    /// Returns the pre-edit snapshot, when this app has an edit.
    ///
    /// # Errors
    ///
    /// Returns an error when the internal lock was poisoned.
    pub fn original(&self, app: AppId) -> Result<Option<Vec<LaunchOption>>> {
        Ok(self.lock()?.get(&app).map(|entry| entry.original.clone()))
    }

    /// Records the desired list for `app`.
    ///
    /// On the first call for an app, `options` is also captured as the original snapshot.
    /// Use [`Self::stage_with_original`] when the caller already has a pre-edit snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be persisted.
    pub fn stage(&self, app: AppId, options: &[LaunchOption]) -> Result<()> {
        self.stage_with_original(app, options, options)
    }

    /// Records a desired list together with an explicit pre-edit snapshot.
    ///
    /// A second call for the same app replaces only the desired list; the original snapshot
    /// and change number are preserved, matching the C# `Save` semantics.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be persisted.
    pub fn stage_with_original(
        &self,
        app: AppId,
        original: &[LaunchOption],
        desired: &[LaunchOption],
    ) -> Result<()> {
        let mut guard = self.lock()?;
        let mut next = guard.clone();
        if let Some(entry) = next.get_mut(&app) {
            entry.desired = desired.to_vec();
            entry.saved_at = Utc::now();
        } else {
            next.insert(
                app,
                LaunchMod {
                    app_id: app,
                    change_number: 0,
                    original: original.to_vec(),
                    desired: desired.to_vec(),
                    saved_at: Utc::now(),
                },
            );
        }
        persist(&self.path, &next)?;
        *guard = next;
        Ok(())
    }

    /// Updates a stored snapshot after Steam has shipped a new app version.
    ///
    /// Returns false when the app is not staged or the change number is unchanged. The
    /// desired list is left untouched.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be persisted.
    pub fn rebase(&self, app: AppId, change_number: u32, current: &[LaunchOption]) -> Result<bool> {
        let mut guard = self.lock()?;
        let Some(entry) = guard.get(&app) else {
            return Ok(false);
        };
        if entry.change_number == change_number {
            return Ok(false);
        }

        let mut next = guard.clone();
        if let Some(entry) = next.get_mut(&app) {
            entry.change_number = change_number;
            entry.original = current.to_vec();
        }
        persist(&self.path, &next)?;
        *guard = next;
        Ok(true)
    }

    /// Forgets an app's edit and persists the store.
    ///
    /// Removing an absent app is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be persisted.
    pub fn clear(&self, app: AppId) -> Result<()> {
        let mut guard = self.lock()?;
        if guard.shift_remove(&app).is_none() {
            return Ok(());
        }
        let next = guard.clone();
        persist(&self.path, &next)?;
        *guard = next;
        Ok(())
    }

    /// Returns true when any edit for this app is staged.
    #[must_use]
    pub fn is_modded(&self, app: AppId) -> bool {
        self.mods.lock().is_ok_and(|mods| mods.contains_key(&app))
    }

    /// Returns a snapshot of every stored edit in insertion order.
    ///
    /// # Errors
    ///
    /// Returns an error when the internal lock was poisoned.
    pub fn all(&self) -> Result<Vec<LaunchMod>> {
        Ok(self.lock()?.values().cloned().collect())
    }

    /// Returns true when no edits are staged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mods.lock().is_ok_and(|mods| mods.is_empty())
    }

    /// Returns true when the two lists differ in any managed field.
    ///
    /// Order matters. `source_index` is not modelled here; every field in [`LaunchOption`]
    /// participates.
    #[must_use]
    pub fn differs(a: &[LaunchOption], b: &[LaunchOption]) -> bool {
        a.len() != b.len() || a.iter().zip(b).any(|(left, right)| left != right)
    }

    fn lock(&self) -> Result<MutexGuard<'_, IndexMap<AppId, LaunchMod>>> {
        self.mods
            .lock()
            .map_err(|_| Error::Other("launch mod store lock is poisoned".to_owned()))
    }
}

fn load(path: &Path) -> Result<IndexMap<AppId, LaunchMod>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(IndexMap::new()),
        Err(error) => {
            return Err(Error::io(
                format!("reading launch mod store {}", path.display()),
                error,
            ));
        }
    };

    let loaded: Option<Vec<LaunchMod>> = serde_json::from_str(&text)?;
    let Some(loaded) = loaded else {
        return Ok(IndexMap::new());
    };

    let mut mods = IndexMap::with_capacity(loaded.len());
    for entry in loaded {
        if mods.insert(entry.app_id, entry).is_some() {
            return Err(Error::parse(
                "launch mod store",
                "duplicate AppId in launchmods.json",
            ));
        }
    }
    Ok(mods)
}

fn persist(path: &Path, mods: &IndexMap<AppId, LaunchMod>) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::io(
                format!("creating launch mod directory {}", parent.display()),
                error,
            )
        })?;
    }

    if mods.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(Error::io(
                    format!("removing launch mod store {}", path.display()),
                    error,
                ));
            }
        }
    }

    let entries: Vec<&LaunchMod> = mods.values().collect();
    let json = serde_json::to_string_pretty(&entries)?;
    let tmp = with_suffix(path, ".tmp");
    std::fs::write(&tmp, json)
        .map_err(|error| Error::io(format!("writing launch mod store {}", tmp.display()), error))?;

    if path.exists() {
        let backup = with_suffix(path, ".bak");
        if let Err(error) = std::fs::copy(path, &backup) {
            warn!(
                path = %backup.display(),
                %error,
                "could not refresh launch mod store backup"
            );
        }
    }

    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::io(
            format!("replacing launch mod store {}", path.display()),
            error,
        ));
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn quarantine(path: &Path) {
    let corrupt = with_suffix(path, ".corrupt");
    if corrupt.exists() {
        let _ = std::fs::remove_file(&corrupt);
    }
    if let Err(error) = std::fs::rename(path, &corrupt) {
        warn!(
            path = %path.display(),
            %error,
            "could not quarantine corrupt launch mod store"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn option(executable: &str, arguments: &str, os_list: &str) -> LaunchOption {
        LaunchOption {
            executable: executable.to_owned(),
            arguments: arguments.to_owned(),
            os_list: os_list.to_owned(),
            ..LaunchOption::default()
        }
    }

    #[test]
    fn launch_options_filter_by_operating_system() {
        let options = vec![
            option("all.exe", "", ""),
            option("win.exe", "", "windows"),
            option("unix", "", "Linux, macOS"),
            option("mixed.exe", "", "Windows;Linux"),
            option("elsewhere", "", "android"),
        ];

        let linux: Vec<_> = filter_for_os(&options, "linux")
            .into_iter()
            .map(|entry| entry.executable.as_str())
            .collect();
        assert_eq!(linux, vec!["all.exe", "unix", "mixed.exe"]);

        assert!(options[1].applies_to_os("win"));
        assert!(!options[1].applies_to_os("linux"));
        assert!(options[2].applies_to_os("macos"));
        assert!(options[2].applies_to_os("osx"));
        assert!(options[4].applies_to_os("android"));
        assert!(!options[4].applies_to_os("windows"));
    }

    #[test]
    fn reads_launch_entries_from_an_appinfo_body() {
        let mut entry = Bvdf::new();
        entry.push_text("executable", "Game.exe");
        entry.push_text("arguments", "-novid");
        entry.push_text("type", "default");
        let mut config = Bvdf::new();
        config.push_text("oslist", "windows");
        config.push_text("BetaKey", "public");
        entry.push_table("config", config);

        let mut launch = Bvdf::new();
        launch.push_table("0", entry);
        launch.push_text("1", "ignored, not a table");
        let mut body = Bvdf::new();
        let mut app_config = Bvdf::new();
        app_config.push_table("launch", launch);
        body.push_table("config", app_config);

        let options = read_launch_options(&body);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].executable, "Game.exe");
        assert_eq!(options[0].arguments, "-novid");
        assert_eq!(options[0].os_list, "windows");
        assert_eq!(options[0].beta_key, "public");
        assert_eq!(options[0].summary(), "Game.exe  ·  -novid  ·  windows");
    }

    #[test]
    fn mod_store_round_trips_and_restores_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launchmods.json");

        let original = vec![option("Game.exe", "", "windows")];
        let desired = vec![
            option("ModLoader.exe", "--profile dev", "windows"),
            option("Game.exe", "-safe", ""),
        ];

        let store = LaunchModStore::new(path.clone());
        assert!(!store.is_modded(AppId::new(730)));
        assert!(store.is_empty());

        store
            .stage_with_original(AppId::new(730), &original, &desired)
            .unwrap();
        assert!(store.is_modded(AppId::new(730)));
        assert_eq!(
            store.staged(AppId::new(730)).unwrap(),
            Some(desired.clone())
        );
        assert_eq!(
            store.original(AppId::new(730)).unwrap(),
            Some(original.clone())
        );

        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"AppId\": 730"), "{json}");
        assert!(json.contains("\"Type\": \"default\""), "{json}");
        assert!(json.contains("\"SavedAt\""), "{json}");

        let reopened = LaunchModStore::new(path.clone());
        assert_eq!(
            reopened.staged(AppId::new(730)).unwrap(),
            Some(desired.clone())
        );
        assert_eq!(reopened.original(AppId::new(730)).unwrap(), Some(original));

        // A second stage must not replace the original snapshot.
        let newer = vec![option("Other.exe", "", "")];
        reopened.stage(AppId::new(730), &newer).unwrap();
        assert_eq!(
            reopened.original(AppId::new(730)).unwrap(),
            Some(vec![option("Game.exe", "", "windows")])
        );
        assert_eq!(reopened.staged(AppId::new(730)).unwrap(), Some(newer));

        reopened.clear(AppId::new(730)).unwrap();
        assert!(!reopened.is_modded(AppId::new(730)));
        assert!(!path.exists());
        assert!(LaunchModStore::new(path).is_empty());
    }

    #[test]
    fn rebase_refreshes_only_when_the_change_number_moves() {
        let dir = tempfile::tempdir().unwrap();
        let store = LaunchModStore::new(dir.path().join("launchmods.json"));
        let app = AppId::new(220);
        store
            .stage_with_original(
                app,
                &[option("old.exe", "", "")],
                &[option("mod.exe", "", "")],
            )
            .unwrap();

        assert!(
            !store
                .rebase(app, 0, &[option("ignored.exe", "", "")])
                .unwrap()
        );
        assert!(store.rebase(app, 42, &[option("new.exe", "", "")]).unwrap());
        assert_eq!(
            store.original(app).unwrap(),
            Some(vec![option("new.exe", "", "")])
        );
        assert_eq!(
            store.staged(app).unwrap(),
            Some(vec![option("mod.exe", "", "")])
        );
    }

    #[test]
    fn corrupt_json_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launchmods.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let store = LaunchModStore::new(path.clone());
        assert!(store.is_empty());
        assert!(dir.path().join("launchmods.json.corrupt").exists());
        assert!(!path.exists());
    }
}
