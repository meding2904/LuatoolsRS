//! Steam hardware and accessory appid blacklist.
//!
//! The store and featured endpoints include Steam Deck, Index, controllers, and VR hardware.
//! The desktop client hides those entries from catalogue surfaces. The list is refreshed at
//! most once per session and kept for 14 days; a failed refresh leaves the previous set intact.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use luatools_core::config::HARDWARE_APPID_LIST_URL;
use luatools_core::{AppId, Error, Result};
use parking_lot::RwLock;
use reqwest::Client;
use tokio::sync::OnceCell;

/// How long a downloaded blacklist remains authoritative.
pub const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// A lazily refreshed set of hardware app ids.
#[derive(Debug)]
pub struct HardwareAppIdService {
    client: Client,
    path: PathBuf,
    ids: RwLock<BTreeSet<u32>>,
    loaded: OnceCell<()>,
}

impl HardwareAppIdService {
    /// Creates a service seeded from `path` when the file is parseable.
    #[must_use]
    pub fn new(client: Client, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let ids = read_disk(&path).unwrap_or_default();
        Self {
            client,
            path,
            ids: RwLock::new(ids),
            loaded: OnceCell::new(),
        }
    }

    /// The on-disk cache file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns true when the app should be hidden from catalogue surfaces.
    #[must_use]
    pub fn is_blacklisted(&self, app: AppId) -> bool {
        self.ids.read().contains(&app.get())
    }

    /// Number of blacklisted ids currently loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.read().len()
    }

    /// Returns true when no ids are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.read().is_empty()
    }

    /// Refreshes the list once per session when the disk copy is stale or empty.
    pub async fn ensure_fresh(&self) {
        let _ = self
            .loaded
            .get_or_init(|| async {
                if is_fresh(&self.path) && !self.is_empty() {
                    return;
                }
                if let Err(error) = self.refresh().await {
                    tracing::debug!(%error, "hardware app list refresh failed");
                }
            })
            .await;
    }

    /// Forces a network refresh.
    ///
    /// An empty or malformed response is rejected so a bad mirror cannot wipe a good cache.
    ///
    /// # Errors
    ///
    /// Returns a network, HTTP, parse, or IO error when the refresh cannot complete.
    pub async fn refresh(&self) -> Result<()> {
        let body = crate::github::fetch_text_with(
            &self.client,
            HARDWARE_APPID_LIST_URL,
            &crate::github::GithubMirrors::default(),
        )
        .await?;
        let ids = parse_list(&body)?;
        if ids.is_empty() {
            return Err(Error::parse(
                "hardware app list",
                "the response contained no usable app ids",
            ));
        }
        *self.ids.write() = ids.clone();
        write_disk(&self.path, &ids)
    }
}

/// Parses the public hardware list.
///
/// Accepts the documented array of `{ "appid": n }` objects and a plain array of numbers.
///
/// # Errors
///
/// Returns [`Error::Parse`] when the body is not one of those shapes.
pub fn parse_list(body: &str) -> Result<BTreeSet<u32>> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| Error::parse("hardware app list", error.to_string()))?;
    let array = value
        .as_array()
        .ok_or_else(|| Error::parse("hardware app list", "response is not an array"))?;
    let mut ids = BTreeSet::new();
    for item in array {
        let raw = item
            .as_u64()
            .or_else(|| item.get("appid").and_then(serde_json::Value::as_u64));
        if let Some(raw) = raw.and_then(|raw| u32::try_from(raw).ok())
            && raw > 0
        {
            ids.insert(raw);
        }
    }
    Ok(ids)
}

fn is_fresh(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age < MAX_AGE)
}

fn read_disk(path: &Path) -> Result<BTreeSet<u32>> {
    let bytes = std::fs::read(path)
        .map_err(|error| Error::io(format!("reading {}", path.display()), error))?;
    let raw: Vec<u32> = serde_json::from_slice(&bytes)
        .map_err(|error| Error::parse("hardware app list", error.to_string()))?;
    Ok(raw.into_iter().filter(|id| *id > 0).collect())
}

fn write_disk(path: &Path, ids: &BTreeSet<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
    }
    let bytes = serde_json::to_vec(&ids.iter().copied().collect::<Vec<_>>())?;
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
    fn object_and_number_entries_are_accepted() {
        let ids = parse_list(
            r#"[{"appid":1675200,"name":"Steam Deck"},{"appid":0},{"appid":1059530},42]"#,
        )
        .expect("hardware list");
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&1_675_200));
        assert!(ids.contains(&42));
        assert!(!ids.contains(&0));
    }

    #[test]
    fn malformed_or_non_array_bodies_are_errors() {
        assert!(parse_list("{}").is_err());
        assert!(parse_list("nope").is_err());
    }

    #[test]
    fn disk_round_trip_filters_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hardware.json");
        let ids = BTreeSet::from([1, 2]);
        write_disk(&path, &ids).unwrap();
        assert_eq!(read_disk(&path).unwrap(), ids);
    }
}
