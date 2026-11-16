//! Public depot, branch, and DLC metadata from `api.steamcmd.net`.
//!
//! This is display metadata only. It never requests depot keys and cannot fetch depot content.
//! Successful lookups are cached for the session; failures are retried after 60 seconds.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use luatools_core::config::STEAMCMD_INFO_BASE;
use luatools_core::{AppDepotInfo, AppId, ContentDepot, DepotId, Error, ManifestId, Result};
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::Value;

/// How long a failed lookup is remembered.
pub const FAILURE_TTL: Duration = Duration::from_secs(60);

/// Memory-only depot metadata client.
#[derive(Debug, Clone)]
pub struct SteamDepotInfo {
    client: Client,
    base: String,
    cache: std::sync::Arc<Mutex<HashMap<u32, CacheEntry>>>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    info: Option<AppDepotInfo>,
    at: Instant,
}

impl SteamDepotInfo {
    /// Builds a client against the public metadata service.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Network`] when the HTTP client cannot be constructed.
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| Error::Network {
                context: "building depot metadata client".to_owned(),
                message: error.to_string(),
            })?;
        Ok(Self::with_client(client))
    }

    /// Builds a client with a caller-supplied HTTP client, for tests and proxies.
    #[must_use]
    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            base: STEAMCMD_INFO_BASE.to_owned(),
            cache: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Overrides the metadata base URL, primarily for tests.
    #[must_use]
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_owned();
        self
    }

    /// Returns depot metadata, or `None` when the service has nothing usable.
    pub async fn get(&self, app: AppId) -> Option<AppDepotInfo> {
        if let Some(entry) = self.cache.lock().get(&app.get()).cloned() {
            let fresh = entry.info.is_some() || entry.at.elapsed() < FAILURE_TTL;
            if fresh {
                return entry.info;
            }
        }

        let url = format!("{}/{}", self.base, app.get());
        let info = match self.fetch(&url, app).await {
            Ok(info) => info,
            Err(error) => {
                tracing::debug!(%error, app = app.get(), "depot metadata lookup failed");
                None
            }
        };
        self.cache.lock().insert(
            app.get(),
            CacheEntry {
                info: info.clone(),
                at: Instant::now(),
            },
        );
        info
    }

    /// Returns a previously cached success without touching the network.
    #[must_use]
    pub fn try_cached(&self, app: AppId) -> Option<AppDepotInfo> {
        self.cache
            .lock()
            .get(&app.get())
            .and_then(|entry| entry.info.clone())
    }

    /// Drops one cache entry so the next call refetches.
    pub fn invalidate(&self, app: AppId) {
        self.cache.lock().remove(&app.get());
    }

    async fn fetch(&self, url: &str, app: AppId) -> Result<Option<AppDepotInfo>> {
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
        parse_app_info(&body, app)
    }
}

