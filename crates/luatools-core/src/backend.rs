//! The message contract between the interface and the backend.
//!
//! The UI is immediate mode: it never blocks and never awaits. It sends [`Command`]s through
//! [`BackendHandle`] and folds [`Event`]s into its state on every frame. The backend owns the
//! runtime, the network, and the Steam probing, so the UI can be tested against a mock handle.

use std::path::PathBuf;

use crate::app::{AppDetails, LibrarySnapshot};
use crate::download::{DownloadRequest, DownloadSnapshot};
use crate::ids::AppId;
use crate::ids::DownloadId;
use crate::settings::Settings;

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

/// A request from the UI to the backend.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Command {
    /// Rescan every Steam library and refresh the app list.
    RefreshLibrary,
    /// Load depots, launch options, and manifest state for one app.
    LoadAppDetails(AppId),
    /// Open a file or directory with the OS handler.
    OpenPath(PathBuf),
    /// Reveal a path in the file manager.
    RevealPath(PathBuf),
    /// Start a game through the Steam client.
    LaunchApp(AppId),
    /// Add a download job.
    EnqueueDownload(Box<DownloadRequest>),
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
    /// Enable or disable depots for an app.
    SaveDepotSelection {
        /// Target app.
        app_id: AppId,
        /// Depot ids the user enabled.
        enabled: Vec<crate::ids::DepotId>,
    },
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
