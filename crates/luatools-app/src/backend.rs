//! Backend actor: owns the runtime and the services, consumes [`Command`]s, emits [`Event`]s.
//!
//! The UI never touches Steam, HTTP, or the filesystem. It sends commands to this actor, which
//! owns the runtime and the caches. Steam work happens on the actor task; progress and results
//! leave as [`Event`]s.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use luatools_core::{
    AppDepotInfo, AppDetails, AppEntry, AppId, AppMetadata, AppPaths, AuthState, BackendHandle,
    Command, ContentDepot, DepotEntry, DepotId, Event, Featured, FeaturedItem, InstalledState,
    LaunchOption, LibrarySnapshot, LogLevel, NavTarget, PluginStatus, Settings, Toast, UpdateState,
};
use luatools_downloads::{
    DownloadQueue, HistoryStore, HttpClientTransfer, QueueConfig, QueueHandle,
};
use luatools_net::appdetails::AppDetailsCache;
use luatools_net::applist::AppListCache;
use luatools_net::depot_info::SteamDepotInfo;
use luatools_net::hardware::HardwareAppIdService;
use luatools_net::http::{HttpClientConfig, build as build_http};
use luatools_net::plugin::PluginInstaller;
use luatools_net::update::GitHubReleases;
use luatools_net::{ApiClient, AuthClient, AuthConfig, DohResolver, FileSecretStore, SecretStore};
use luatools_steam::{
    AppInfoCache, CoverCache, LaunchModStore, SteamLibrary, active_localconfig,
    apply_launch_options, discover_libraries, find_steam_root, parse_steam_link,
    read_launch_options, read_user_launch_options, write_launch_entries,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::cef::{CefBridge, CefInjector};
use crate::platform;
use crate::plugin_api::{PluginState, start as start_plugin_api};
use crate::plugin_installer;
use crate::settings_store::SettingsStore;

/// Repositories checked for updates, primary first.
const UPDATE_REPOS: [&str; 1] = ["meding2904/LuatoolsRS"];

/// Everything the actor needs to start.
pub struct BackendConfig {
    /// Filesystem layout.
    pub paths: AppPaths,
    /// Persisted settings.
    pub settings: Settings,
    /// Initial protocol URL or pending URL.
    pub startup_url: Option<String>,
}

/// Long-lived network and cache services.
struct Services {
    http: reqwest::Client,
    api: ApiClient,
    details: Arc<AppDetailsCache>,
    applist: Arc<AppListCache>,
    depots: Arc<SteamDepotInfo>,
    hardware: Arc<HardwareAppIdService>,
    covers: CoverCache,
}

impl Services {
    fn build(settings: &Settings, paths: &AppPaths) -> Self {
        let config = HttpClientConfig {
            timeout: Duration::from_secs(30),
            user_agent: luatools_core::config::USER_AGENT.to_owned(),
            proxy: settings.proxy_url.clone(),
            doh: settings.use_doh,
        };
        let http = build_http(&config).unwrap_or_else(|error| {
            tracing::warn!(%error, "configured HTTP client could not be built; using defaults");
            reqwest::Client::new()
        });
        let api = ApiClient::new(http.clone());
        let applist = Arc::new(AppListCache::new(
            http.clone(),
            paths.cache_dir().join("applist.json"),
        ));
        let details = Arc::new(AppDetailsCache::new(
            http.clone(),
            paths.cache_dir().join("details"),
        ));
        let depots = Arc::new(SteamDepotInfo::with_client(http.clone()));
        let hardware = Arc::new(HardwareAppIdService::new(
            http.clone(),
            paths.cache_dir().join("hardware.json"),
        ));
        Self {
            http,
            api,
            details,
            applist,
            depots,
            hardware,
            covers: CoverCache::new(paths.cache_dir().join("covers")),
        }
    }

    fn ensure_background(&self) {
        let applist = Arc::clone(&self.applist);
        let hardware = Arc::clone(&self.hardware);
        tokio::spawn(async move {
            applist.ensure_loaded().await;
        });
        tokio::spawn(async move {
            hardware.ensure_fresh().await;
        });
    }
}

/// Long-lived Steam caches owned by the actor.
struct SteamState {
    root: Option<PathBuf>,
    libraries: Vec<SteamLibrary>,
    appinfo: Option<AppInfoCache>,
    names: HashMap<AppId, String>,
    snapshot: LibrarySnapshot,
}

impl SteamState {
    fn empty() -> Self {
        Self {
            root: None,
            libraries: Vec::new(),
            appinfo: None,
            names: HashMap::new(),
            snapshot: LibrarySnapshot::default(),
        }
    }

    fn rescan(
        &mut self,
        override_path: Option<&Path>,
        extra_paths: &[PathBuf],
        services: &Services,
    ) -> luatools_core::Result<LibrarySnapshot> {
        self.root = find_steam_root(override_path);
        self.libraries.clear();
        self.names.clear();
        self.appinfo = None;

        let Some(root) = self.root.clone() else {
            self.snapshot = LibrarySnapshot::default();
            return Ok(self.snapshot.clone());
        };
        self.libraries = discover_libraries(&root)?;
        for extra in extra_paths {
            if extra.as_os_str().is_empty() {
                continue;
            }
            match discover_libraries(extra) {
                Ok(libraries) => {
                    for library in libraries {
                        if !self
                            .libraries
                            .iter()
                            .any(|existing| existing.root == library.root)
                        {
                            self.libraries.push(library);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, path = %extra.display(), "extra Steam library unavailable");
                }
            }
        }
        let appinfo_path = root.join("appcache").join("appinfo.vdf");
        if appinfo_path.is_file() {
            match AppInfoCache::open(&appinfo_path) {
                Ok(cache) => self.appinfo = Some(cache),
                Err(error) => {
                    tracing::warn!(%error, path = %appinfo_path.display(), "appinfo unavailable");
                }
            }
        }

        let installed: Vec<(AppId, String, PathBuf, u64, Option<String>)> = self
            .libraries
            .iter()
            .flat_map(|library| {
                library.apps.iter().filter(|m| m.installed).map(|m| {
                    (
                        m.app_id,
                        m.name.clone(),
                        m.install_path(&library.root),
                        m.size_on_disk,
                        m.build_id.clone(),
                    )
                })
            })
            .collect();

        let mut apps = Vec::with_capacity(installed.len());
        for (id, fallback, path, size_on_disk, build_id) in installed {
            let name = self.app_name(id, &fallback, services);
            let mut entry = AppEntry {
                id,
                name,
                installed: InstalledState::Installed {
                    path,
                    size_on_disk,
                    build_id,
                },
                cover_url: None,
                cover_path: None,
                release_date: None,
                developer: None,
                genres: Vec::new(),
                last_played: None,
                playtime_minutes: 0,
                metadata: None,
            };
            if let Some(metadata) = services.details.get_cached(id) {
                apply_metadata(&mut entry, &metadata);
            }
            if let Some(path) = local_cover(&root, &services.covers, id) {
                entry.cover_path = Some(path);
            }
            apps.push(entry);
        }
        apps.sort_by_key(|a| a.name.to_lowercase());
        apps.dedup_by_key(|a| a.id);

        self.snapshot = LibrarySnapshot {
            apps,
            steam_root: self.root.clone(),
            scanned_at: Some(Utc::now()),
        };
        Ok(self.snapshot.clone())
    }

    /// Resolves a display name from appinfo, the bulk list, the ACF name, then the id.
    fn app_name(&mut self, app_id: AppId, fallback: &str, services: &Services) -> String {
        if let Some(name) = self.names.get(&app_id) {
            return name.clone();
        }
        let mut resolved = services.applist.get_name(app_id);
        if resolved.is_none()
            && let Some(cache) = self.appinfo.as_mut()
            && let Ok(Some(info)) = cache.get(app_id)
        {
            let entry = info.entry();
            if !entry.name.trim().is_empty() {
                resolved = Some(entry.name.trim().to_owned());
            }
        }
        let name = resolved.unwrap_or_else(|| {
            if fallback.trim().is_empty() {
                app_id.to_string()
            } else {
                fallback.to_owned()
            }
        });
        self.names.insert(app_id, name.clone());
        name
    }

    fn installed_entry(
        &self,
        app_id: AppId,
    ) -> Option<(&SteamLibrary, &luatools_steam::AppManifest)> {
        self.libraries.iter().find_map(|library| {
            library
                .apps
                .iter()
                .find(|m| m.app_id == app_id && m.installed)
                .map(|m| (library, m))
        })
    }

    fn app_state(&self, app_id: AppId) -> InstalledState {
        self.installed_entry(app_id)
            .map_or(InstalledState::NotInstalled, |(library, manifest)| {
                InstalledState::Installed {
                    path: manifest.install_path(&library.root),
                    size_on_disk: manifest.size_on_disk,
                    build_id: manifest.build_id.clone(),
                }
            })
    }

    fn details(
        &mut self,
        app_id: AppId,
        services: &Services,
        selections: &DepotSelectionStore,
    ) -> Option<AppDetails> {
        let mut app = self.snapshot.app(app_id).cloned().or_else(|| {
            let installed = self.app_state(app_id);
            if !installed.is_installed() && services.applist.get_name(app_id).is_none() {
                return None;
            }
            Some(AppEntry {
                id: app_id,
                name: self.app_name(app_id, "", services),
                installed,
                cover_url: None,
                cover_path: None,
                release_date: None,
                developer: None,
                genres: Vec::new(),
                last_played: None,
                playtime_minutes: 0,
                metadata: None,
            })
        })?;

        let (launch_entries, depots) = self.read_app_metadata(app_id, services, selections);
        let launch_options = self
            .root
            .as_deref()
            .and_then(active_localconfig)
            .and_then(|path| read_user_launch_options(&path, app_id).ok().flatten())
            .unwrap_or_default();

        if let Some(metadata) = services.details.get_cached(app_id) {
            apply_metadata(&mut app, &metadata);
        }
        if app.cover_path.is_none()
            && let Some(root) = self.root.as_deref()
        {
            app.cover_path = local_cover(root, &services.covers, app_id);
        }

        Some(AppDetails {
            app,
            depots,
            launch_options,
            launch_entries,
            metadata: services.details.get_cached(app_id),
            depot_info: None,
            install_dir: self
                .installed_entry(app_id)
                .map(|(_, m)| m.install_dir.clone()),
            library_found: self.root.is_some(),
        })
    }

    fn store_details(
        &mut self,
        app_id: AppId,
        metadata: AppMetadata,
        services: &Services,
    ) -> AppDetails {
        let installed = self.app_state(app_id);
        let name = metadata
            .name
            .clone()
            .or_else(|| services.applist.get_name(app_id))
            .unwrap_or_else(|| app_id.to_string());
        let mut app = AppEntry {
            id: app_id,
            name,
            installed,
            cover_url: metadata.header_image.clone(),
            cover_path: None,
            release_date: metadata.release_date.clone(),
            developer: (!metadata.developers.is_empty()).then(|| metadata.developer_label()),
            genres: metadata.genres.clone(),
            last_played: None,
            playtime_minutes: 0,
            metadata: Some(metadata.clone()),
        };
        if app.cover_path.is_none()
            && let Some(root) = self.root.as_deref()
        {
            app.cover_path = local_cover(root, &services.covers, app_id);
        }
        AppDetails {
            app,
            depots: Vec::new(),
            launch_options: String::new(),
            launch_entries: Vec::new(),
            metadata: Some(metadata),
            depot_info: None,
            install_dir: None,
            library_found: self.root.is_some(),
        }
    }

    fn read_app_metadata(
        &mut self,
        app_id: AppId,
        services: &Services,
        selections: &DepotSelectionStore,
    ) -> (Vec<LaunchOption>, Vec<DepotEntry>) {
        let mut launch = Vec::new();
        let mut depots = Vec::new();
        if let Some(cache) = self.appinfo.as_mut()
            && let Ok(Some(info)) = cache.get(app_id)
            && let Some(body) = info.body().cloned()
        {
            launch = read_launch_options(&body);
            if let Some(table) = body.get_table("depots") {
                for property in table.items() {
                    let Ok(depot_id) = property.name.parse::<u32>() else {
                        continue;
                    };
                    let name = property
                        .value
                        .as_table()
                        .and_then(|t| t.get_text("name"))
                        .unwrap_or_else(|| format!("Depot {depot_id}"));
                    depots.push(DepotEntry::new(DepotId::new(depot_id), name));
                }
            }
        }
        if let Some(info) = services.depots.try_cached(app_id) {
            merge_depot_info(&mut depots, &info);
        }
        for depot in &mut depots {
            if let Some(enabled) = selections.is_enabled(app_id, depot.id) {
                depot.enabled = enabled;
            }
        }
        depots.sort_by_key(|d| d.id);
        (launch, depots)
    }
}

impl SteamState {
    fn find_drifted(&mut self, mod_store: &LaunchModStore) -> Vec<AppId> {
        let Ok(mods) = mod_store.all() else {
            return Vec::new();
        };
        if mods.is_empty() {
            return Vec::new();
        }
        let Some(cache) = self.appinfo.as_mut() else {
            return Vec::new();
        };
        let mut drifted = Vec::new();
        for launch_mod in mods {
            let Ok(Some(info)) = cache.get(launch_mod.app_id) else {
                continue;
            };
            let body = info.body().unwrap_or(&info.data);
            let live = read_launch_options(body);
            if !launch_options_equal(&live, &launch_mod.desired) {
                drifted.push(launch_mod.app_id);
            }
        }
        drifted
    }
}

fn launch_options_equal(left: &[LaunchOption], right: &[LaunchOption]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.executable == right.executable
                && left.arguments == right.arguments
                && left.working_dir == right.working_dir
                && left.description == right.description
                && left.option_type == right.option_type
                && left.os_list == right.os_list
                && left.os_arch == right.os_arch
                && left.beta_key == right.beta_key
                && left.owns_dlc == right.owns_dlc
        })
}

