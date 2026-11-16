//! The message contract between the interface and the backend.
//!
//! The UI is immediate mode: it never blocks and never awaits. It sends [`Command`]s through
//! [`BackendHandle`] and folds [`Event`]s into its state on every frame. The backend owns the
//! runtime, the network, and the Steam probing, so the UI can be tested against a mock handle.

use std::path::PathBuf;

use crate::app::{AppDetails, LibrarySnapshot};
use crate::auth::AuthState;
use crate::depot::AppDepotInfo;
use crate::download::{DownloadRequest, DownloadSnapshot};
use crate::ids::AppId;
use crate::ids::DownloadId;
use crate::launch::LaunchOption;
use crate::settings::Settings;
use crate::store::{AppMetadata, Featured, StoreSearchItem};

/// Severity of a log line surfaced in the UI console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    /// Verbose diagnostics.
    Debug,
    /// Normal progress information.
    Info,
    /// Something recoverable went wrong.
    Warn,
    /// An operation failed.
    Error,
}

/// Visual tone of a toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToastKind {
    /// Neutral information.
    Info,
    /// Operation succeeded.
    Success,
    /// Operation succeeded with caveats.
    Warning,
    /// Operation failed.
    Error,
}

/// A transient message shown in the corner of the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// Tone of the message.
    pub kind: ToastKind,
    /// Rendered text, already localized by the backend when it carries a resource key.
    pub message: String,
}

impl Toast {
    /// Builds an informational toast.
    #[must_use]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Info,
            message: message.into(),
        }
    }

    /// Builds a success toast.
    #[must_use]
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Success,
            message: message.into(),
        }
    }

    /// Builds an error toast.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Error,
            message: message.into(),
        }
    }
}

/// State of the Steam store plugin integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    /// Not installed.
    NotInstalled,
    /// Installed and active.
    Installed {
        /// Installed version string.
        version: String,
    },
    /// A different version is available.
    UpdateAvailable {
        /// Installed version.
        installed: String,
        /// Available version.
        available: String,
    },
    /// An operation is in flight.
    Working {
        /// Localized step description.
        step: String,
    },
    /// The last operation failed.
    Failed {
        /// Localized failure reason.
        reason: String,
    },
}

/// Result of checking for application updates.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    /// No check has run.
    Idle,
    /// A check is in flight.
    Checking,
    /// Running the latest version.
    UpToDate {
        /// Current version.
        current: String,
    },
    /// A newer version exists.
    Available {
        /// Current version.
        current: String,
        /// Newest version.
        latest: String,
        /// Release notes, already rendered as plain text.
        notes: String,
    },
    /// The update is downloading.
    Downloading {
        /// 0.0 to 1.0.
        progress: f32,
    },
    /// Downloaded and waiting for a restart.
    ReadyToInstall {
        /// Version that will be installed.
        version: String,
    },
    /// The last check or download failed.
    Failed {
        /// Localized reason.
        reason: String,
    },
}

/// A destination the interface can navigate to without knowing the page enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavTarget {
    /// Dashboard.
    Home,
    /// Store search and featured catalogue.
    Add,
    /// Installed and owned library.
    Builds,
    /// Download queue.
    Downloads,
    /// Per-app management.
    Manage,
    /// Application settings.
    Settings,
    /// Steam store plugin.
    Plugin,
}

/// A request from the UI to the backend.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Command {
    /// Rescan every Steam library and refresh the app list.
    RefreshLibrary,
    /// Load depots, launch options, and manifest state for one app.
    LoadAppDetails(AppId),
    /// Search the public Steam store.
    SearchStore(String),
    /// Load the top-seller and new-release strips.
    LoadFeatured,
    /// Load public store metadata for an app that may not be installed.
    LoadStoreDetails(AppId),
    /// Resolve depot, branch, and DLC metadata from the public service.
    LoadDepotInfo(AppId),
    /// Refresh cached store metadata for one app.
    RefreshAppMetadata(AppId),
    /// Open a file or directory with the OS handler.
    OpenPath(PathBuf),
    /// Reveal a path in the file manager.
    RevealPath(PathBuf),
    /// Start a game through the Steam client.
    LaunchApp(AppId),
    /// Ask Steam to install or update an app (`steam://install/<id>`).
    InstallApp(AppId),
    /// Open the Steam store page for an app.
    OpenStorePage(AppId),
    /// Close and relaunch the Steam client.
    RestartSteam,
    /// Add a download job.
    EnqueueDownload(Box<DownloadRequest>),
    /// Queue a plain HTTP(S) file download into the configured download folder.
    EnqueueUrlDownload {
        /// Source URL.
        url: String,
        /// Optional display name; the response filename is used when omitted.
        name: Option<String>,
    },
    /// Temporarily stop a running job.
    PauseDownload(DownloadId),
    /// Resume a paused job.
    ResumeDownload(DownloadId),
    /// Cancel and discard a job.
    CancelDownload(DownloadId),
    /// Remove a finished or failed job from the list.
    RemoveDownload(DownloadId),
    /// Put a failed job back at the end of the queue.
    RetryDownload(DownloadId),
    /// Empty the finished-jobs list.
    ClearDownloadHistory,
    /// Persist and apply settings.
    SaveSettings(Box<Settings>),
    /// Re-read settings from disk.
    ReloadSettings,
    /// Ask the update service whether a newer build exists.
    CheckForUpdates,
    /// Download and stage the pending update.
    InstallUpdate,
    /// Open the folder containing a staged update.
    OpenUpdateFolder,
    /// Re-read the Steam store plugin's installation status.
    CheckPluginStatus,
    /// Install or update the Steam store plugin.
    InstallPlugin,
    /// Remove the Steam store plugin.
    UninstallPlugin,
    /// Write launch options for an app.
    SaveLaunchOptions {
        /// Target app.
        app_id: AppId,
        /// Options string, empty to clear.
        options: String,
    },
    /// Write the structured Play-button entries for an app.
    SaveLaunchEntries {
        /// Target app.
        app_id: AppId,
        /// Entries in display order.
        entries: Vec<LaunchOption>,
    },
    /// Restore Steam's original Play-button entries for an app.
    RestoreLaunchOptions(AppId),
    /// Re-apply every staged launch-option edit whose live appinfo changed.
    ReapplyLaunchOptions,
    /// Enable or disable depots for an app.
    SaveDepotSelection {
        /// Target app.
        app_id: AppId,
        /// Depot ids the user enabled.
        enabled: Vec<crate::ids::DepotId>,
    },
    /// Handle a `luatools://`, Steam, or store URL.
    HandleProtocolUrl(String),
    /// Begin the interactive sign-in flow.
    SignIn,
    /// Forget the stored session.
    SignOut,
    /// Cancel an in-flight sign-in.
    CancelSignIn,
    /// Open a URL in the default browser.
    OpenUrl(String),
    /// Shut down the application.
    Quit,
}

