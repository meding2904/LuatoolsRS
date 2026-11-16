//! Application state: domain data, page state, and the reducer that folds backend events.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use luatools_core::{
    AppDetails, AppId, AuthState, BackendHandle, Command, DownloadId, DownloadSnapshot, Event,
    LibrarySnapshot, LogLevel, PluginStatus, Settings, Toast, UpdateState,
};
use luatools_i18n::{Key, Translator};

use crate::pages::{self, Page};
use crate::theme::Theme;

pub use luatools_core::ToastKind;

/// A toast currently on screen.
#[derive(Debug, Clone)]
pub struct ToastItem {
    /// Tone.
    pub kind: ToastKind,
    /// Rendered text.
    pub message: String,
    /// When it was pushed, used for expiry.
    pub shown_at: Instant,
}

impl ToastItem {
    /// Builds a toast item from a backend toast.
    #[must_use]
    pub fn new(toast: Toast) -> Self {
        Self {
            kind: toast.kind,
            message: toast.message,
            shown_at: Instant::now(),
        }
    }
}

/// One line in the in-app console.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Severity.
    pub level: LogLevel,
    /// Rendered line.
    pub message: String,
}

/// Maximum number of log lines kept in memory.
const LOG_CAPACITY: usize = 500;

/// Per-page state, owned here so pages stay stateless functions.
#[derive(Default)]
pub struct Pages {
    /// Home dashboard.
    pub home: pages::home::State,
    /// Store catalogue.
    pub add: pages::add::State,
    /// Library/browse page.
    pub builds: pages::builds::State,
    /// Download queue.
    pub downloads: pages::downloads::State,
    /// App management page.
    pub manage: pages::manage::State,
    /// Steam store plugin.
    pub plugin: pages::plugin::State,
    /// Settings page.
    pub settings: pages::settings::State,
    /// First-run flow.
    pub onboarding: pages::onboarding::State,
}

/// The launch-options dialog, open while `Some`.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchOptionsDialog {
    /// App being edited.
    pub app_id: AppId,
    /// App name for the title.
    pub app_name: String,
    /// Structured Play-button entries.
    pub entries: Vec<luatools_core::LaunchOption>,
    /// Editable localconfig LaunchOptions string.
    pub local_options: String,
    /// Selected entry index.
    pub selected: usize,
    /// Inline validation message.
    pub error: Option<String>,
}

/// Everything the interface renders.
pub struct AppState {
    /// Command channel to the backend.
    pub backend: BackendHandle,
    /// Active palette.
    pub theme: Theme,
    /// Whether the dark palette is active.
    pub dark_mode: bool,
    /// Active translations.
    pub i18n: Translator,
    /// Settings currently in effect.
    pub settings: Settings,
    /// Last library scan.
    pub snapshot: LibrarySnapshot,
    /// Loaded app details, keyed by app.
    pub details: HashMap<AppId, AppDetails>,
    /// Download queue, ordered by enqueue time.
    pub downloads: Vec<DownloadSnapshot>,
    /// Update service state.
    pub update: UpdateState,
    /// Plugin integration state.
    pub plugin: PluginStatus,
    /// Authentication state.
    pub auth: AuthState,
    /// On-screen toasts.
    pub toasts: Vec<ToastItem>,
    /// Long-running operations.
    pub busy: BTreeSet<&'static str>,
    /// In-app console.
    pub logs: VecDeque<LogEntry>,
    /// Current page.
    pub page: Page,
    /// Search text shared by list pages.
    pub search: String,
    /// Page states.
    pub pages: Pages,
    /// Open launch-options dialog.
    pub launch_options: Option<LaunchOptionsDialog>,
    /// Pending confirmation dialog.
    pub confirm: Option<ConfirmDialog>,
    /// Set when the backend asks the shell to reveal the window.
    pub show_window: bool,
    /// Apps whose staged launch edits no longer match appinfo.
    pub drifted_launches: Vec<AppId>,
    /// Loaded cover textures, keyed by app.
    covers: HashMap<AppId, egui::TextureHandle>,
}

