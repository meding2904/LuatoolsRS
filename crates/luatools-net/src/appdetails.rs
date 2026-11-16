//! Disk-backed Steam store metadata cache.
//!
//! Each app's parsed metadata is stored as `details/<appid>.json`. The cache feeds library
//! filters, store details, and cover selection. Network resolution is rate-limited to stay
//! below Steam's appdetails quota.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use luatools_core::{AppId, AppMetadata, Error, Result};
use parking_lot::RwLock;
use reqwest::Client;

use crate::api::ApiClient;

/// Maximum accepted appdetails requests inside [`RATE_WINDOW`].
pub const RATE_LIMIT: usize = 190;

/// Rolling request window used by the appdetails limiter.
pub const RATE_WINDOW: Duration = Duration::from_secs(200);

/// A metadata cache rooted at one directory.
#[derive(Debug)]
pub struct AppDetailsCache {
    client: ApiClient,
    dir: PathBuf,
    memory: RwLock<HashMap<AppId, AppMetadata>>,
    request_times: tokio::sync::Mutex<VecDeque<Instant>>,
}

impl AppDetailsCache {
    /// Builds a cache rooted at `dir`.
    #[must_use]
    pub fn new(client: Client, dir: impl Into<PathBuf>) -> Self {
        Self {
            client: ApiClient::new(client),
            dir: dir.into(),
            memory: RwLock::new(HashMap::new()),
            request_times: tokio::sync::Mutex::new(VecDeque::new()),
        }
    }

    /// Builds a cache against a custom store origin, for tests.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] when `base` is not an absolute HTTP(S) URL.
    pub fn with_base_url(
        client: Client,
        dir: impl Into<PathBuf>,
        base: impl AsRef<str>,
    ) -> Result<Self> {
        Ok(Self {
            client: ApiClient::with_base_url(client, base)?,
            dir: dir.into(),
            memory: RwLock::new(HashMap::new()),
            request_times: tokio::sync::Mutex::new(VecDeque::new()),
        })
    }

    /// The directory containing `details/<appid>.json`.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// Path of one app's cached metadata.
    #[must_use]
    pub fn path_for(&self, app: AppId) -> PathBuf {
        self.dir.join(format!("{}.json", app.get()))
    }

    /// Returns metadata from memory or disk without touching the network.
    #[must_use]
    pub fn get_cached(&self, app: AppId) -> Option<AppMetadata> {
        if let Some(metadata) = self.memory.read().get(&app).cloned() {
            return Some(metadata);
        }
        let metadata = read_details(&self.path_for(app)).ok().flatten()?;
        self.memory.write().insert(app, metadata.clone());
        Some(metadata)
    }

    /// Returns true when a details file exists, including the `{}` delisted marker.
    #[must_use]
    pub fn has_full_details(&self, app: AppId) -> bool {
        self.path_for(app).is_file()
    }

    /// Resolves metadata over the network and writes the cache.
    ///
    /// Returns `None` when Steam has no usable entry. Rate-limit waits happen before the
    /// request; transport and parse failures are not retried.
    pub async fn resolve(&self, app: AppId) -> Option<AppMetadata> {
        if let Some(metadata) = self.get_cached(app) {
            return Some(metadata);
        }
        self.fetch(app).await
    }

    /// Refreshes one app, ignoring both memory and disk caches.
    ///
    /// A failed refresh leaves the previous disk entry in place until it is overwritten by a
    /// later successful lookup.
    pub async fn refresh(&self, app: AppId) -> Option<AppMetadata> {
        self.memory.write().remove(&app);
        self.fetch(app).await
    }

    async fn fetch(&self, app: AppId) -> Option<AppMetadata> {
        self.acquire_slot().await;
        match self.client.app_details(app).await {
            Ok(Some(raw)) => {
                let metadata = raw.metadata();
                let name = metadata.name.as_deref()?;
                if name.trim().is_empty() {
                    return None;
                }
                if let Err(error) = write_details(&self.path_for(app), &metadata) {
                    tracing::warn!(%error, app = app.get(), "could not cache app details");
                }
                self.memory.write().insert(app, metadata.clone());
                Some(metadata)
            }
            Ok(None) => {
                if let Err(error) = write_marker(&self.path_for(app)) {
                    tracing::warn!(%error, app = app.get(), "could not write delisted marker");
                }
                None
            }
            Err(error) => {
                tracing::debug!(%error, app = app.get(), "appdetails lookup failed");
                None
            }
        }
    }

    async fn acquire_slot(&self) {
        loop {
            let wait = {
                let mut times = self.request_times.lock().await;
                let now = Instant::now();
                while times
                    .front()
                    .is_some_and(|at| now.duration_since(*at) > RATE_WINDOW)
                {
                    times.pop_front();
                }
                if times.len() < RATE_LIMIT {
                    times.push_back(now);
                    return;
                }
                times
                    .front()
                    .map(|at| RATE_WINDOW.saturating_sub(now.duration_since(*at)))
                    .unwrap_or_default()
            };
            tokio::time::sleep(wait.max(Duration::from_millis(50))).await;
        }
    }
}

fn read_details(path: &Path) -> Result<Option<AppMetadata>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(format!("reading {}", path.display()), error)),
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    if bytes.as_slice() == b"{}" {
        return Ok(None);
    }
    let metadata = serde_json::from_slice(&bytes)
        .map_err(|error| Error::parse("app details", format!("{}: {error}", path.display())))?;
    Ok(Some(metadata))
}

fn write_details(path: &Path, metadata: &AppMetadata) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(metadata)?)
}

fn write_marker(path: &Path) -> Result<()> {
    write_atomic(path, b"{}")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, bytes)
        .map_err(|error| Error::io(format!("writing {}", temp.display()), error))?;
    std::fs::rename(&temp, path)
        .map_err(|error| Error::io(format!("renaming {}", path.display()), error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_metadata_round_trips_and_marks_delisted_apps() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AppDetailsCache::new(Client::new(), dir.path());
        let metadata = AppMetadata {
            name: Some("Counter-Strike 2".to_owned()),
            header_image: Some("https://cdn/730.jpg".to_owned()),
            ..AppMetadata::default()
        };
        write_details(&cache.path_for(AppId::new(730)), &metadata).unwrap();
        assert_eq!(cache.get_cached(AppId::new(730)), Some(metadata));
        assert!(cache.has_full_details(AppId::new(730)));

        write_marker(&cache.path_for(AppId::new(1))).unwrap();
        assert_eq!(cache.get_cached(AppId::new(1)), None);
        assert!(cache.has_full_details(AppId::new(1)));
    }

    #[test]
    fn corrupt_details_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cache = AppDetailsCache::new(Client::new(), dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(cache.path_for(AppId::new(1)), b"not json").unwrap();
        assert_eq!(cache.get_cached(AppId::new(1)), None);
    }
}