/// Notifications from the backend to the UI.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// A library scan finished.
    LibraryRefreshed(Box<LibrarySnapshot>),
    /// App details finished loading.
    AppDetailsLoaded(Box<AppDetails>),
    /// Store metadata for an app that may not be installed.
    StoreDetailsLoaded(Box<AppDetails>),
    /// Store search finished.
    StoreSearchResults {
        /// Query that produced the result, used to discard stale responses.
        query: String,
        /// Matching store entries.
        items: Vec<StoreSearchItem>,
    },
    /// Featured store strips finished loading.
    FeaturedLoaded(Box<Featured>),
    /// Cached store metadata for one app changed.
    AppMetadataLoaded {
        /// App the metadata belongs to.
        app_id: AppId,
        /// Parsed metadata.
        metadata: Box<AppMetadata>,
    },
    /// Depot, branch, and DLC metadata finished loading.
    DepotInfoLoaded(Box<AppDepotInfo>),
    /// A cover image was downloaded or discovered on disk.
    CoverUpdated {
        /// App the cover belongs to.
        app_id: AppId,
        /// Local image path.
        path: PathBuf,
    },
    /// Authentication state changed.
    AuthState(AuthState),
    /// The interface should navigate to a destination.
    Navigate(NavTarget),
    /// The main window should be shown and focused.
    ShowWindow,
    /// A download changed state or progressed.
    DownloadUpdated(Box<DownloadSnapshot>),
    /// A download left the queue.
    DownloadRemoved(DownloadId),
    /// The finished-jobs list was cleared.
    DownloadHistoryCleared,
    /// Settings were saved and are now active.
    SettingsSaved(Box<Settings>),
    /// Launch options were written.
    LaunchOptionsSaved(AppId),
    /// Staged launch edits were overwritten by Steam and differ from the live appinfo.
    LaunchOptionsDrifted(Vec<AppId>),
    /// Depot selection was written.
    DepotSelectionSaved(AppId),
    /// Update service changed state.
    UpdateState(UpdateState),
    /// Plugin integration changed state.
    PluginStatus(PluginStatus),
    /// Show a transient toast.
    Toast(Toast),
    /// A log line for the console.
    Log {
        /// Severity.
        level: LogLevel,
        /// Rendered line.
        message: String,
    },
    /// An operation failed; the UI decides whether to toast or inline it.
    Error {
        /// Short operation name, e.g. `refresh library`.
        context: String,
        /// Rendered reason.
        message: String,
    },
    /// A long-running operation started or finished.
    Busy {
        /// Stable operation key, e.g. `library`.
        key: &'static str,
        /// True while running.
        active: bool,
    },
}

/// Sender half of the command channel handed to the UI.
#[derive(Debug, Clone)]
pub struct BackendHandle {
    tx: tokio::sync::mpsc::UnboundedSender<Command>,
}

impl BackendHandle {
    /// Wraps the sending half of the command channel.
    #[must_use]
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<Command>) -> Self {
        Self { tx }
    }

    /// Queues a command.
    ///
    /// Returns `false` when the backend has shut down, which the UI treats as a fatal state.
    pub fn send(&self, command: Command) -> bool {
        self.tx.send(command).is_ok()
    }

    /// Returns a handle that records commands instead of forwarding them, for tests.
    #[must_use]
    pub fn disconnected() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        Self { tx }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_forwards_commands_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = BackendHandle::new(tx);
        assert!(handle.send(Command::RefreshLibrary));
        assert!(handle.send(Command::OpenUrl("https://lua.tools".to_owned())));
        assert!(matches!(rx.try_recv(), Ok(Command::RefreshLibrary)));
        assert!(matches!(rx.try_recv(), Ok(Command::OpenUrl(_))));
    }

    #[test]
    fn disconnected_handle_reports_failure() {
        assert!(!BackendHandle::disconnected().send(Command::Quit));
    }

    #[test]
    fn toast_helpers_set_the_tone() {
        assert_eq!(Toast::info("a").kind, ToastKind::Info);
        assert_eq!(Toast::success("b").kind, ToastKind::Success);
        assert_eq!(Toast::error("c").kind, ToastKind::Error);
    }
}