/// A destructive action awaiting confirmation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmDialog {
    /// Dialog title.
    pub title: String,
    /// Body text.
    pub body: String,
    /// Label of the confirming button.
    pub confirm_label: String,
    /// Command sent when confirmed.
    pub action: Command,
}

impl AppState {
    /// Builds the initial state from persisted settings.
    #[must_use]
    pub fn new(backend: BackendHandle, settings: Settings) -> Self {
        let dark_mode = !matches!(settings.theme, luatools_core::ThemePreference::Light);
        let i18n = Translator::new(settings.language);
        let page = settings
            .last_page
            .as_deref()
            .and_then(Page::from_id)
            .unwrap_or(Page::Home);
        Self {
            backend,
            theme: if dark_mode {
                Theme::dark()
            } else {
                Theme::light()
            },
            dark_mode,
            i18n,
            settings,
            snapshot: LibrarySnapshot::default(),
            details: HashMap::new(),
            downloads: Vec::new(),
            update: UpdateState::Idle,
            plugin: PluginStatus::NotInstalled,
            auth: AuthState::Guest,
            toasts: Vec::new(),
            busy: BTreeSet::new(),
            logs: VecDeque::with_capacity(64),
            page,
            search: String::new(),
            pages: Pages::default(),
            launch_options: None,
            confirm: None,
            show_window: false,
            drifted_launches: Vec::new(),
            covers: HashMap::new(),
        }
    }

