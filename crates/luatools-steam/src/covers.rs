//! Cover image path resolution.
//!
//! This module does not download or decode images; it only decides where a cached cover
//! lives and whether Steam already has one on disk. The C# `CoverCache` also downloads and
//! validates JPEGs, which belongs to the network/UI layers in this port.

use std::path::{Path, PathBuf};

use luatools_core::AppId;

/// Resolves cover paths inside LuaTools' data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverCache {
    directory: PathBuf,
}

impl CoverCache {
    /// Creates a cache rooted at `directory` (normally `<data dir>/covers`).
    #[must_use]
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Creates a cache at `<data_dir>/covers`.
    #[must_use]
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("covers"))
    }

    /// Root directory of the cache.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the cache path for an app, whether or not the file exists.
    #[must_use]
    pub fn path_for(&self, app: AppId) -> PathBuf {
        self.directory.join(format!("{}.jpg", app.get()))
    }

    /// Returns the cached cover path when the file exists.
    #[must_use]
    pub fn local_path(&self, app: AppId) -> Option<PathBuf> {
        let path = self.path_for(app);
        path.is_file().then_some(path)
    }

    /// Finds a cover Steam itself has already downloaded.
    ///
    /// The normal layout is `appcache/librarycache/<appid>/` with `library_600x900.jpg` or
    /// `header.jpg`; older Steam clients used flat `<appid>_header.jpg` files beside it.
    /// This function only checks for files; it never decodes or validates them.
    #[must_use]
    pub fn discover_on_disk(steam_root: &Path, app: AppId) -> Option<PathBuf> {
        // shortcut: the C# CoverCache only writes its own `<appid>.jpg` files; this mirrors
        // Steam's standard `appcache/librarycache` layout by filename only and does not
        // validate image bytes.
        let library_cache = steam_root.join("appcache").join("librarycache");
        let app_id = app.to_string();
        let app_directory = library_cache.join(&app_id);

        for name in [
            "library_600x900.jpg",
            "header.jpg",
            "library_600x900_2x.jpg",
            "library_hero.jpg",
        ] {
            let candidate = app_directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        for name in [
            format!("{app_id}_library_600x900.jpg"),
            format!("{app_id}_header.jpg"),
            format!("{app_id}.jpg"),
        ] {
            let candidate = library_cache.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_are_appid_jpg_files() {
        let cache = CoverCache::new(PathBuf::from("/data/covers"));
        assert_eq!(
            cache.path_for(AppId::new(730)),
            PathBuf::from("/data/covers/730.jpg")
        );
        assert_eq!(
            CoverCache::in_data_dir(Path::new("/data")).directory(),
            Path::new("/data/covers")
        );
        assert_eq!(cache.local_path(AppId::new(730)), None);
    }

    #[test]
    fn discovers_steam_library_cache_assets_in_preference_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app_dir = root.join("appcache").join("librarycache").join("730");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("header.jpg"), b"header").unwrap();
        std::fs::write(app_dir.join("library_600x900.jpg"), b"capsule").unwrap();

        assert_eq!(
            CoverCache::discover_on_disk(root, AppId::new(730)),
            Some(app_dir.join("library_600x900.jpg"))
        );
        assert_eq!(CoverCache::discover_on_disk(root, AppId::new(440)), None);
    }

    #[test]
    fn falls_back_to_flat_legacy_cover_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let library_cache = root.join("appcache").join("librarycache");
        std::fs::create_dir_all(&library_cache).unwrap();
        std::fs::write(library_cache.join("440_header.jpg"), b"legacy").unwrap();

        assert_eq!(
            CoverCache::discover_on_disk(root, AppId::new(440)),
            Some(library_cache.join("440_header.jpg"))
        );
    }
}
