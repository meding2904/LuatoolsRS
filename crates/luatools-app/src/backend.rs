//! Backend actor: owns the runtime and the services, consumes [`Command`]s, emits [`Event`]s.
//!
//! The UI never touches Steam, HTTP, or the filesystem. It sends commands to this actor, which
//! owns the runtime and the caches. Steam work happens on the actor task; progress and results
//! leave as [`Event`]s.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use luatools_core::{
    AppDetails, AppEntry, AppId, AppPaths, BackendHandle, Command, DepotEntry, Event,
    InstalledState, LibrarySnapshot, LogLevel, PluginStatus, Settings, Toast, UpdateState,
};
use luatools_downloads::{DownloadQueue, HistoryStore, QueueConfig, QueueHandle};
use luatools_net::http::{HttpClientConfig, build as build_http};
use luatools_net::plugin::{PluginInstaller, UnsupportedPluginInstaller};
use luatools_net::update::GitHubReleases;
use luatools_steam::{
    AppInfoCache, LaunchModStore, LaunchOption, SteamLibrary, discover_libraries, find_steam_root,
    parse_steam_link, read_launch_options,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::platform;
use crate::settings_store::SettingsStore;

/// Repositories checked for updates, primary first.
const UPDATE_REPOS: [&str; 2] = ["madoiscool/LuaTools", "mendy-tools/LuaTools"];

/// Everything the actor needs to start.
pub struct BackendConfig {
    /// Filesystem layout.
    pub paths: AppPaths,
    /// Persisted settings.
    pub settings: Settings,
    /// Initial protocol URL or pending URL.
    pub startup_url: Option<String>,
}

/// Long-lived caches owned by the actor.
struct SteamState {
    root: Option<PathBuf>,
    libraries: Vec<SteamLibrary>,
    appinfo: Option<AppInfoCache>,
    names: HashMap<AppId, String>,
}

impl SteamState {
    fn empty() -> Self {
        Self {
            root: None,
            libraries: Vec::new(),
            appinfo: None,
            names: HashMap::new(),
        }
    }

    fn rescan(&mut self, override_path: Option<&Path>) -> luatools_core::Result<LibrarySnapshot> {
        self.root = find_steam_root(override_path);
        self.libraries.clear();
        self.names.clear();
        self.appinfo = None;

        let Some(root) = self.root.clone() else {
            return Ok(LibrarySnapshot::default());
        };
        self.libraries = discover_libraries(&root)?;
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
            let name = self.app_name(id, &fallback);
            apps.push(AppEntry {
                id,
                name,
                installed: InstalledState::Installed {
                    path,
                    size_on_disk,
                    build_id,
                },
                cover_url: None,
                release_date: None,
                developer: None,
                genres: Vec::new(),
                last_played: None,
                playtime_minutes: 0,
            });
        }
        apps.sort_by_key(|a| a.name.to_lowercase());
        apps.dedup_by_key(|a| a.id);

        Ok(LibrarySnapshot {
            apps,
            steam_root: self.root.clone(),
            scanned_at: Some(Utc::now()),
        })
    }

    /// Resolves a display name from appinfo, then the ACF name, then the id.
    fn app_name(&mut self, app_id: AppId, fallback: &str) -> String {
        if let Some(name) = self.names.get(&app_id) {
            return name.clone();
        }
        let mut resolved = None;
        if let Some(cache) = self.appinfo.as_mut()
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

    fn details(&mut self, app_id: AppId, snapshot: &LibrarySnapshot) -> Option<AppDetails> {
        let entry = snapshot.app(app_id).cloned();
        let installed = self.installed_entry(app_id).map(|(library, manifest)| {
            (
                manifest.install_path(&library.root),
                manifest.size_on_disk,
                manifest.build_id.clone(),
                manifest.install_dir.clone(),
            )
        });
        let app = entry.or_else(|| {
            installed.as_ref().map(|(path, size, build, _)| AppEntry {
                id: app_id,
                name: self.app_name(app_id, ""),
                installed: InstalledState::Installed {
                    path: path.clone(),
                    size_on_disk: *size,
                    build_id: build.clone(),
                },
                cover_url: None,
                release_date: None,
                developer: None,
                genres: Vec::new(),
                last_played: None,
                playtime_minutes: 0,
            })
        })?;

        let (launch_options, depots) = self.read_app_metadata(app_id);
        Some(AppDetails {
            app,
            depots,
            launch_options,
            install_dir: installed.map(|(_, _, _, dir)| dir),
            library_found: self.root.is_some(),
        })
    }

    fn read_app_metadata(&mut self, app_id: AppId) -> (String, Vec<DepotEntry>) {
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
                    depots.push(DepotEntry {
                        id: luatools_core::DepotId::new(depot_id),
                        name,
                        size: None,
                        manifest_id: None,
                        optional: false,
                        enabled: true,
                    });
                }
            }
        }
        depots.sort_by_key(|d| d.id);
        (launch_options_text(&launch), depots)
    }
}