fn check_launch_drift(
    steam: &mut SteamState,
    mod_store: &LaunchModStore,
    events: &UnboundedSender<Event>,
) {
    let drifted = steam.find_drifted(mod_store);
    let _ = events.send(Event::LaunchOptionsDrifted(drifted));
}

fn apply_metadata(entry: &mut AppEntry, metadata: &AppMetadata) {
    if let Some(name) = metadata.name.as_deref()
        && !name.trim().is_empty()
    {
        entry.name = name.trim().to_owned();
    }
    entry.cover_url = metadata.header_image.clone();
    entry.release_date = metadata.release_date.clone();
    entry.developer = (!metadata.developers.is_empty()).then(|| metadata.developer_label());
    entry.genres = metadata.genres.clone();
    entry.metadata = Some(metadata.clone());
}

fn local_cover(root: &Path, cache: &CoverCache, app: AppId) -> Option<PathBuf> {
    CoverCache::discover_on_disk(root, app).or_else(|| cache.local_path(app))
}

fn merge_depot_info(depots: &mut Vec<DepotEntry>, info: &AppDepotInfo) {
    for content in &info.depots {
        if let Some(existing) = depots.iter_mut().find(|depot| depot.id == content.id) {
            apply_content_depot(existing, content, info);
            continue;
        }
        let mut depot = DepotEntry::new(content.id, format!("Depot {}", content.id.get()));
        apply_content_depot(&mut depot, content, info);
        depots.push(depot);
    }
}

fn apply_content_depot(entry: &mut DepotEntry, content: &ContentDepot, info: &AppDepotInfo) {
    entry.size = (content.size > 0).then_some(content.size);
    entry.public_manifest_id = content.public_manifest_id;
    entry.os = content.os.clone();
    entry.language = content.language.clone();
    entry.dlc_app_id = content.dlc_app_id;
    entry.from_app_id = content.from_app_id;
    entry.is_shared = content.is_shared;
    entry.build_id = info.public_build_id.clone();
    if content.is_dlc() {
        entry.optional = true;
    }
}

/// Persisted depot enable/disable preferences.
struct DepotSelectionStore {
    path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DepotSelections {
    #[serde(default)]
    apps: HashMap<String, Vec<u32>>,
}

impl DepotSelectionStore {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn load(&self) -> DepotSelections {
        std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn get(&self, app: AppId) -> Option<HashSet<DepotId>> {
        self.load()
            .apps
            .get(&app.get().to_string())
            .map(|ids| ids.iter().copied().map(DepotId::from).collect())
    }

    fn is_enabled(&self, app: AppId, depot: DepotId) -> Option<bool> {
        self.get(app).map(|enabled| enabled.contains(&depot))
    }

    fn save(&self, app: AppId, enabled: &[DepotId]) -> luatools_core::Result<()> {
        let mut selections = self.load();
        selections.apps.insert(
            app.get().to_string(),
            enabled.iter().map(|depot| depot.get()).collect(),
        );
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| luatools_core::Error::io(parent.display().to_string(), error))?;
        }
        let bytes = serde_json::to_vec_pretty(&selections)?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, bytes)
            .map_err(|error| luatools_core::Error::io(temp.display().to_string(), error))?;
        std::fs::rename(&temp, &self.path)
            .map_err(|error| luatools_core::Error::io(self.path.display().to_string(), error))
    }
}

