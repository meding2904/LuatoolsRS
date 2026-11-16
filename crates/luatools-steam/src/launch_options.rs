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
use luatools_core::{AppId, Error, LaunchOption, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::binary_vdf::Bvdf;

pub use luatools_core::filter_for_os;

/// Reads one entry out of its `config.launch.<n>` table.
///
/// A missing `type` becomes an empty string, matching the C# `FromTable` behaviour (the
/// `"default"` value is only a constructor default).
#[must_use]
pub fn launch_option_from_table(table: &Bvdf) -> LaunchOption {
    let config = table.get_table("config");
    LaunchOption {
        index: String::new(),
        source_index: String::new(),
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
        .filter_map(|property| {
            property.as_table().map(|table| {
                let mut option = launch_option_from_table(table);
                option.index = property.name.clone();
                option.source_index = property.name.clone();
                option
            })
        })
        .collect()
}

/// Replaces an app body's `config.launch` table with `options`, preserving unmodelled keys on
/// entries that still have a same-index table.
///
/// The table key is the list position (`0`, `1`, ...), because Steam reads the running order
/// from the key. Empty optional fields are removed rather than written blank, matching Steam.
pub fn write_launch_options(app_body: &mut Bvdf, options: &[LaunchOption]) {
    let previous = app_body
        .get_table("config")
        .and_then(|config| config.get_table("launch"))
        .cloned();
    let mut launch = Bvdf::with_capacity(options.len());
    for (index, option) in options.iter().enumerate() {
        let lookup = if option.source_index.is_empty() {
            index.to_string()
        } else {
            option.source_index.clone()
        };
        let mut table = previous
            .as_ref()
            .and_then(|launch| launch.get_table(&lookup).cloned())
            .unwrap_or_default();
        set_or_remove(&mut table, "executable", &option.executable);
        set_or_remove(&mut table, "arguments", &option.arguments);
        set_or_remove(&mut table, "workingdir", &option.working_dir);
        set_or_remove(&mut table, "description", &option.description);
        set_or_remove(&mut table, "type", &option.option_type);

        let any_config = !option.os_list.trim().is_empty()
            || !option.os_arch.trim().is_empty()
            || !option.beta_key.trim().is_empty()
            || !option.owns_dlc.trim().is_empty();
        if any_config {
            if table.get_table("config").is_none() {
                table.push_table("config", Bvdf::default());
            }
            if let Some(config) = table.get_table_mut("config") {
                set_or_remove(config, "oslist", &option.os_list);
                set_or_remove(config, "osarch", &option.os_arch);
                set_or_remove(config, "BetaKey", &option.beta_key);
                set_or_remove(config, "ownsdlc", &option.owns_dlc);
            }
        } else if table.get_table("config").is_some_and(Bvdf::is_empty) {
            table.remove("config");
        }
        launch.push_table(index.to_string(), table);
    }

    match app_body.get_table_mut("config") {
        Some(config) => {
            config.remove("launch");
            config.push_table("launch", launch);
        }
        None => {
            let mut config = Bvdf::default();
            config.push_table("launch", launch);
            app_body.push_table("config", config);
        }
    }
}

fn set_or_remove(table: &mut Bvdf, key: &str, value: &str) {
    let value = value.trim();
    table.remove(key);
    if !value.is_empty() {
        table.push_text(key, value);
    }
}

/// One game's staged launch-option edit plus the snapshot needed to undo it.
///
/// List order is authoritative. Each entry also carries its original `Index` and
/// `SourceIndex` so unmodelled keys survive a reorder.
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
    #[test]
    fn write_replaces_launch_table_and_omits_empty_fields() {
        let mut body = Bvdf::default();
        let mut config = Bvdf::default();
        let mut launch = Bvdf::default();
        let mut existing = Bvdf::default();
        existing.push_text("executable", "old.exe");
        existing.push_text("vacmodulefilename", "keep.bin");
        launch.push_table("0", existing);
        config.push_table("launch", launch);
        body.push_table("config", config);

        let options = vec![
            option("new.exe", "-novid", "windows"),
            LaunchOption {
                executable: "tool.exe".to_owned(),
                ..LaunchOption::default()
            },
        ];
        write_launch_options(&mut body, &options);
        let launch = body
            .get_table("config")
            .and_then(|config| config.get_table("launch"))
            .expect("launch table");
        assert_eq!(launch.items().len(), 2);
        let first = launch.get_table("0").expect("first");
        assert_eq!(first.get_text("executable").as_deref(), Some("new.exe"));
        assert_eq!(first.get_text("arguments").as_deref(), Some("-novid"));
        assert_eq!(
            first.get_text("vacmodulefilename").as_deref(),
            Some("keep.bin")
        );
        assert!(first.get_text("workingdir").is_none());
        assert_eq!(
            first
                .get_table("config")
                .and_then(|config| config.get_text("oslist"))
                .as_deref(),
            Some("windows")
        );
        let second = launch.get_table("1").expect("second");
        assert_eq!(second.get_text("executable").as_deref(), Some("tool.exe"));
        assert!(second.get_text("arguments").is_none());
    }

    #[test]
    fn write_creates_config_and_launch_when_missing() {
        let mut body = Bvdf::default();
        write_launch_options(&mut body, &[option("game.exe", "", "")]);
        let launch = body
            .get_table("config")
            .and_then(|config| config.get_table("launch"))
            .expect("launch table");
        assert_eq!(
            launch
                .get_table("0")
                .and_then(|table| table.get_text("executable"))
                .as_deref(),
            Some("game.exe")
        );
    }
}
