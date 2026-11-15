//! Crash-safe settings persistence.
//!
//! Load order is primary, then `.bak`, then defaults. A primary file that fails to parse is
//! moved to `.corrupt` rather than overwritten, so a user can recover a hand-edited file. Save
//! writes a temporary file and renames it over the primary, keeping the previous primary as
//! `.bak`.

use std::path::{Path, PathBuf};

use luatools_core::Settings;
use luatools_core::error::{Error, Result};

/// Reads and writes `settings.json`.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Creates a store for the given settings file.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The settings file this store manages.
    #[cfg(test)]
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn backup_path(&self) -> PathBuf {
        self.path.with_extension("json.bak")
    }

    fn temp_path(&self) -> PathBuf {
        self.path.with_extension("json.tmp")
    }

    fn corrupt_path(&self) -> PathBuf {
        self.path.with_extension("json.corrupt")
    }

    /// Loads settings, falling back to the backup and then to defaults.
    ///
    /// This never fails: a corrupt file is preserved and the defaults are returned, because a
    /// broken settings file must not stop the application from starting.
    #[must_use]
    pub fn load(&self) -> Settings {
        match self.read(&self.path) {
            Ok(Some(settings)) => return settings.sanitized(),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "settings file is corrupt");
                self.preserve_corrupt();
            }
        }
        match self.read(&self.backup_path()) {
            Ok(Some(settings)) => settings.sanitized(),
            Ok(None) => Settings::default(),
            Err(error) => {
                tracing::warn!(%error, "settings backup is corrupt");
                Settings::default()
            }
        }
    }

    fn read(&self, path: &Path) -> Result<Option<Settings>> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.is_empty() => Ok(None),
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(Error::from),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::io(format!("reading {}", path.display()), error)),
        }
    }

    fn preserve_corrupt(&self) {
        if let Err(error) = std::fs::rename(&self.path, self.corrupt_path()) {
            tracing::warn!(%error, "could not preserve corrupt settings file");
        }
    }

    /// Writes settings atomically, keeping the previous file as a backup.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory, temporary file, backup, or final rename fails.
    /// Returns [`Error::InvalidInput`] when validation rejects the values.
    pub fn save(&self, settings: &Settings) -> Result<()> {
        settings.validate()?;
        let settings = settings.clone().sanitized();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
        }
        let bytes = serde_json::to_vec_pretty(&settings)?;
        let temp = self.temp_path();
        std::fs::write(&temp, &bytes)
            .map_err(|e| Error::io(format!("writing {}", temp.display()), e))?;
        if self.path.exists() {
            let backup = self.backup_path();
            if let Err(error) = std::fs::copy(&self.path, &backup) {
                tracing::warn!(%error, "could not refresh settings backup");
            }
        }
        std::fs::rename(&temp, &self.path)
            .map_err(|e| Error::io(format!("renaming {}", self.path.display()), e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luatools_core::Locale;

    fn store() -> (tempfile::TempDir, SettingsStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(tmp.path().join("config/settings.json"));
        (tmp, store)
    }

    #[test]
    fn missing_file_yields_defaults() {
        let (_tmp, store) = store();
        assert_eq!(store.load(), Settings::default());
    }

    #[test]
    fn round_trip_preserves_values() {
        let (_tmp, store) = store();
        let settings = Settings {
            language: Locale::Ja,
            max_concurrent_downloads: 7,
            download_dir: std::env::temp_dir().join("lt-downloads"),
            ..Settings::default()
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load(), settings);
    }

    #[test]
    fn backup_is_used_when_primary_is_corrupt() {
        let (_tmp, store) = store();
        let good = Settings {
            language: Locale::De,
            download_dir: std::env::temp_dir().join("lt"),
            ..Settings::default()
        };
        store.save(&good).unwrap();
        // A second save moves the first file to `.bak`; that copy is what recovery reads.
        store.save(&good).unwrap();
        std::fs::write(store.path(), b"not json").unwrap();
        assert_eq!(store.load(), good);
        assert!(store.corrupt_path().exists());
    }

    #[test]
    fn corrupt_file_is_preserved_and_defaults_returned() {
        let (_tmp, store) = store();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), b"{ broken").unwrap();
        let loaded = store.load();
        assert_eq!(loaded, Settings::default());
        assert!(store.corrupt_path().exists());
        assert!(!store.path().exists());
    }

    #[test]
    fn saving_twice_creates_a_backup() {
        let (_tmp, store) = store();
        let first = Settings::default();
        store.save(&first).unwrap();
        let mut second = first.clone();
        second.language = Locale::Fr;
        store.save(&second).unwrap();
        assert!(store.backup_path().exists());
        assert_eq!(store.load(), second);
    }

    #[test]
    fn invalid_settings_are_rejected() {
        let (_tmp, store) = store();
        let bad = Settings {
            proxy_url: Some("not a url".to_owned()),
            ..Settings::default()
        };
        assert!(store.save(&bad).is_err());
        assert!(!store.path().exists());
    }
}