/// Formats structured launch entries as one `executable args` line each.
#[cfg(test)]
#[must_use]
pub fn launch_options_text(options: &[LaunchOption]) -> String {
    options
        .iter()
        .map(|option| {
            let args = option.arguments.trim();
            if args.is_empty() {
                option.executable.clone()
            } else {
                format!("{} {args}", option.executable)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses edited launch lines back into full options, keeping fields the text does not show.
#[cfg(test)]
#[must_use]
pub fn parse_launch_options_text(text: &str, original: &[LaunchOption]) -> Vec<LaunchOption> {
    let template = original.first().cloned().unwrap_or_default();
    let mut options = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut option = original
            .get(index)
            .cloned()
            .unwrap_or_else(|| template.clone());
        let (executable, arguments) = match line.split_once(char::is_whitespace) {
            Some((executable, arguments)) => (executable, arguments.trim()),
            None => (line, ""),
        };
        option.executable = executable.to_owned();
        option.arguments = arguments.to_owned();
        options.push(option);
    }
    options
}

/// Spawns the backend thread and returns the UI-side handle and event stream.
///
/// # Panics
///
/// Panics only when the OS refuses to create a thread, which is unrecoverable at startup.
pub fn spawn(
    config: BackendConfig,
) -> luatools_core::Result<(BackendHandle, UnboundedReceiver<Event>)> {
    let (command_tx, command_rx) = unbounded_channel();
    let (event_tx, event_rx) = unbounded_channel();
    let handle = BackendHandle::new(command_tx);

    std::thread::Builder::new()
        .name("luatools-backend".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "could not start the backend runtime");
                    return;
                }
            };
            runtime.block_on(run(config, command_rx, event_tx));
        })
        .map_err(|error| luatools_core::Error::Other(format!("backend thread: {error}")))?;

    Ok((handle, event_rx))
}

