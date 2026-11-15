//! Filesystem layout.
//!
//! Every path the application writes goes through [`AppPaths`]. The root is taken from
//! `LUATOOLS_HOME` when set (tests, portable builds), otherwise from the platform config
//! directory.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Resolved directories for configuration, cache, data, and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    /// Discovers the platform default locations.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Other`] when the platform has no home directory.
    pub fn discover() -> Result<Self> {
        if let Some(root) = std::env::var_os("LUATOOLS_HOME") {
            return Ok(Self::new(PathBuf::from(root)));
        }
        let dirs = directories::ProjectDirs::from("tools", "LuaTools", "LuaTools")
            .ok_or_else(|| Error::Other("no home directory available".to_owned()))?;
        Ok(Self::new(dirs.config_dir().to_path_buf()))
    }

    /// Creates a layout rooted at an explicit directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root of all application state.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Configuration files (`settings.json`).
    #[must_use]
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// Disposable caches (covers, app list, manifests).
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Persistent data that is expensive to rebuild (download history, plugin state).
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Log files.
    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Default download destination when the user has not chosen one.
    #[must_use]
    pub fn default_download_dir(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// Full path of the settings file.
    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir().join("settings.json")
    }

    /// Full path of the download history file.
    #[must_use]
    pub fn history_file(&self) -> PathBuf {
        self.data_dir().join("history.json")
    }

    /// Creates every directory in the layout.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when a directory cannot be created.
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.config_dir(),
            self.cache_dir(),
            self.data_dir(),
            self.log_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_relative_to_the_root() {
        let paths = AppPaths::new("/tmp/lt");
        assert_eq!(paths.config_dir(), PathBuf::from("/tmp/lt/config"));
        assert_eq!(
            paths.settings_file(),
            PathBuf::from("/tmp/lt/config/settings.json")
        );
        assert_eq!(
            paths.history_file(),
            PathBuf::from("/tmp/lt/data/history.json")
        );
    }

    #[test]
    fn ensure_creates_every_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::new(tmp.path().join("home"));
        paths.ensure().unwrap();
        for dir in [
            paths.config_dir(),
            paths.cache_dir(),
            paths.data_dir(),
            paths.log_dir(),
        ] {
            assert!(dir.is_dir(), "{} should exist", dir.display());
        }
    }
}