/// Formats staged launch entries as one `executable args` line each.
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

/// Parses edited launch lines back into full options, keeping the fields the text does not show.
///
/// Lines are matched to the original entries by position; extra lines inherit the defaults of
/// the first original entry, and blank lines are ignored.
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
    let mod_store = LaunchModStore::new(config.paths.data_dir().join("launchmods.json"));
    let mut steam = SteamState::empty();

    let queue = DownloadQueue::spawn(
        QueueConfig::new(
            settings.effective_concurrency(),
            config.paths.cache_dir().join("staging"),
        ),
        events.clone(),
        Some(HistoryStore::new(config.paths.history_file())),
    );
    hydrate_history(&queue, &events).await;

    let plugin = UnsupportedPluginInstaller;
    if let Ok(status) = plugin.status() {
        let _ = events.send(Event::PluginStatus(status));
    }
    let _ = events.send(Event::SettingsSaved(Box::new(settings.clone())));
    let _ = events.send(Event::UpdateState(UpdateState::Idle));

    if let Some(url) = config.startup_url {
        handle_url(&url, &events);
    }

    while let Some(command) = commands.recv().await {
        match command {
            Command::RefreshLibrary => {
                let _ = events.send(Event::Busy {
                    key: "library",
                    active: true,
                });
                let started = std::time::Instant::now();
                let result = steam.rescan(settings.steam_path.as_deref());
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
                        let _ = events.send(Event::LibraryRefreshed(Box::new(snapshot)));
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error {
                            context: "refresh library".to_owned(),
                            message: error.to_string(),
                        });
                    }
                }
            }
            Command::LoadAppDetails(app_id) => {
                if steam.libraries.is_empty()
                    && let Ok(snapshot) = steam.rescan(settings.steam_path.as_deref())
                {
                    let _ = events.send(Event::LibraryRefreshed(Box::new(snapshot)));
                }
                let snapshot = LibrarySnapshot {
                    apps: steam
                        .libraries
                        .iter()
                        .flat_map(|library| library.apps.iter())
                        .filter(|m| m.installed)
                        .map(|m| AppEntry::new(m.app_id, m.name.clone()))
                        .collect(),
                    steam_root: steam.root.clone(),
                    scanned_at: None,
                };
                match steam.details(app_id, &snapshot) {
                    Some(details) => {
                        let _ = events.send(Event::AppDetailsLoaded(Box::new(details)));
                    }
                    None => {
                        let _ = events.send(Event::Error {
                            context: "load app".to_owned(),
                            message: format!("app {app_id} was not found in any Steam library"),
                        });
                    }
                }
            }
            Command::SaveLaunchOptions { app_id, options } => {
                let original = steam
                    .appinfo
                    .as_mut()
                    .and_then(|cache| cache.get(app_id).ok().flatten())
                    .and_then(|info| info.body().cloned())
                    .map(|body| read_launch_options(&body))
                    .unwrap_or_default();
                let parsed = parse_launch_options_text(&options, &original);
                match mod_store.stage_with_original(app_id, &parsed, &original) {
                    Ok(()) => {
                        let _ = events.send(Event::LaunchOptionsSaved(app_id));
                    }
                    Err(error) => {
                        let _ = events.send(Event::Error {
                            context: "save launch options".to_owned(),
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
            Command::PauseDownload(id) => report_flag(queue.pause(id), "pause", &events),
            Command::ResumeDownload(id) => report_flag(queue.resume(id), "resume", &events),
            Command::CancelDownload(id) => report_flag(queue.cancel(id), "cancel", &events),
            Command::RemoveDownload(id) => report_flag(queue.remove(id), "remove", &events),
            Command::RetryDownload(id) => report_flag(queue.retry(id), "retry", &events),
            Command::ClearDownloadHistory => {
                report_flag(queue.clear_history(), "clear history", &events);
            }
            Command::SaveSettings(next) => {
                let next = *next;
                match settings_store.save(&next) {
                    Ok(()) => {
                        settings = next.clone();
                        let _ = events.send(Event::SettingsSaved(Box::new(next)));
                        let _ = events.send(Event::Toast(Toast::success("Settings saved")));
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
                settings = settings_store.load();
                let _ = events.send(Event::SettingsSaved(Box::new(settings.clone())));
            }
            Command::OpenPath(path) => report(platform::open_path(&path), "open", &events),
            Command::RevealPath(path) => report(platform::reveal_path(&path), "reveal", &events),
            Command::OpenUrl(url) => report(platform::open_url(&url), "open url", &events),
            Command::LaunchApp(app_id) => {
                report(platform::launch_steam_app(app_id.get()), "launch", &events);
            }
            Command::CheckForUpdates => {
                let _ = events.send(Event::UpdateState(UpdateState::Checking));
                let Some(assets) = update_check(&events).await else {
                    continue;
                };
                let current = env!("CARGO_PKG_VERSION").to_owned();
                if let Some(release) = assets {
                    let _ = events.send(Event::UpdateState(UpdateState::Available {
                        current,
                        latest: release.version,
                        notes: release.notes,
                    }));
                } else {
                    let _ = events.send(Event::UpdateState(UpdateState::UpToDate { current }));
                }
            }
            Command::InstallUpdate => {
                let _ = events.send(Event::Toast(Toast::info(
                    "Updates are applied by your package manager in this build",
                )));
            }
            Command::InstallPlugin | Command::UninstallPlugin => {
                let _ = events.send(Event::Error {
                    context: "plugin".to_owned(),
                    message: "the Steam plugin installer is only available on Windows".to_owned(),
                });
                let _ = events.send(Event::PluginStatus(PluginStatus::NotInstalled));
            }
            Command::SaveDepotSelection { app_id, .. } => {
                let _ = events.send(Event::Error {
                    context: "depots".to_owned(),
                    message: format!("depot editing is not available for {app_id}"),
                });
            }
            Command::Quit => {
                let _ = events.send(Event::Toast(Toast::info("Closing LuaTools")));
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
}

/// Checks GitHub releases, returning `Some(None)` for up-to-date and `None` on failure.
async fn update_check(
    events: &UnboundedSender<Event>,
) -> Option<Option<luatools_net::update::Release>> {
    let client = match build_http(&HttpClientConfig::default()) {
        Ok(client) => client,
        Err(error) => {
            let _ = events.send(Event::UpdateState(UpdateState::Failed {
                reason: error.to_string(),
            }));
            return None;
        }
    };
    let checker = match GitHubReleases::new(client, UPDATE_REPOS, env!("CARGO_PKG_VERSION")) {
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

fn handle_url(url: &str, events: &UnboundedSender<Event>) {
    match parse_steam_link(url) {
        Some(link) => {
            let _ = events.send(Event::Toast(Toast::info(format!("Opening {link:?}"))));
        }
        None => {
            let _ = events.send(Event::Error {
                context: "protocol".to_owned(),
                message: format!("unrecognised link: {url}"),
            });
        }
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
        assert_eq!(parsed[0].description, "Play");
    }

    #[test]
    fn blank_lines_are_ignored_and_extra_lines_inherit_defaults() {
        let original = vec![option("game.exe", "")];
        let parsed = parse_launch_options_text("\ngame.exe\nmod.exe -abc\n", &original);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].executable, "mod.exe");
        assert_eq!(parsed[1].os_list, "windows");
    }
}