/// Parses one app's depot metadata.
///
/// Missing apps and missing depot blocks yield an empty [`AppDepotInfo`]; non-numeric depot keys
/// are skipped. A structurally invalid body is a parse error.
///
/// # Errors
///
/// Returns [`Error::Parse`] when the body is not the expected JSON object.
pub fn parse_app_info(body: &str, app: AppId) -> Result<Option<AppDepotInfo>> {
    let root: Value = serde_json::from_str(body)
        .map_err(|error| Error::parse("steamcmd info", error.to_string()))?;
    let Some(data) = root.get("data").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(entry) = data.get(&app.get().to_string()) else {
        return Ok(None);
    };
    let Some(entry) = entry.as_object() else {
        return Ok(None);
    };

    let mut depots = Vec::new();
    if let Some(table) = entry.get("depots").and_then(Value::as_object) {
        for (raw_id, value) in table {
            let Ok(id) = raw_id.parse::<u32>() else {
                continue;
            };
            let Some(value) = value.as_object() else {
                continue;
            };
            let config = value.get("config").and_then(Value::as_object);
            let manifests = value.get("manifests").and_then(Value::as_object);
            let public = manifests
                .and_then(|manifests| manifests.get("public"))
                .and_then(Value::as_object);
            let size = public
                .and_then(|public| public.get("size"))
                .and_then(Value::as_str)
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let public_manifest_id = public
                .and_then(|public| public.get("gid"))
                .and_then(value_as_id)
                .and_then(|raw| raw.parse::<u64>().ok())
                .map(ManifestId::from);
            let dlc_app_id = value
                .get("dlcappid")
                .and_then(Value::as_str)
                .and_then(|raw| raw.trim().parse::<u32>().ok())
                .map(AppId::from);
            let from_app_id = value
                .get("depotfromapp")
                .and_then(Value::as_str)
                .and_then(|raw| raw.trim().parse::<u32>().ok())
                .map(AppId::from);
            let os = config
                .and_then(|config| config.get("oslist"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let language = config
                .and_then(|config| config.get("dlclanguage"))
                .or_else(|| config.and_then(|config| config.get("language")))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            depots.push(ContentDepot {
                id: DepotId::new(id),
                size,
                dlc_app_id,
                is_shared: value.contains_key("depotfromapp"),
                os,
                language,
                public_manifest_id,
                from_app_id,
            });
        }
    }

    let public_build_id = entry
        .get("depots")
        .and_then(Value::as_object)
        .and_then(|depots| depots.get("branches"))
        .and_then(Value::as_object)
        .and_then(|branches| branches.get("public"))
        .and_then(Value::as_object)
        .and_then(|public| public.get("buildid"))
        .and_then(value_as_id);

    let dlc_ids = entry
        .get("extended")
        .and_then(Value::as_object)
        .and_then(|extended| extended.get("listofdlc"))
        .and_then(Value::as_str)
        .map(|csv| {
            csv.split(',')
                .filter_map(|part| part.trim().parse::<u32>().ok())
                .map(AppId::from)
                .collect()
        })
        .unwrap_or_default();

    let launch_exes = parse_launch_exes(entry.get("config"));

    Ok(Some(AppDepotInfo {
        app_id: app,
        depots,
        dlc_ids,
        launch_exes,
        public_build_id,
    }))
}

fn value_as_id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn parse_launch_exes(config: Option<&Value>) -> Vec<String> {
    let Some(launch) = config
        .and_then(Value::as_object)
        .and_then(|config| config.get("launch"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };

    let mut defaults = Vec::new();
    let mut others = Vec::new();
    for entry in launch.values() {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(executable) = entry.get("executable").and_then(Value::as_str) else {
            continue;
        };
        let executable = executable.trim();
        if executable.is_empty() {
            continue;
        }
        let launch_config = entry.get("config").and_then(Value::as_object);
        let windows = launch_config
            .and_then(|config| config.get("oslist"))
            .and_then(Value::as_str)
            .is_none_or(|os| os.to_ascii_lowercase().contains("windows"));
        let has_beta = launch_config
            .and_then(|config| config.get("betakey"))
            .and_then(Value::as_str)
            .is_some_and(|key| !key.trim().is_empty());
        let has_owns_dlc = launch_config.is_some_and(|config| config.contains_key("ownsdlc"));
        if !windows || has_beta || has_owns_dlc {
            continue;
        }
        let normalized = executable.replace('/', "\\");
        let is_default = entry
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("default"));
        if is_default {
            defaults.push(normalized);
        } else {
            others.push(normalized);
        }
    }
    let mut seen = std::collections::HashSet::new();
    defaults
        .into_iter()
        .chain(others)
        .filter(|exe| seen.insert(exe.to_ascii_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "data": {
            "386940": {
                "depots": {
                    "228983": {
                        "config": {"oslist": "windows", "language": "english"},
                        "manifests": {"public": {"gid": "999", "size": "12345"}},
                        "dlcappid": "1234",
                        "depotfromapp": "228980"
                    },
                    "228984": {
                        "config": {"oslist": "linux", "dlclanguage": "german"},
                        "manifests": {"public": {"gid": 1000, "size": "42"}}
                    },
                    "branches": {"public": {"buildid": 7777}},
                    "baselanguages": "english"
                },
                "extended": {"listofdlc": "1234, 5678, nope"},
                "config": {
                    "launch": {
                        "0": {"executable": "Game.exe", "type": "default"},
                        "1": {"executable": "Game64.exe", "config": {"oslist": "windows"}},
                        "2": {"executable": "mac.app", "config": {"oslist": "macos"}},
                        "3": {"executable": "Beta.exe", "config": {"betakey": "beta"}},
                        "4": {"executable": "Tool.exe", "config": {"ownsdlc": ""}}
                    }
                }
            }
        },
        "status": "success"
    }"#;

    #[test]
    fn parses_depots_dlc_and_launch_exes() {
        let info = parse_app_info(SAMPLE, AppId::new(386_940))
            .expect("valid body")
            .expect("app exists");
        assert_eq!(info.depots.len(), 2);
        let first = info.depot(DepotId::new(228_983)).expect("first depot");
        assert_eq!(first.size, 12_345);
        assert_eq!(first.dlc_app_id, Some(AppId::new(1234)));
        assert_eq!(first.from_app_id, Some(AppId::new(228_980)));
        assert!(first.is_shared);
        assert_eq!(first.public_manifest_id, Some(ManifestId::new(999)));
        let second = info.depot(DepotId::new(228_984)).expect("second depot");
        assert_eq!(second.language.as_deref(), Some("german"));
        assert_eq!(second.public_manifest_id, Some(ManifestId::new(1000)));
        assert_eq!(info.public_build_id.as_deref(), Some("7777"));
        assert_eq!(info.dlc_ids, vec![AppId::new(1234), AppId::new(5678)]);
        assert_eq!(info.launch_exes, vec!["Game.exe", "Game64.exe"]);
    }

    #[test]
    fn missing_app_is_none_and_invalid_json_is_an_error() {
        assert!(
            parse_app_info(r#"{"data":{}}"#, AppId::new(1))
                .unwrap()
                .is_none()
        );
        assert!(parse_app_info("nope", AppId::new(1)).is_err());
    }
}