async fn run(
    config: BackendConfig,
    mut commands: UnboundedReceiver<Command>,
    events: UnboundedSender<Event>,
) {
    let settings_store = SettingsStore::new(config.paths.settings_file());
    let mut settings = config.settings.clone();
    if settings.installation_id.is_empty() {
        settings.installation_id = fresh_installation_id();
        if let Err(error) = settings_store.save(&settings) {
            tracing::warn!(%error, "could not persist the installation id");
        }
    }
    let selections = DepotSelectionStore::new(config.paths.config_dir().join("depots.json"));
    let mod_store = LaunchModStore::new(config.paths.data_dir().join("launchmods.json"));
    let mut services = Services::build(&settings, &config.paths);
    services.ensure_background();
    let mut steam = SteamState::empty();
    let plugin_state = Arc::new(parking_lot::RwLock::new(PluginState::default()));
    let server_shutdown = tokio_util::sync::CancellationToken::new();
    let server_port = start_plugin_api(
        &settings,
        Arc::clone(&plugin_state),
        server_shutdown.clone(),
    )
    .await
    .unwrap_or(0);

    if settings.analytics_enabled {
        let http = services.http.clone();
        tokio::spawn(async move {
            track_launch(&http).await;
        });
    }

    let queue_config = QueueConfig::new(
        settings.effective_concurrency(),
        config.paths.cache_dir().join("staging"),
    )
    .with_speed_limit(settings.speed_limit_bytes)
    .with_verify(settings.verify_after_download);
    let queue = match build_blocking_client(&settings) {
        Some(client) => {
            let transfer = Arc::new(
                HttpClientTransfer::with_client(queue_config.staging_dir.clone(), client)
                    .with_speed_limit(queue_config.speed_limit_bytes),
            );
            DownloadQueue::spawn_with(
                queue_config,
                events.clone(),
                Some(HistoryStore::new(config.paths.history_file())),
                transfer,
            )
        }
        None => DownloadQueue::spawn(
            queue_config,
            events.clone(),
            Some(HistoryStore::new(config.paths.history_file())),
        ),
    };
    hydrate_history(&queue, &events).await;

    let mut plugin: Arc<dyn PluginInstaller> = Arc::from(plugin_installer::installer_with_root(
        settings.steam_path.as_deref(),
    ));
    {
        let plugin = Arc::clone(&plugin);
        let events = events.clone();
        tokio::task::spawn_blocking(move || match plugin.status() {
            Ok(status) => {
                let _ = events.send(Event::PluginStatus(status));
            }
            Err(_) => {
                let _ = events.send(Event::PluginStatus(PluginStatus::NotInstalled));
            }
        });
    }
    let _ = events.send(Event::SettingsSaved(Box::new(settings.clone())));
    let _ = events.send(Event::UpdateState(UpdateState::Idle));

    // Restore a stored session as a guest-safe best effort.
    let secret_store = Arc::new(FileSecretStore::new(
        config.paths.data_dir().join("auth.json"),
    ));
    let auth_client = AuthConfig::from_env()
        .map(|auth_config| Arc::new(AuthClient::new(services.http.clone(), auth_config)));
    let auth_cancel: Arc<parking_lot::Mutex<Option<tokio_util::sync::CancellationToken>>> =
        Arc::new(parking_lot::Mutex::new(None));
    restore_auth(
        Arc::clone(&secret_store),
        auth_client.clone(),
        events.clone(),
    );

    if let Some(url) = config.startup_url {
        handle_url(&url, &mut steam, &services, &selections, &events).await;
    }

    refresh_library(&mut steam, &services, &settings, &plugin_state, &events);
    check_launch_drift(&mut steam, &mod_store, &events);

    let pending_update: Arc<parking_lot::Mutex<Option<luatools_net::update::Release>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let cef_bridge = Arc::new(BackendCefBridge {
        events: events.clone(),
        plugin_state: Arc::clone(&plugin_state),
        http: services.http.clone(),
        pending_update: Arc::clone(&pending_update),
    });
    let _cef = CefInjector::start(
        config.paths.clone(),
        server_port,
        cef_bridge,
        server_shutdown.clone(),
    );
    if settings.check_updates {
        let events = events.clone();
        let http = services.http.clone();
        let pending_update = Arc::clone(&pending_update);
        tokio::spawn(async move {
            if let Some(release) = update_check(&http, &events).await {
                let current = env!("CARGO_PKG_VERSION").to_owned();
                let state = release.map_or(
                    UpdateState::UpToDate {
                        current: current.clone(),
                    },
                    |release| {
                        *pending_update.lock() = Some(release.clone());
                        UpdateState::Available {
                            current,
                            latest: release.version,
                            notes: release.notes,
                        }
                    },
                );
                let _ = events.send(Event::UpdateState(state));
            }
        });
    }

    let pending_path = pending_url_path(&config.paths);
    let mut interval = tokio::time::interval(Duration::from_millis(750));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                handle_command(
                    command,
                    &mut steam,
                    &mut services,
                    &mut settings,
                    &selections,
                    &settings_store,
                    &mod_store,
                    &queue,
                    &config.paths,
                    &plugin_state,
                    &secret_store,
                    &auth_client,
                    &auth_cancel,
                    &pending_update,
                    &mut plugin,
                    &events,
                ).await;
            }
            _ = interval.tick() => {
                if let Some(url) = take_pending(&pending_path) {
                    handle_url(&url, &mut steam, &services, &selections, &events).await;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    command: Command,
    steam: &mut SteamState,
    services: &mut Services,
    settings: &mut Settings,
    selections: &DepotSelectionStore,
    settings_store: &SettingsStore,
    mod_store: &LaunchModStore,
    queue: &QueueHandle,
    paths: &AppPaths,
    plugin_state: &Arc<parking_lot::RwLock<PluginState>>,
    secret_store: &Arc<FileSecretStore>,
    auth_client: &Option<Arc<AuthClient>>,
    auth_cancel: &Arc<parking_lot::Mutex<Option<tokio_util::sync::CancellationToken>>>,
    pending_update: &Arc<parking_lot::Mutex<Option<luatools_net::update::Release>>>,
    plugin: &mut Arc<dyn PluginInstaller>,
    events: &UnboundedSender<Event>,
) {
    match command {
        Command::RefreshLibrary => {
            refresh_library(steam, services, settings, plugin_state, events);
            check_launch_drift(steam, mod_store, events);
        }
        Command::LoadAppDetails(app_id) => {
            if steam.libraries.is_empty()
                && let Ok(snapshot) = steam.rescan(
                    settings.steam_path.as_deref(),
                    &settings.extra_library_paths,
                    services,
                )
            {
                let _ = events.send(Event::LibraryRefreshed(Box::new(snapshot)));
            }
            match steam.details(app_id, services, selections) {
                Some(details) => {
                    let _ = events.send(Event::AppDetailsLoaded(Box::new(details)));
                    spawn_metadata_and_depots(app_id, services.clone_for_tasks(), events);
                }
                None => {
                    let _ = events.send(Event::Error {
                        context: "load app".to_owned(),
                        message: format!("app {app_id} was not found in any Steam library"),
                    });
                }
            }
        }
        Command::SearchStore(query) => {
            let services = services.clone_for_tasks();
            let events = events.clone();
            let query = query.trim().to_owned();
            if query.is_empty() {
                let _ = events.send(Event::StoreSearchResults {
                    query,
                    items: Vec::new(),
                });
                return;
            }
            tokio::spawn(async move {
                match services.api.search_store(&query).await {
                    Ok(items) => {
                        let items = filter_hardware(items, &services.hardware, &|item| item.app_id);
                        let _ = events.send(Event::StoreSearchResults { query, items });
                    }
                    Err(error) => {
                        let _ = events.send(Event::StoreSearchResults {
                            query: query.clone(),
                            items: Vec::new(),
                        });
                        let _ = events.send(Event::Error {
                            context: "store search".to_owned(),
                            message: error.to_string(),
                        });
                    }
                }
            });
        }
        Command::LoadFeatured => {
            let services = services.clone_for_tasks();
            let events = events.clone();
            tokio::spawn(async move {
                match services.api.featured().await {
                    Ok(featured) => {
                        let _ = events.send(Event::FeaturedLoaded(Box::new(filter_featured(
                            featured,
                            &services.hardware,
                        ))));
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error {
                            context: "featured store".to_owned(),
                            message: error.to_string(),
                        });
                    }
                }
            });
        }
        Command::LoadStoreDetails(app_id) => match services.details.resolve(app_id).await {
            Some(metadata) => {
                let cover_url = metadata.header_image.clone();
                let mut details = steam.store_details(app_id, metadata, services);
                if let Some(url) = cover_url.as_deref()
                    && let Some(path) =
                        download_cover(&services.http, &services.covers, app_id, url).await
                {
                    details.app.cover_path = Some(path.clone());
                    let _ = events.send(Event::CoverUpdated { app_id, path });
                }
                let _ = events.send(Event::StoreDetailsLoaded(Box::new(details)));
            }
            None => {
                let _ = events.send(Event::Error {
                    context: "store details".to_owned(),
                    message: format!("Steam has no metadata for app {app_id}"),
                });
            }
        },
        Command::LoadDepotInfo(app_id) => {
            services.depots.invalidate(app_id);
            let services = services.clone_for_tasks();
            let events = events.clone();
            tokio::spawn(async move {
                if let Some(info) = services.depots.get(app_id).await {
                    let _ = events.send(Event::DepotInfoLoaded(Box::new(info)));
                }
            });
        }
        Command::RefreshAppMetadata(app_id) => {
            let services = services.clone_for_tasks();
            let events = events.clone();
            tokio::spawn(async move {
                if let Some(metadata) = services.details.refresh(app_id).await {
                    let _ = events.send(Event::AppMetadataLoaded {
                        app_id,
                        metadata: Box::new(metadata),
                    });
                }
            });
        }
        Command::SaveLaunchOptions { app_id, options } => {
            if steam.root.is_none() {
                steam.root = find_steam_root(settings.steam_path.as_deref());
            }
            let applied = match steam.root.as_deref() {
                Some(root) => apply_launch_options(root, app_id, &options),
                None => Ok(None),
            };
            match applied {
                Ok(Some(_)) => {
                    let _ = events.send(Event::LaunchOptionsSaved(app_id));
                    let _ = events.send(Event::Toast(Toast::success(
                        "Launch options written; restart Steam to apply them",
                    )));
                }
                Ok(None) => {
                    let _ = events.send(Event::LaunchOptionsSaved(app_id));
                    let _ = events.send(Event::Toast(Toast::info(
                        "Launch options staged; no Steam account config was found",
                    )));
                }
                Err(error) => {
                    let _ = events.send(Event::Error {
                        context: "apply launch options".to_owned(),
                        message: error.to_string(),
                    });
                }
            }
        }
        Command::SaveLaunchEntries { app_id, entries } => {
            save_launch_entries(steam, mod_store, app_id, &entries, events);
        }
        Command::RestoreLaunchOptions(app_id) => {
            restore_launch_entries(steam, mod_store, app_id, events);
        }
        Command::ReapplyLaunchOptions => {
            let Some(root) = steam.root.clone() else {
                let _ = events.send(Event::Error {
                    context: "re-apply launch options".to_owned(),
                    message: "Steam was not found".to_owned(),
                });
                return;
            };
            ensure_appinfo(steam, &root);
            let mods = match mod_store.all() {
                Ok(mods) => mods,
                Err(error) => {
                    let _ = events.send(Event::Error {
                        context: "re-apply launch options".to_owned(),
                        message: error.to_string(),
                    });
                    return;
                }
            };
            if mods.is_empty() {
                let _ = events.send(Event::Toast(Toast::info(
                    "There are no staged launch edits to re-apply",
                )));
                return;
            }
            let edits: HashMap<AppId, Vec<LaunchOption>> = mods
                .into_iter()
                .map(|launch_mod| (launch_mod.app_id, launch_mod.desired))
                .collect();
            match write_appinfo_launch_edits(steam, &root, &edits) {
                Ok(()) => {
                    let _ = events.send(Event::LaunchOptionsDrifted(Vec::new()));
                    let _ = events.send(Event::Toast(Toast::success(
                        "Steam launch edits were re-applied to appinfo.vdf",
                    )));
                }
                Err(error) => {
                    let _ = events.send(Event::Error {
                        context: "re-apply launch options".to_owned(),
                        message: error.to_string(),
                    });
                }
            }
        }
        Command::EnqueueDownload(request) => {
            if !queue.enqueue(*request) {
                let _ = events.send(Event::Error {
                    context: "queue".to_owned(),
                    message: "the download queue is not running".to_owned(),
                });
            }
        }
        Command::EnqueueUrlDownload { url, name } => {
            let url = url.trim().to_owned();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                let _ = events.send(Event::Error {
                    context: "queue".to_owned(),
                    message: "only http(s) file URLs can be queued".to_owned(),
                });
                return;
            }
            let request = luatools_core::DownloadRequest {
                app_id: AppId::default(),
                app_name: name
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| url.clone()),
                kind: luatools_core::DownloadKind::File,
                depots: Vec::new(),
                url: Some(url),
                destination: settings.download_dir.clone(),
                verify: settings.verify_after_download,
            };
            if !queue.enqueue(request) {
                let _ = events.send(Event::Error {
                    context: "queue".to_owned(),
                    message: "the download queue is not running".to_owned(),
                });
            }
        }
        Command::PauseDownload(id) => report_flag(queue.pause(id), "pause", events),
        Command::ResumeDownload(id) => report_flag(queue.resume(id), "resume", events),
        Command::CancelDownload(id) => report_flag(queue.cancel(id), "cancel", events),
        Command::RemoveDownload(id) => report_flag(queue.remove(id), "remove", events),
        Command::RetryDownload(id) => report_flag(queue.retry(id), "retry", events),
        Command::ClearDownloadHistory => {
            report_flag(queue.clear_history(), "clear history", events);
        }
        Command::SaveSettings(next) => {
            let next = *next;
            match settings_store.save(&next) {
                Ok(()) => {
                    let steam_path_changed = settings.steam_path != next.steam_path;
                    let library_changed = steam_path_changed
                        || settings.extra_library_paths != next.extra_library_paths;
                    let start_with_os_changed = settings.start_with_os != next.start_with_os;
                    let plugin_enabled_changed = settings.plugin_enabled != next.plugin_enabled;
                    let plugin_enabled = next.plugin_enabled;
                    *settings = next.clone();
                    *services = Services::build(settings, paths);
                    services.ensure_background();
                    if start_with_os_changed
                        && let Err(error) = platform::set_start_with_os(next.start_with_os)
                    {
                        let _ = events.send(Event::Error {
                            context: "start with the operating system".to_owned(),
                            message: error.to_string(),
                        });
                    }
                    let _ = events.send(Event::SettingsSaved(Box::new(next)));
                    let _ = events.send(Event::Toast(Toast::success("Settings saved")));
                    if steam_path_changed {
                        *plugin = Arc::from(plugin_installer::installer_with_root(
                            settings.steam_path.as_deref(),
                        ));
                    }
                    if library_changed {
                        refresh_library(steam, services, settings, plugin_state, events);
                    }
                    if plugin_enabled_changed {
                        spawn_plugin_toggle(Arc::clone(plugin), plugin_enabled, events.clone());
                    }
                }
                Err(error) => {
                    let _ = events.send(Event::Error {
                        context: "save settings".to_owned(),
                        message: error.to_string(),
                    });
                }
            }
        }
        Command::ReloadSettings => {
            let previous_path = settings.steam_path.clone();
            *settings = settings_store.load();
            *services = Services::build(settings, paths);
            services.ensure_background();
            if previous_path != settings.steam_path {
                *plugin = Arc::from(plugin_installer::installer_with_root(
                    settings.steam_path.as_deref(),
                ));
                refresh_library(steam, services, settings, plugin_state, events);
            }
            let _ = events.send(Event::SettingsSaved(Box::new(settings.clone())));
        }
        Command::OpenPath(path) => report(platform::open_path(&path), "open", events),
        Command::RevealPath(path) => report(platform::reveal_path(&path), "reveal", events),
        Command::OpenUrl(url) => report(platform::open_url(&url), "open url", events),
        Command::LaunchApp(app_id) => {
            report(platform::launch_steam_app(app_id.get()), "launch", events);
        }
        Command::InstallApp(app_id) => {
            report(platform::install_steam_app(app_id.get()), "install", events);
        }
        Command::OpenStorePage(app_id) => {
            report(
                platform::open_store_page(app_id.get()),
                "store page",
                events,
            );
        }
        Command::RestartSteam => {
            let _ = events.send(Event::Busy {
                key: "steam",
                active: true,
            });
            report(platform::restart_steam(), "restart steam", events);
            let _ = events.send(Event::Busy {
                key: "steam",
                active: false,
            });
        }
        Command::CheckForUpdates => {
            let _ = events.send(Event::UpdateState(UpdateState::Checking));
            let http = services.http.clone();
            if let Some(assets) = update_check(&http, events).await {
                let current = env!("CARGO_PKG_VERSION").to_owned();
                if let Some(release) = assets {
                    *pending_update.lock() = Some(release.clone());
                    let _ = events.send(Event::UpdateState(UpdateState::Available {
                        current,
                        latest: release.version,
                        notes: release.notes,
                    }));
                } else {
                    let _ = events.send(Event::UpdateState(UpdateState::UpToDate { current }));
                }
            }
        }
        Command::OpenUpdateFolder => {
            let version = pending_update
                .lock()
                .as_ref()
                .map(|release| release.version.clone());
            let Some(version) = version else {
                let _ = events.send(Event::Toast(Toast::info(
                    "No update is staged. Check for updates first.",
                )));
                return;
            };
            let version_dir: String = version
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            report(
                platform::open_path(&paths.data_dir().join("updates").join(version_dir)),
                "open update folder",
                events,
            );
        }
        Command::InstallUpdate => {
            let release = pending_update.lock().clone();
            let Some(release) = release else {
                let _ = events.send(Event::Toast(Toast::info(
                    "No update is staged. Check for updates first.",
                )));
                return;
            };
            let Some(asset) = update_asset(&release) else {
                let _ = events.send(Event::Error {
                    context: "update".to_owned(),
                    message: "the release has no asset for this platform".to_owned(),
                });
                return;
            };
            let Ok(name) = luatools_net::plugin::safe_asset_name(&asset.name) else {
                let _ = events.send(Event::Error {
                    context: "update".to_owned(),
                    message: "the release asset name is unsafe".to_owned(),
                });
                return;
            };
            let version_dir: String = release
                .version
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            let destination = paths
                .data_dir()
                .join("updates")
                .join(version_dir)
                .join(name);
            let download_url = asset.download_url.clone();
            let expected_digest = asset.digest.clone();
            let http = services.http.clone();
            let events = events.clone();
            let version = release.version.clone();
            tokio::spawn(async move {
                let _ = events.send(Event::UpdateState(UpdateState::Downloading {
                    progress: 0.0,
                }));
                let progress_events = events.clone();
                let result = luatools_net::github::download(
                    &http,
                    &download_url,
                    &destination,
                    move |received, total| {
                        let progress = total
                            .filter(|total| *total > 0)
                            .map_or(0.0, |total| received as f32 / total as f32)
                            .clamp(0.0, 1.0);
                        let _ = progress_events
                            .send(Event::UpdateState(UpdateState::Downloading { progress }));
                    },
                )
                .await;
                match result {
                    Ok(digest) => {
                        if let Some(expected) = expected_digest
                            .as_deref()
                            .and_then(|value| value.strip_prefix("sha256:"))
                            && !digest.as_str().eq_ignore_ascii_case(expected)
                        {
                            let _ = std::fs::remove_file(&destination);
                            let _ = events.send(Event::UpdateState(UpdateState::Failed {
                                reason: "the downloaded update did not match its published digest"
                                    .to_owned(),
                            }));
                            return;
                        }
                        let _ = events.send(Event::UpdateState(UpdateState::ReadyToInstall {
                            version: version.clone(),
                        }));
                        let _ = events.send(Event::Toast(Toast::success(format!(
                            "Update {version} staged at {} ({digest})",
                            destination.display()
                        ))));
                    }
                    Err(error) => {
                        let _ = events.send(Event::UpdateState(UpdateState::Failed {
                            reason: error.to_string(),
                        }));
                    }
                }
            });
        }
        Command::CheckPluginStatus => {
            let plugin = Arc::clone(plugin);
            let events = events.clone();
            tokio::task::spawn_blocking(move || match plugin.status() {
                Ok(status) => {
                    let _ = events.send(Event::PluginStatus(status));
                }
                Err(error) => {
                    let _ = events.send(Event::PluginStatus(PluginStatus::Failed {
                        reason: error.to_string(),
                    }));
                }
            });
        }
        Command::InstallPlugin => {
            let plugin = Arc::clone(plugin);
            let events = events.clone();
            let _ = events.send(Event::PluginStatus(PluginStatus::Working {
                step: "Installing the Steam store plugin".to_owned(),
            }));
            tokio::task::spawn_blocking(move || {
                let mut progress = |value: f32| {
                    let percent = (value.clamp(0.0, 1.0) * 100.0).round() as u8;
                    let _ = events.send(Event::PluginStatus(PluginStatus::Working {
                        step: format!("Installing the Steam store plugin ({percent}%)"),
                    }));
                };
                let result = plugin.install(&mut progress);
                finish_plugin(result, plugin.as_ref(), &events);
            });
        }
        Command::UninstallPlugin => {
            let plugin = Arc::clone(plugin);
            let events = events.clone();
            let _ = events.send(Event::PluginStatus(PluginStatus::Working {
                step: "Removing the Steam store plugin".to_owned(),
            }));
            tokio::task::spawn_blocking(move || {
                let result = plugin.uninstall();
                finish_plugin(result, plugin.as_ref(), &events);
            });
        }
        Command::SaveDepotSelection { app_id, enabled } => {
            match selections.save(app_id, &enabled) {
                Ok(()) => {
                    let _ = events.send(Event::DepotSelectionSaved(app_id));
                }
                Err(error) => {
                    let _ = events.send(Event::Error {
                        context: "save depots".to_owned(),
                        message: error.to_string(),
                    });
                }
            }
        }
        Command::HandleProtocolUrl(url) => {
            handle_url(&url, steam, services, selections, events).await;
        }
        Command::SignIn => {
            let Some(client) = auth_client.clone() else {
                let _ = events.send(Event::AuthState(AuthState::Failed {
                    reason: "sign-in is not configured (set LUATOOLS_SUPABASE_URL and LUATOOLS_SUPABASE_ANON_KEY)".to_owned(),
                }));
                return;
            };
            let store = Arc::clone(secret_store);
            let events = events.clone();
            let cancel = tokio_util::sync::CancellationToken::new();
            let auth_cancel = Arc::clone(auth_cancel);
            if let Some(previous) = auth_cancel.lock().take() {
                previous.cancel();
            }
            *auth_cancel.lock() = Some(cancel.clone());
            let callback = client.config().callback_url();
            let _ = events.send(Event::AuthState(AuthState::WaitingForBrowser {
                callback_url: callback.clone(),
            }));
            tokio::spawn(async move {
                let result = sign_in(client.as_ref(), cancel).await;
                *auth_cancel.lock() = None;
                match result {
                    Ok(session) => {
                        let _ = store.save(&session);
                        let _ = events.send(Event::AuthState(AuthState::SignedIn {
                            profile: profile_from_session(&session),
                        }));
                    }
                    Err(error) => {
                        let _ = events.send(Event::AuthState(AuthState::Failed {
                            reason: error.to_string(),
                        }));
                    }
                }
            });
        }
        Command::SignOut => {
            if let Err(error) = secret_store.clear() {
                let _ = events.send(Event::Error {
                    context: "sign out".to_owned(),
                    message: error.to_string(),
                });
            }
            let _ = events.send(Event::AuthState(AuthState::Guest));
        }
        Command::CancelSignIn => {
            if let Some(cancel) = auth_cancel.lock().take() {
                cancel.cancel();
            }
            let _ = events.send(Event::AuthState(AuthState::Guest));
        }
        Command::Quit => {
            let _ = events.send(Event::Toast(Toast::info("Closing LuaTools")));
            tokio::time::sleep(Duration::from_millis(50)).await;
            std::process::exit(0);
        }
        // `Command` is non-exhaustive; a newer UI may send a variant this build predates.
        other => {
            let _ = events.send(Event::Error {
                context: "backend".to_owned(),
                message: format!("unsupported command: {other:?}"),
            });
        }
    }
}