    /// Translates a key with the active locale.
    #[must_use]
    pub fn t(&self, key: Key) -> &'static str {
        self.i18n.t(key)
    }

    /// Translates and formats a key.
    #[must_use]
    pub fn tf(&self, key: Key, args: &[&str]) -> String {
        self.i18n.format(key, args)
    }

    /// Sends a command to the backend.
    pub fn send(&self, command: Command) {
        if !self.backend.send(command.clone()) {
            tracing::error!(?command, "backend channel closed");
        }
    }

    /// Switches the interface language and persists it.
    pub fn set_locale(&mut self, locale: luatools_core::Locale) {
        self.i18n.set_locale(locale);
        self.settings.language = locale;
        self.save_settings();
    }

    /// Persists the current settings.
    pub fn save_settings(&self) {
        self.send(Command::SaveSettings(Box::new(self.settings.clone())));
    }

    /// Whether an operation is in flight.
    #[must_use]
    pub fn is_busy(&self, key: &str) -> bool {
        self.busy.contains(key)
    }

    /// Returns the download with the given id.
    #[must_use]
    pub fn download(&self, id: DownloadId) -> Option<&DownloadSnapshot> {
        self.downloads.iter().find(|d| d.id == id)
    }

    /// Unfinished jobs, including paused ones, oldest first.
    pub fn active_downloads(&self) -> impl Iterator<Item = &DownloadSnapshot> {
        self.downloads.iter().filter(|d| !d.status.is_finished())
    }

    /// Finished jobs, newest first.
    pub fn finished_downloads(&self) -> impl Iterator<Item = &DownloadSnapshot> {
        self.downloads
            .iter()
            .rev()
            .filter(|d| d.status.is_finished())
    }

    /// Details for the app, if loaded.
    #[must_use]
    pub fn details(&self, id: AppId) -> Option<&AppDetails> {
        self.details.get(&id)
    }

    /// Requests details when they are not cached yet.
    pub fn ensure_details(&mut self, id: AppId) {
        if !self.details.contains_key(&id) {
            self.send(Command::LoadAppDetails(id));
        }
    }

    /// Opens the launch-options editor for an app.
    pub fn open_launch_options(&mut self, id: AppId) {
        let Some(app) = self.snapshot.app(id) else {
            return;
        };
        let Some(details) = self.details.get(&id) else {
            return;
        };
        self.launch_options = Some(LaunchOptionsDialog {
            app_id: id,
            app_name: app.name.clone(),
            entries: details.launch_entries.clone(),
            local_options: details.launch_options.clone(),
            selected: 0,
            error: None,
        });
    }

    /// Loads a cover texture from disk, caching it by app id.
    pub fn cover_texture(
        &mut self,
        ctx: &egui::Context,
        app_id: AppId,
        path: Option<&std::path::Path>,
    ) -> Option<egui::TextureHandle> {
        if let Some(texture) = self.covers.get(&app_id) {
            return Some(texture.clone());
        }
        let path = path?;
        let image = image::open(path).ok()?.to_rgba8();
        let size = [image.width() as usize, image.height() as usize];
        let pixels = image.into_raw();
        let texture = ctx.load_texture(
            format!("cover-{app_id}"),
            egui::ColorImage::from_rgba_unmultiplied(size, &pixels),
            egui::TextureOptions::LINEAR,
        );
        self.covers.insert(app_id, texture.clone());
        Some(texture)
    }

    /// Drops a cached cover so the next frame reloads it.
    pub fn invalidate_cover(&mut self, app_id: AppId) {
        self.covers.remove(&app_id);
    }

    /// Pushes a toast, keeping the newest four.
    pub fn push_toast(&mut self, toast: Toast) {
        self.toasts.push(ToastItem::new(toast));
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    /// Applies every event available without blocking.
    pub fn drain_events(&mut self, rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>) {
        while let Ok(event) = rx.try_recv() {
            self.apply_event(event);
        }
    }

    /// Folds one backend event into the state.
    pub fn apply_event(&mut self, event: Event) {
        match event {
            Event::LibraryRefreshed(snapshot) => {
                self.snapshot = *snapshot;
            }
            Event::AppDetailsLoaded(details) => {
                self.details.insert(details.app.id, *details);
            }
            Event::StoreDetailsLoaded(details) => {
                self.pages.add.selected = Some(details.app.id);
                self.pages.add.searching = false;
                self.details.insert(details.app.id, *details);
            }
            Event::StoreSearchResults { query, items } => {
                if query == self.pages.add.last_query {
                    self.pages.add.results = items;
                    self.pages.add.searching = false;
                }
            }
            Event::FeaturedLoaded(featured) => {
                self.pages.add.featured = *featured;
            }
            Event::AppMetadataLoaded { app_id, metadata } => {
                if let Some(entry) = self
                    .snapshot
                    .apps
                    .iter_mut()
                    .find(|entry| entry.id == app_id)
                {
                    apply_metadata(entry, &metadata);
                }
                if let Some(details) = self.details.get_mut(&app_id) {
                    apply_metadata(&mut details.app, &metadata);
                    details.metadata = Some(*metadata);
                }
            }
            Event::DepotInfoLoaded(info) => {
                if let Some(details) = self.details.get_mut(&info.app_id) {
                    details.apply_depot_info(&info);
                }
            }
            Event::CoverUpdated { app_id, path } => {
                if let Some(entry) = self
                    .snapshot
                    .apps
                    .iter_mut()
                    .find(|entry| entry.id == app_id)
                {
                    entry.cover_path = Some(path.clone());
                }
                if let Some(details) = self.details.get_mut(&app_id) {
                    details.app.cover_path = Some(path);
                }
                self.invalidate_cover(app_id);
            }
            Event::LaunchOptionsDrifted(apps) => self.drifted_launches = apps,
            Event::AuthState(state) => {
                if let AuthState::Failed { reason } = &state {
                    self.push_toast(Toast::error(reason.clone()));
                }
                self.auth = state;
            }
            Event::Navigate(target) => self.page = Page::from_nav(target),
            Event::ShowWindow => self.show_window = true,
            Event::DownloadUpdated(snapshot) => {
                let snapshot = *snapshot;
                match self.downloads.iter_mut().find(|d| d.id == snapshot.id) {
                    Some(existing) => *existing = snapshot,
                    None => self.downloads.push(snapshot),
                }
            }
            Event::DownloadRemoved(id) => {
                self.downloads.retain(|d| d.id != id);
            }
            Event::DownloadHistoryCleared => {
                self.downloads.retain(|d| !d.status.is_finished());
            }
            Event::SettingsSaved(settings) => {
                self.settings = *settings;
                self.i18n.set_locale(self.settings.language);
                self.pages.settings.draft = None;
                self.pages.settings.dirty = false;
            }
            Event::LaunchOptionsSaved(app_id) => {
                if let Some(details) = self.details.get_mut(&app_id) {
                    if let Some(dialog) = self.launch_options.take() {
                        details.launch_entries = dialog.entries;
                        details.launch_options = dialog.local_options;
                    }
                } else {
                    self.launch_options = None;
                }
                self.push_toast(Toast::success("Launch options saved"));
            }
            Event::DepotSelectionSaved(_) => {
                self.push_toast(Toast::success("Depot selection saved"));
            }
            Event::UpdateState(state) => self.update = state,
            Event::PluginStatus(status) => self.plugin = status,
            Event::Toast(toast) => self.push_toast(toast),
            Event::Log { level, message } => {
                self.logs.push_back(LogEntry { level, message });
                if self.logs.len() > LOG_CAPACITY {
                    self.logs.pop_front();
                }
            }
            Event::Error { context, message } => {
                self.logs.push_back(LogEntry {
                    level: LogLevel::Error,
                    message: format!("{context}: {message}"),
                });
                self.push_toast(Toast::error(format!("{context}: {message}")));
            }
            Event::Busy { key, active } => {
                if active {
                    self.busy.insert(key);
                } else {
                    self.busy.remove(key);
                }
            }
            // `Event` is non-exhaustive: future backend events are ignored rather than breaking
            // every build, but they are logged so a missing handler is visible.
            other => {
                tracing::debug!(?other, "unhandled backend event");
            }
        }
    }
}

