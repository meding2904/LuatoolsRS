//! Bulk appid-to-name lookup with a 14-day disk cache.
//!
//! The Steam store search answers only a handful of names at a time. Library and management
//! screens need a name for every app they display, including delisted products that no longer
//! appear in search. This module keeps the public app-list sources behind one small cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use luatools_core::config::{APP_LIST_URL_PRIMARY, APP_LIST_URL_STEAM};
use luatools_core::{AppId, Error, Result};
use parking_lot::RwLock;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::OnceCell;

/// How long a downloaded list remains authoritative.
pub const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// A lock-protected appid-to-name table with lazy disk and network loading.
#[derive(Debug)]
pub struct AppListCache {
    client: Client,
    path: PathBuf,
    names: RwLock<BTreeMap<u32, String>>,
    loaded: OnceCell<()>,
}

impl AppListCache {
    /// Creates a cache backed by `path` and seeds it from any parseable file already there.
    #[must_use]
    pub fn new(client: Client, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let names = read_disk(&path).unwrap_or_default();
        Self {
            client,
            path,
            names: RwLock::new(names),
            loaded: OnceCell::new(),
        }
    }

    /// The on-disk file this cache manages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a name without touching the network.
    #[must_use]
    pub fn get_name(&self, app: AppId) -> Option<String> {
        self.names.read().get(&app.get()).cloned()
    }

    /// Number of names currently loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.read().len()
    }

    /// Returns true when no names are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.read().is_empty()
    }

    /// Loads the list once per session.
    ///
    /// A fresh non-empty disk file is used without a request. Otherwise the primary source is
    /// tried, then Steam's public list. A failed refresh keeps whatever names were already
    /// loaded.
    pub async fn ensure_loaded(&self) {
        let _ = self
            .loaded
            .get_or_init(|| async {
                if is_fresh(&self.path) && !self.is_empty() {
                    return;
                }
                if let Err(error) = self.refresh().await {
                    tracing::debug!(%error, "app list refresh failed");
                }
            })
            .await;
    }

    /// Forces a network refresh and persists the result.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Network`] or [`Error::Parse`] when neither source yields a usable list,
    /// and [`Error::Io`] when the result cannot be saved.
    pub async fn refresh(&self) -> Result<()> {
        let mut last_error = None;
        for source in [Source::Primary, Source::Steam] {
            match self.fetch(source).await {
                Ok(names) if !names.is_empty() => {
                    *self.names.write() = names.clone();
                    write_disk(&self.path, &names)?;
                    return Ok(());
                }
                Ok(_) => {
                    last_error = Some(Error::parse("app list", "source returned no usable names"));
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| Error::parse("app list", "no app-list source was available")))
    }

    async fn fetch(&self, source: Source) -> Result<BTreeMap<u32, String>> {
        let url = source.url();
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| Error::Network {
                context: format!("GET {url}"),
                message: error.to_string(),
            })?;
        if !response.status().is_success() {
            return Err(Error::Http {
                status: response.status().as_u16(),
                url: url.to_owned(),
            });
        }
        let body = response.text().await.map_err(|error| Error::Network {
            context: format!("reading {url}"),
            message: error.to_string(),
        })?;
        parse_list(&body, SourceMarker(source))
    }
}

#[derive(Debug, Clone, Copy)]
enum Source {
    Primary,
    Steam,
}

impl Source {
    const fn url(self) -> &'static str {
        match self {
            Self::Primary => APP_LIST_URL_PRIMARY,
            Self::Steam => APP_LIST_URL_STEAM,
        }
    }
}

/// Parses either supported app-list shape.
///
/// # Errors
///
/// Returns [`Error::Parse`] when the body is not the JSON shape for `source`.
pub fn parse_list(body: &str, source: SourceMarker) -> Result<BTreeMap<u32, String>> {
    let source = source.0;
    let mut names = BTreeMap::new();
    match source {
        Source::Primary => {
            let entries: Vec<FlatEntry> = serde_json::from_str(body)
                .map_err(|error| Error::parse("app list", error.to_string()))?;
            for entry in entries {
                insert_name(&mut names, entry.appid, entry.name);
            }
        }
        Source::Steam => {
            let response: SteamResponse = serde_json::from_str(body)
                .map_err(|error| Error::parse("app list", error.to_string()))?;
            for entry in response.applist.apps {
                insert_name(&mut names, entry.appid, entry.name);
            }
        }
    }
    Ok(names)
}

/// Marker used by [`parse_list`] so tests can select a source without exposing internals.
#[derive(Debug, Clone, Copy)]
pub struct SourceMarker(Source);

impl SourceMarker {
    /// Marker for the primary flat-array source.
    #[must_use]
    pub const fn primary() -> Self {
        Self(Source::Primary)
    }

    /// Marker for Steam's wrapped app-list source.
    #[must_use]
    pub const fn steam() -> Self {
        Self(Source::Steam)
    }
}

fn insert_name(names: &mut BTreeMap<u32, String>, app_id: u32, name: String) {
    let name = name.trim();
    if app_id > 0 && !name.is_empty() {
        names.insert(app_id, name.to_owned());
    }
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

fn read_disk(path: &Path) -> Result<BTreeMap<u32, String>> {
    let bytes = std::fs::read(path)
        .map_err(|error| Error::io(format!("reading {}", path.display()), error))?;
    let raw: BTreeMap<String, String> = serde_json::from_slice(&bytes)
        .map_err(|error| Error::parse("app list", error.to_string()))?;
    let mut names = BTreeMap::new();
    for (raw_id, name) in raw {
        if let Ok(app_id) = raw_id.parse::<u32>() {
            insert_name(&mut names, app_id, name);
        }
    }
    Ok(names)
}

fn write_disk(path: &Path, names: &BTreeMap<u32, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
    }
    let raw: BTreeMap<String, &str> = names
        .iter()
        .map(|(id, name)| (id.to_string(), name.as_str()))
        .collect();
    let bytes = serde_json::to_vec(&raw)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, bytes)
        .map_err(|error| Error::io(format!("writing {}", temp.display()), error))?;
    std::fs::rename(&temp, path)
        .map_err(|error| Error::io(format!("renaming {}", path.display()), error))
}

#[derive(Debug, Deserialize)]
struct FlatEntry {
    #[serde(default)]
    appid: u32,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct SteamResponse {
    applist: SteamAppList,
}

#[derive(Debug, Deserialize)]
struct SteamAppList {
    #[serde(default)]
    apps: Vec<FlatEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_and_wrapped_shapes_parse() {
        let flat = parse_list(
            r#"[{"appid":10,"name":" Counter-Strike "},{"appid":0,"name":"bad"},{"appid":20,"name":""}]"#,
            SourceMarker::primary(),
        )
        .expect("flat list");
        assert_eq!(flat.get(&10).map(String::as_str), Some("Counter-Strike"));
        assert_eq!(flat.len(), 1);

        let wrapped = parse_list(
            r#"{"applist":{"apps":[{"appid":730,"name":"CS2"}]}}"#,
            SourceMarker::steam(),
        )
        .expect("wrapped list");
        assert_eq!(wrapped.get(&730).map(String::as_str), Some("CS2"));
    }

    #[test]
    fn malformed_shapes_are_errors() {
        assert!(parse_list("not json", SourceMarker::primary()).is_err());
        assert!(parse_list("[]", SourceMarker::steam()).is_err());
    }
}