impl Services {
    fn clone_for_tasks(&self) -> Arc<ServiceSnapshot> {
        Arc::new(ServiceSnapshot {
            http: self.http.clone(),
            api: self.api.clone(),
            details: Arc::clone(&self.details),
            depots: Arc::clone(&self.depots),
            hardware: Arc::clone(&self.hardware),
            covers: self.covers.clone(),
        })
    }
}

/// Cheap snapshot of the long-lived services that background tasks can own.
struct ServiceSnapshot {
    http: reqwest::Client,
    api: ApiClient,
    details: Arc<AppDetailsCache>,
    depots: Arc<SteamDepotInfo>,
    hardware: Arc<HardwareAppIdService>,
    covers: CoverCache,
}

fn filter_hardware<T>(
    items: Vec<T>,
    hardware: &HardwareAppIdService,
    app: &impl Fn(&T) -> AppId,
) -> Vec<T> {
    items
        .into_iter()
        .filter(|item| !hardware.is_blacklisted(app(item)))
        .collect()
}

fn filter_featured(featured: Featured, hardware: &HardwareAppIdService) -> Featured {
    Featured {
        top_sellers: filter_hardware(featured.top_sellers, hardware, &|item: &FeaturedItem| {
            item.app_id
        }),
        new_releases: filter_hardware(featured.new_releases, hardware, &|item: &FeaturedItem| {
            item.app_id
        }),
    }
}