fn apply_metadata(entry: &mut luatools_core::AppEntry, metadata: &luatools_core::AppMetadata) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use luatools_core::{AppEntry, DownloadRequest};

    fn state() -> AppState {
        AppState::new(BackendHandle::disconnected(), Settings::default())
    }

    #[test]
    fn library_event_replaces_snapshot() {
        let mut state = state();
        state.apply_event(Event::LibraryRefreshed(Box::new(LibrarySnapshot {
            apps: vec![AppEntry::new(AppId::new(730), "CS2")],
            ..LibrarySnapshot::default()
        })));
        assert_eq!(state.snapshot.apps.len(), 1);
        assert!(state.snapshot.app(AppId::new(730)).is_some());
    }

    #[test]
    fn download_events_insert_then_update() {
        let mut state = state();
        let request = DownloadRequest {
            app_id: AppId::new(1),
            app_name: "a".into(),
            kind: luatools_core::DownloadKind::Depot,
            depots: Vec::new(),
            url: None,
            destination: "/tmp".into(),
            verify: false,
        };
        let snap = DownloadSnapshot::queued(DownloadId::new(9), request, Utc::now());
        state.apply_event(Event::DownloadUpdated(Box::new(snap.clone())));
        assert_eq!(state.downloads.len(), 1);
        let mut updated = snap;
        updated.status = luatools_core::DownloadStatus::Completed;
        state.apply_event(Event::DownloadUpdated(Box::new(updated)));
        assert_eq!(state.downloads.len(), 1);
        assert_eq!(state.finished_downloads().count(), 1);
    }

    #[test]
    fn clearing_history_keeps_active_jobs() {
        let mut state = state();
        let request = DownloadRequest {
            app_id: AppId::new(1),
            app_name: "a".into(),
            kind: luatools_core::DownloadKind::File,
            depots: Vec::new(),
            url: Some("https://example.invalid/x".into()),
            destination: "/tmp".into(),
            verify: false,
        };
        let mut done = DownloadSnapshot::queued(DownloadId::new(1), request.clone(), Utc::now());
        done.status = luatools_core::DownloadStatus::Completed;
        let active = DownloadSnapshot::queued(DownloadId::new(2), request, Utc::now());
        state.apply_event(Event::DownloadUpdated(Box::new(done)));
        state.apply_event(Event::DownloadUpdated(Box::new(active)));
        state.apply_event(Event::DownloadHistoryCleared);
        assert_eq!(state.downloads.len(), 1);
        assert_eq!(state.downloads[0].id, DownloadId::new(2));
    }

    #[test]
    fn errors_become_toasts_and_log_lines() {
        let mut state = state();
        state.apply_event(Event::Error {
            context: "refresh library".into(),
            message: "no steam root".into(),
        });
        assert_eq!(state.toasts.len(), 1);
        assert_eq!(state.toasts[0].kind, ToastKind::Error);
        assert_eq!(state.logs.len(), 1);
    }

    #[test]
    fn busy_keys_track_completion() {
        let mut state = state();
        state.apply_event(Event::Busy {
            key: "library",
            active: true,
        });
        assert!(state.is_busy("library"));
        state.apply_event(Event::Busy {
            key: "library",
            active: false,
        });
        assert!(!state.is_busy("library"));
    }

    #[test]
    fn log_capacity_is_bounded() {
        let mut state = state();
        for i in 0..LOG_CAPACITY + 20 {
            state.apply_event(Event::Log {
                level: LogLevel::Info,
                message: format!("line {i}"),
            });
        }
        assert_eq!(state.logs.len(), LOG_CAPACITY);
        assert!(state.logs.back().unwrap().message.ends_with("519"));
    }

    #[test]
    fn language_switch_updates_translator_and_settings() {
        let mut state = state();
        state.set_locale(luatools_core::Locale::Fr);
        assert_eq!(state.i18n.locale(), luatools_core::Locale::Fr);
        assert_eq!(state.settings.language, luatools_core::Locale::Fr);
    }
    #[test]
    fn metadata_and_navigation_events_update_state() {
        let mut state = state();
        state.apply_event(Event::LibraryRefreshed(Box::new(LibrarySnapshot {
            apps: vec![AppEntry::new(AppId::new(730), "730")],
            ..LibrarySnapshot::default()
        })));
        state.apply_event(Event::AppMetadataLoaded {
            app_id: AppId::new(730),
            metadata: Box::new(luatools_core::AppMetadata {
                name: Some("Counter-Strike 2".to_owned()),
                header_image: Some("https://cdn/730.jpg".to_owned()),
                release_year: Some(2012),
                genres: vec!["Action".to_owned()],
                ..luatools_core::AppMetadata::default()
            }),
        });
        let entry = state.snapshot.app(AppId::new(730)).expect("entry");
        assert_eq!(entry.name, "Counter-Strike 2");
        assert_eq!(entry.cover_url.as_deref(), Some("https://cdn/730.jpg"));
        assert_eq!(
            entry.metadata.as_ref().and_then(|m| m.release_year),
            Some(2012)
        );
        state.apply_event(Event::Navigate(luatools_core::NavTarget::Add));
        assert_eq!(state.page, Page::Add);
        state.apply_event(Event::ShowWindow);
        assert!(state.show_window);
    }

    #[test]
    fn failed_auth_becomes_a_toast() {
        let mut state = state();
        state.apply_event(Event::AuthState(AuthState::Failed {
            reason: "not configured".to_owned(),
        }));
        assert_eq!(state.toasts.len(), 1);
        assert!(matches!(state.auth, AuthState::Failed { .. }));
    }
}
