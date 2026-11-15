//! Small JSON cache with time-based expiry and atomic writes.
//!
//! The backend caches API responses, app metadata, and covers under
//! [`crate::AppPaths::cache_dir`]. Writes go to a temporary file in the same directory and are
//! renamed into place, so an interrupted write never leaves a half-written entry.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

/// A directory of JSON documents keyed by a caller-chosen string.
#[derive(Debug, Clone)]
pub struct JsonCache {
    root: PathBuf,
}

/// Metadata about a cache hit, returned alongside the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheEntry {
    /// Age of the entry.
    pub age: Duration,
    /// Size of the stored document in bytes.
    pub size: u64,
}

impl JsonCache {
    /// Creates a cache rooted at `dir`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { root: dir.into() }
    }

    /// The cache root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Maps a key to a file path, hashing keys that contain separators.
    fn path_for(&self, key: &str) -> PathBuf {
        let safe = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        let name = if safe {
            format!("{key}.json")
        } else {
            let digest = crate::hash::sha256_bytes(key.as_bytes());
            format!("{}.json", &digest.as_str()[..32])
        };
        self.root.join(name)
    }

    /// Reads a document if it exists and is younger than `ttl`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Serde`] when the document is corrupt; callers treat that as a miss and
    /// overwrite it.
    pub fn get<T: DeserializeOwned>(&self, key: &str, ttl: Duration) -> Result<Option<T>> {
        let path = self.path_for(key);
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(format!("stat {}", path.display()), e)),
        };
        let modified = meta
            .modified()
            .map_err(|e| Error::io(format!("mtime of {}", path.display()), e))?;
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age > ttl {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Writes a document, replacing any previous entry.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory or temporary file cannot be written.
    pub fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| Error::io(format!("creating {}", self.root.display()), e))?;
        let path = self.path_for(key);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(value)?;
        std::fs::write(&tmp, &bytes)
            .map_err(|e| Error::io(format!("writing {}", tmp.display()), e))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| Error::io(format!("renaming {}", path.display()), e))?;
        Ok(())
    }

    /// Removes one entry. Missing entries are not an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for errors other than a missing file.
    pub fn remove(&self, key: &str) -> Result<()> {
        let path = self.path_for(key);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        }
    }

    /// Deletes every entry in the cache directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory cannot be removed.
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(format!("clearing {}", self.root.display()), e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Payload {
        name: String,
        count: u32,
    }

    fn cache() -> (tempfile::TempDir, JsonCache) {
        let tmp = tempfile::tempdir().unwrap();
        let cache = JsonCache::new(tmp.path().join("cache"));
        (tmp, cache)
    }

    #[test]
    fn missing_entries_are_none() {
        let (_tmp, cache) = cache();
        let got: Option<Payload> = cache.get("absent", Duration::from_secs(60)).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn stored_values_round_trip() {
        let (_tmp, cache) = cache();
        let payload = Payload {
            name: "cs2".to_owned(),
            count: 7,
        };
        cache.put("app-730", &payload).unwrap();
        let got: Option<Payload> = cache.get("app-730", Duration::from_secs(60)).unwrap();
        assert_eq!(got, Some(payload));
    }

    #[test]
    fn expired_entries_are_ignored() {
        let (_tmp, cache) = cache();
        cache
            .put(
                "old",
                &Payload {
                    name: "x".into(),
                    count: 1,
                },
            )
            .unwrap();
        let got: Option<Payload> = cache.get("old", Duration::ZERO).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn keys_with_separators_are_hashed() {
        let (_tmp, cache) = cache();
        cache
            .put(
                "../escape",
                &Payload {
                    name: "x".into(),
                    count: 2,
                },
            )
            .unwrap();
        let files: Vec<_> = std::fs::read_dir(cache.root()).unwrap().collect();
        assert_eq!(files.len(), 1);
        let name = files[0].as_ref().unwrap().file_name();
        assert!(!name.to_string_lossy().contains(".."));
        let got: Option<Payload> = cache.get("../escape", Duration::from_secs(60)).unwrap();
        assert!(got.is_some());
    }

    #[test]
    fn clear_removes_the_tree_and_is_idempotent() {
        let (_tmp, cache) = cache();
        cache
            .put(
                "a",
                &Payload {
                    name: "x".into(),
                    count: 3,
                },
            )
            .unwrap();
        cache.clear().unwrap();
        assert!(!cache.root().exists());
        cache.clear().unwrap();
    }
}