fn spawn_metadata_and_depots(
    app_id: AppId,
    services: Arc<ServiceSnapshot>,
    events: &UnboundedSender<Event>,
) {
    let events = events.clone();
    tokio::spawn(async move {
        if let Some(info) = services.depots.get(app_id).await {
            let _ = events.send(Event::DepotInfoLoaded(Box::new(info)));
        }
        if let Some(metadata) = services.details.resolve(app_id).await {
            if let Some(url) = metadata.header_image.as_deref()
                && let Some(path) =
                    download_cover(&services.http, &services.covers, app_id, url).await
            {
                let _ = events.send(Event::CoverUpdated { app_id, path });
            }
            let _ = events.send(Event::AppMetadataLoaded {
                app_id,
                metadata: Box::new(metadata),
            });
        }
    });
}

async fn download_cover(
    client: &reqwest::Client,
    covers: &CoverCache,
    app: AppId,
    url: &str,
) -> Option<PathBuf> {
    let path = covers.path_for(app);
    if path.is_file() {
        return Some(path);
    }
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return None;
    }
    let temp = path.with_extension("jpg.tmp");
    if std::fs::write(&temp, &bytes).is_err() {
        return None;
    }
    if std::fs::rename(&temp, &path).is_err() {
        let _ = std::fs::remove_file(&temp);
        return None;
    }
    Some(path)
}

fn refresh_library(
    steam: &mut SteamState,
    services: &Services,
    settings: &Settings,
    plugin_state: &Arc<parking_lot::RwLock<PluginState>>,
    events: &UnboundedSender<Event>,
) {
    let _ = events.send(Event::Busy {
        key: "library",
        active: true,
    });
    let started = std::time::Instant::now();
    let result = steam.rescan(
        settings.steam_path.as_deref(),
        &settings.extra_library_paths,
        services,
    );
    let _ = events.send(Event::Busy {
        key: "library",
        active: false,
    });
    match result {
        Ok(snapshot) => {
            tracing::info!(
                apps = snapshot.apps.len(),
                ms = started.elapsed().as_millis(),
                "library scanned"
            );
            let ids: Vec<AppId> = snapshot.apps.iter().map(|app| app.id).collect();
            plugin_state
                .write()
                .replace(snapshot.apps.iter().map(|app| (app.id, app.name.clone())));
            let _ = events.send(Event::LibraryRefreshed(Box::new(snapshot)));
            let services = services.clone_for_tasks();
            let events = events.clone();
            tokio::spawn(async move {
                for app in ids {
                    if let Some(metadata) = services.details.resolve(app).await {
                        let _ = events.send(Event::AppMetadataLoaded {
                            app_id: app,
                            metadata: Box::new(metadata.clone()),
                        });
                        if let Some(url) = metadata.header_image.as_deref()
                            && let Some(path) =
                                download_cover(&services.http, &services.covers, app, url).await
                        {
                            let _ = events.send(Event::CoverUpdated { app_id: app, path });
                        }
                    }
                }
            });
        }
        Err(error) => {
            let _ = events.send(Event::Error {
                context: "refresh library".to_owned(),
                message: error.to_string(),
            });
        }
    }
}

async fn handle_url(
    url: &str,
    steam: &mut SteamState,
    services: &Services,
    selections: &DepotSelectionStore,
    events: &UnboundedSender<Event>,
) {
    let Some(link) = parse_steam_link(url) else {
        let _ = events.send(Event::Error {
            context: "protocol".to_owned(),
            message: format!("unrecognised link: {url}"),
        });
        return;
    };
    let show_window = !link.is_silent();
    if show_window {
        let _ = events.send(Event::ShowWindow);
    }
    match link {
        luatools_steam::SteamLink::Store(app) | luatools_steam::SteamLink::SteamDb(app) => {
            let _ = events.send(Event::Navigate(NavTarget::Add));
            load_store_for_protocol(app, steam, services, events).await;
        }
        luatools_steam::SteamLink::App(app) => {
            let _ = events.send(Event::Navigate(NavTarget::Manage));
            if let Some(details) = steam.details(app, services, selections) {
                let _ = events.send(Event::AppDetailsLoaded(Box::new(details)));
                spawn_metadata_and_depots(app, services.clone_for_tasks(), events);
            }
        }
        luatools_steam::SteamLink::Protocol {
            action,
            app,
            silent,
        } => match action.as_str() {
            "game" => {
                let _ = events.send(Event::Navigate(NavTarget::Add));
                load_store_for_protocol(app, steam, services, events).await;
            }
            "manage" => {
                let _ = events.send(Event::Navigate(NavTarget::Manage));
                if let Some(details) = steam.details(app, services, selections) {
                    let _ = events.send(Event::AppDetailsLoaded(Box::new(details)));
                    spawn_metadata_and_depots(app, services.clone_for_tasks(), events);
                }
            }
            "install" => {
                if !silent {
                    let _ = events.send(Event::Navigate(NavTarget::Add));
                }
                report(platform::install_steam_app(app.get()), "install", events);
            }
            "run" | "rungameid" => {
                report(platform::launch_steam_app(app.get()), "launch", events);
            }
            "store" => {
                let _ = events.send(Event::Navigate(NavTarget::Add));
                load_store_for_protocol(app, steam, services, events).await;
            }
            _ => {
                let _ = events.send(Event::Toast(Toast::info(
                    "This protocol action is not available in this build",
                )));
            }
        },
    }
}

async fn load_store_for_protocol(
    app: AppId,
    steam: &mut SteamState,
    services: &Services,
    events: &UnboundedSender<Event>,
) {
    if let Some(metadata) = services.details.resolve(app).await {
        let details = steam.store_details(app, metadata, services);
        let _ = events.send(Event::StoreDetailsLoaded(Box::new(details)));
    }
}

fn save_launch_entries(
    steam: &mut SteamState,
    mod_store: &LaunchModStore,
    app_id: AppId,
    entries: &[LaunchOption],
    events: &UnboundedSender<Event>,
) {
    let Some(root) = steam.root.clone() else {
        let _ = events.send(Event::Error {
            context: "save launch entries".to_owned(),
            message: "Steam was not found".to_owned(),
        });
        return;
    };
    ensure_appinfo(steam, &root);
    let original = steam
        .appinfo
        .as_mut()
        .and_then(|cache| cache.get(app_id).ok().flatten())
        .map(|info| read_launch_options(info.body().unwrap_or(&info.data)))
        .unwrap_or_default();
    if let Err(error) = mod_store.stage_with_original(app_id, entries, &original) {
        let _ = events.send(Event::Error {
            context: "save launch entries".to_owned(),
            message: error.to_string(),
        });
        return;
    }
    match write_appinfo_launch_entries(steam, &root, app_id, entries) {
        Ok(()) => {
            check_launch_drift(steam, mod_store, events);
            let _ = events.send(Event::LaunchOptionsSaved(app_id));
            let _ = events.send(Event::Toast(Toast::success(
                "Launch entries written to appinfo.vdf; restart Steam to apply them",
            )));
        }
        Err(error) => {
            let _ = events.send(Event::Error {
                context: "write appinfo".to_owned(),
                message: error.to_string(),
            });
        }
    }
}

fn restore_launch_entries(
    steam: &mut SteamState,
    mod_store: &LaunchModStore,
    app_id: AppId,
    events: &UnboundedSender<Event>,
) {
    let Some(root) = steam.root.clone() else {
        let _ = events.send(Event::Error {
            context: "restore launch entries".to_owned(),
            message: "Steam was not found".to_owned(),
        });
        return;
    };
    let original = match mod_store.original(app_id) {
        Ok(Some(original)) => original,
        Ok(None) => {
            let _ = events.send(Event::Toast(Toast::info(
                "There are no staged launch entries to restore",
            )));
            return;
        }
        Err(error) => {
            let _ = events.send(Event::Error {
                context: "restore launch entries".to_owned(),
                message: error.to_string(),
            });
            return;
        }
    };
    ensure_appinfo(steam, &root);
    match write_appinfo_launch_entries(steam, &root, app_id, &original) {
        Ok(()) => {
            if let Err(error) = mod_store.clear(app_id) {
                tracing::warn!(%error, app_id = app_id.get(), "could not clear launch store");
            }
            check_launch_drift(steam, mod_store, events);
            let _ = events.send(Event::LaunchOptionsSaved(app_id));
            let _ = events.send(Event::Toast(Toast::success(
                "Steam's original launch entries were restored",
            )));
        }
        Err(error) => {
            let _ = events.send(Event::Error {
                context: "restore appinfo".to_owned(),
                message: error.to_string(),
            });
        }
    }
}

fn prune_appinfo_backups(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("appinfo-") && name.ends_with(".vdf"))
        })
        .collect();
    if backups.len() <= keep {
        return;
    }
    backups.sort();
    let remove = backups.len() - keep;
    for path in backups.into_iter().take(remove) {
        let _ = std::fs::remove_file(path);
    }
}

fn ensure_appinfo(steam: &mut SteamState, root: &Path) {
    if steam.appinfo.is_some() {
        return;
    }
    let path = root.join("appcache").join("appinfo.vdf");
    match AppInfoCache::open(&path) {
        Ok(cache) => steam.appinfo = Some(cache),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "appinfo unavailable");
        }
    }
}

fn write_appinfo_launch_entries(
    steam: &mut SteamState,
    root: &Path,
    app_id: AppId,
    entries: &[LaunchOption],
) -> luatools_core::Result<()> {
    let mut edits = HashMap::new();
    edits.insert(app_id, entries.to_vec());
    write_appinfo_launch_edits(steam, root, &edits)
}

fn write_appinfo_launch_edits(
    steam: &mut SteamState,
    root: &Path,
    edits: &HashMap<AppId, Vec<LaunchOption>>,
) -> luatools_core::Result<()> {
    let Some(cache) = steam.appinfo.as_mut() else {
        return Err(luatools_core::Error::NotFound {
            path: root.join("appcache").join("appinfo.vdf"),
        });
    };
    let mut replacements = HashMap::with_capacity(edits.len());
    for (app_id, entries) in edits {
        let Some(info) = cache.get(*app_id)? else {
            continue;
        };
        let mut replacement = info.data.clone();
        if let Some(body) = replacement.get_table_mut("appinfo") {
            write_launch_entries(body, entries);
        } else {
            write_launch_entries(&mut replacement, entries);
        }
        replacements.insert(*app_id, replacement);
    }
    if replacements.is_empty() {
        return Ok(());
    }

    let path = root.join("appcache").join("appinfo.vdf");
    let backup_dir = root.join("appcache").join("luatools-backup");
    std::fs::create_dir_all(&backup_dir)
        .map_err(|error| luatools_core::Error::io(backup_dir.display().to_string(), error))?;
    let backup = backup_dir.join(format!(
        "appinfo-{}.vdf",
        Utc::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::copy(&path, &backup)
        .map_err(|error| luatools_core::Error::io(backup.display().to_string(), error))?;
    prune_appinfo_backups(&backup_dir, 3);

    let temp = path.with_file_name("appinfo.vdf.tmp");
    cache.save_as(&temp, &replacements)?;
    // Drop the read handle before replacing the file; Windows cannot rename over an open file.
    steam.appinfo = None;
    let steam_was_running = platform::steam_is_running();
    if steam_was_running {
        let _ = platform::stop_steam();
    }
    let renamed = std::fs::rename(&temp, &path);
    if steam_was_running {
        let _ = platform::start_steam();
    }
    if let Err(error) = renamed {
        let _ = std::fs::remove_file(&temp);
        return Err(luatools_core::Error::io(path.display().to_string(), error));
    }
    ensure_appinfo(steam, root);
    Ok(())
}

fn build_blocking_client(settings: &Settings) -> Option<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(luatools_core::config::USER_AGENT);
    if let Some(proxy) = settings
        .proxy_url
        .as_deref()
        .map(str::trim)
        .filter(|proxy| !proxy.is_empty())
    {
        builder = builder.proxy(reqwest::Proxy::all(proxy).ok()?);
    }
    if settings.use_doh {
        builder = builder.dns_resolver(Arc::new(DohResolver::new()));
    }
    builder.build().ok()
}

async fn track_launch(client: &reqwest::Client) {
    let body = serde_json::json!({
        "type": "event",
        "payload": {
            "website": luatools_core::config::UMAMI_WEBSITE_ID,
            "hostname": luatools_core::config::UMAMI_HOSTNAME,
            "url": format!("/launch/{}", env!("CARGO_PKG_VERSION")),
            "title": "App Launch",
        }
    });
    let result = client
        .post(format!("{}/api/send", luatools_core::config::UMAMI_HOST))
        .header(
            "user-agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .json(&body)
        .send()
        .await;
    if let Err(error) = result {
        tracing::debug!(%error, "analytics launch event failed");
    }
}

fn fresh_installation_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let digest = luatools_core::hash::sha256_bytes(
        format!("{now}:{}:{}", std::process::id(), std::env::consts::OS).as_bytes(),
    );
    digest.as_str().chars().take(32).collect()
}

fn spawn_plugin_toggle(
    plugin: Arc<dyn PluginInstaller>,
    enabled: bool,
    events: UnboundedSender<Event>,
) {
    let _ = events.send(Event::PluginStatus(PluginStatus::Working {
        step: if enabled {
            "Installing the Steam store plugin".to_owned()
        } else {
            "Removing the Steam store plugin".to_owned()
        },
    }));
    tokio::task::spawn_blocking(move || {
        let result = if enabled {
            let mut progress = |_| {};
            plugin.install(&mut progress)
        } else {
            plugin.uninstall()
        };
        finish_plugin(result, plugin.as_ref(), &events);
    });
}

struct BackendCefBridge {
    events: UnboundedSender<Event>,
    plugin_state: Arc<parking_lot::RwLock<PluginState>>,
    http: reqwest::Client,
    pending_update: Arc<parking_lot::Mutex<Option<luatools_net::update::Release>>>,
}

impl CefBridge for BackendCefBridge {
    fn has_app(&self, app: u32) -> bool {
        self.plugin_state.read().has_app(AppId::from(app))
    }

    fn open_settings(&self) {
        let _ = self.events.send(Event::Navigate(NavTarget::Settings));
        let _ = self.events.send(Event::ShowWindow);
    }

    fn restart_steam(&self) {
        let events = self.events.clone();
        let _ = events.send(Event::Busy {
            key: "steam",
            active: true,
        });
        tokio::task::spawn_blocking(move || {
            let result = platform::restart_steam();
            if let Err(error) = result {
                let _ = events.send(Event::Error {
                    context: "restart steam".to_owned(),
                    message: error.to_string(),
                });
            }
            let _ = events.send(Event::Busy {
                key: "steam",
                active: false,
            });
        });
    }

    fn open_external_url(&self, url: &str) {
        if let Err(error) = platform::open_url(url) {
            let _ = self.events.send(Event::Error {
                context: "open url".to_owned(),
                message: error.to_string(),
            });
        }
    }

    fn check_updates(&self) {
        let http = self.http.clone();
        let events = self.events.clone();
        let pending_update = Arc::clone(&self.pending_update);
        tokio::spawn(async move {
            let _ = events.send(Event::UpdateState(UpdateState::Checking));
            if let Some(release) = update_check(&http, &events).await {
                let current = env!("CARGO_PKG_VERSION").to_owned();
                match release {
                    Some(release) => {
                        *pending_update.lock() = Some(release.clone());
                        let _ = events.send(Event::UpdateState(UpdateState::Available {
                            current,
                            latest: release.version,
                            notes: release.notes,
                        }));
                    }
                    None => {
                        let _ = events.send(Event::UpdateState(UpdateState::UpToDate { current }));
                    }
                }
            }
        });
    }

    fn loaded_apps(&self) -> Vec<(u32, String)> {
        self.plugin_state
            .read()
            .loaded()
            .into_iter()
            .map(|(app, name)| (app.get(), name))
            .collect()
    }

    fn dismiss_loaded_apps(&self) {
        self.plugin_state.write().dismiss_loaded();
    }

    fn log(&self, message: &str) {
        tracing::info!(target: "luatools::plugin", %message);
    }
}

fn finish_plugin(
    result: luatools_core::Result<()>,
    plugin: &dyn PluginInstaller,
    events: &UnboundedSender<Event>,
) {
    match result {
        Ok(()) => {
            let status = plugin.status().unwrap_or(PluginStatus::NotInstalled);
            let _ = events.send(Event::PluginStatus(status));
            let _ = events.send(Event::Toast(Toast::success("Steam plugin updated")));
        }
        Err(error) => {
            let _ = events.send(Event::PluginStatus(PluginStatus::Failed {
                reason: error.to_string(),
            }));
            let _ = events.send(Event::Error {
                context: "plugin".to_owned(),
                message: error.to_string(),
            });
        }
    }
}

fn restore_auth(
    store: Arc<FileSecretStore>,
    client: Option<Arc<AuthClient>>,
    events: UnboundedSender<Event>,
) {
    tokio::spawn(async move {
        let session = match store.load() {
            Ok(Some(session)) => session,
            Ok(None) => {
                let _ = events.send(Event::AuthState(AuthState::Guest));
                return;
            }
            Err(error) => {
                tracing::debug!(%error, "stored auth session could not be read");
                let _ = events.send(Event::AuthState(AuthState::Guest));
                return;
            }
        };
        let session = if session.needs_refresh(Utc::now(), chrono::Duration::minutes(2)) {
            if let (Some(client), Some(refresh)) =
                (client.as_ref(), session.refresh_token.as_deref())
            {
                match client.refresh(refresh).await {
                    Ok(refreshed) => {
                        let _ = store.save(&refreshed);
                        refreshed
                    }
                    Err(error) => {
                        tracing::debug!(%error, "stored auth session could not be refreshed");
                        let _ = store.clear();
                        let _ = events.send(Event::AuthState(AuthState::Guest));
                        return;
                    }
                }
            } else {
                session
            }
        } else {
            session
        };
        let _ = events.send(Event::AuthState(AuthState::SignedIn {
            profile: profile_from_session(&session),
        }));
    });
}

fn profile_from_session(session: &luatools_net::Session) -> luatools_core::UserProfile {
    luatools_core::UserProfile {
        id: session
            .email
            .clone()
            .or_else(|| session.display_name.clone())
            .unwrap_or_else(|| "session".to_owned()),
        display_name: session.display_name.clone(),
        email: session.email.clone(),
        avatar_url: session.avatar_url.clone(),
    }
}

async fn sign_in(
    client: &AuthClient,
    cancel: tokio_util::sync::CancellationToken,
) -> luatools_core::Result<luatools_net::Session> {
    let pkce = luatools_net::try_pkce_pair()?;
    let url = client.authorize_url(&pkce.challenge);
    platform::open_url(&url)?;
    let code = client
        .wait_for_code(Duration::from_secs(300), cancel)
        .await?;
    client.exchange_code(&code, &pkce.verifier).await
}

fn update_asset(
    release: &luatools_net::update::Release,
) -> Option<&luatools_net::update::ReleaseAsset> {
    let os = std::env::consts::OS;
    let candidates: [&str; 3] = match os {
        "windows" => ["windows", "win", ".exe"],
        "macos" => ["macos", "darwin", ".dmg"],
        _ => ["linux", "appimage", ".tar"],
    };
    release
        .assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_ascii_lowercase();
            candidates.iter().any(|needle| name.contains(needle))
                && !name.ends_with(".sha256")
                && !name.ends_with(".sig")
        })
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|asset| !asset.name.ends_with(".sha256") && !asset.name.ends_with(".sig"))
        })
}

async fn update_check(
    client: &reqwest::Client,
    events: &UnboundedSender<Event>,
) -> Option<Option<luatools_net::update::Release>> {
    let checker = match GitHubReleases::new(client.clone(), UPDATE_REPOS, env!("CARGO_PKG_VERSION"))
    {
        Ok(checker) => checker,
        Err(error) => {
            let _ = events.send(Event::UpdateState(UpdateState::Failed {
                reason: error.to_string(),
            }));
            return None;
        }
    };
    match checker.check().await {
        Ok(release) => Some(release),
        Err(error) => {
            let _ = events.send(Event::UpdateState(UpdateState::Failed {
                reason: error.to_string(),
            }));
            None
        }
    }
}

async fn hydrate_history(queue: &QueueHandle, events: &UnboundedSender<Event>) {
    match queue.list().await {
        Ok(jobs) => {
            for job in jobs {
                let _ = events.send(Event::DownloadUpdated(Box::new(job)));
            }
        }
        Err(error) => {
            let _ = events.send(Event::Log {
                level: LogLevel::Warn,
                message: format!("could not load download history: {error}"),
            });
        }
    }
}

fn report_flag(ok: bool, context: &str, events: &UnboundedSender<Event>) {
    if !ok {
        let _ = events.send(Event::Error {
            context: context.to_owned(),
            message: "the queue rejected the request".to_owned(),
        });
    }
}

fn report(result: luatools_core::Result<()>, context: &str, events: &UnboundedSender<Event>) {
    if let Err(error) = result {
        let _ = events.send(Event::Error {
            context: context.to_owned(),
            message: error.to_string(),
        });
    }
}

/// Removes a stale temporary file left by a crashed save.
pub fn clean_stale_temp(paths: &AppPaths) {
    let stale = paths.settings_file().with_extension("json.tmp");
    if let Err(error) = std::fs::remove_file(&stale)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %stale.display(), "stale settings temp file");
    }
}

/// Path of the single-instance handoff file.
#[must_use]
pub fn pending_url_path(paths: &AppPaths) -> PathBuf {
    paths.data_dir().join("pending-url.tmp")
}

fn take_pending(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(executable: &str, arguments: &str) -> LaunchOption {
        LaunchOption {
            executable: executable.to_owned(),
            arguments: arguments.to_owned(),
            working_dir: String::new(),
            description: "Play".to_owned(),
            option_type: "default".to_owned(),
            os_list: "windows".to_owned(),
            os_arch: String::new(),
            beta_key: String::new(),
            owns_dlc: String::new(),
            ..LaunchOption::default()
        }
    }

    #[test]
    fn launch_text_renders_one_line_per_entry() {
        let options = vec![option("game.exe", "-novid"), option("tool.exe", "")];
        assert_eq!(launch_options_text(&options), "game.exe -novid\ntool.exe");
    }

    #[test]
    fn launch_text_parses_back_into_arguments() {
        let original = vec![option("game.exe", "-novid")];
        let parsed = parse_launch_options_text("game.exe -console -dx11\n", &original);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].executable, "game.exe");
        assert_eq!(parsed[0].arguments, "-console -dx11");
    }

    #[test]
    fn launch_text_ignores_blank_lines() {
        let original = vec![option("game.exe", "")];
        let parsed = parse_launch_options_text("\n  \ngame.exe -x\n", &original);
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn metadata_updates_display_fields() {
        let mut entry = AppEntry::new(AppId::new(730), "730");
        apply_metadata(
            &mut entry,
            &AppMetadata {
                name: Some("Counter-Strike 2".to_owned()),
                header_image: Some("https://cdn/730.jpg".to_owned()),
                release_date: Some("21 Aug, 2012".to_owned()),
                developers: vec!["Valve".to_owned()],
                genres: vec!["Action".to_owned()],
                ..AppMetadata::default()
            },
        );
        assert_eq!(entry.name, "Counter-Strike 2");
        assert_eq!(entry.cover_url.as_deref(), Some("https://cdn/730.jpg"));
        assert_eq!(entry.developer.as_deref(), Some("Valve"));
        assert_eq!(entry.genres, vec!["Action"]);
    }
    #[test]
    fn update_asset_prefers_the_current_platform_and_skips_signatures() {
        let release = luatools_net::update::Release {
            version: "1.0.0".to_owned(),
            notes: String::new(),
            assets: vec![
                luatools_net::update::ReleaseAsset {
                    name: "LuaTools-linux.tar.gz".to_owned(),
                    download_url: "https://example.invalid/linux".to_owned(),
                    digest: None,
                    size: None,
                },
                luatools_net::update::ReleaseAsset {
                    name: "LuaTools-windows.exe".to_owned(),
                    download_url: "https://example.invalid/windows".to_owned(),
                    digest: None,
                    size: None,
                },
                luatools_net::update::ReleaseAsset {
                    name: "LuaTools-windows.exe.sha256".to_owned(),
                    download_url: "https://example.invalid/sha".to_owned(),
                    digest: None,
                    size: None,
                },
            ],
        };
        let asset = update_asset(&release).expect("an asset");
        let expected = if cfg!(target_os = "linux") {
            "LuaTools-linux.tar.gz"
        } else {
            "LuaTools-windows.exe"
        };
        assert_eq!(asset.name, expected);
    }

    #[test]
    fn depot_selection_round_trips_and_distinguishes_empty_from_absent() {
        let dir = tempfile::tempdir().expect("temp");
        let store = DepotSelectionStore::new(dir.path().join("depots.json"));
        assert_eq!(store.is_enabled(AppId::new(1), DepotId::new(2)), None);
        store.save(AppId::new(1), &[DepotId::new(2)]).expect("save");
        assert_eq!(store.is_enabled(AppId::new(1), DepotId::new(2)), Some(true));
        assert_eq!(
            store.is_enabled(AppId::new(1), DepotId::new(3)),
            Some(false)
        );
        store.save(AppId::new(1), &[]).expect("save empty");
        assert_eq!(
            store.is_enabled(AppId::new(1), DepotId::new(2)),
            Some(false)
        );
    }
    #[test]
    fn launch_drift_comparison_ignores_table_keys() {
        let mut left = option("game.exe", "-novid");
        left.index = "0".to_owned();
        left.source_index = "0".to_owned();
        let mut right = option("game.exe", "-novid");
        right.index = "1".to_owned();
        right.source_index = "2".to_owned();
        assert!(launch_options_equal(&[left.clone()], &[right.clone()]));
        right.arguments = "-console".to_owned();
        assert!(!launch_options_equal(&[left], &[right]));
    }
}
