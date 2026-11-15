//! Domain types, settings, cache layout, and the UI/backend command contracts.
//!
//! This crate is UI- and platform-agnostic: it holds the data the interface renders, the
//! messages exchanged with the backend, and the small services both sides share (paths,
//! settings persistence, byte formatting, hashing).
//!
//! The rule for dependents: `luatools-core` never depends on `egui`, and never performs
//! network or Steam work. Those live in the backend crates.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app;
pub mod backend;
pub mod byte_format;
pub mod cache;
pub mod download;
pub mod error;
pub mod hash;
pub mod ids;
pub mod locale;
pub mod paths;
pub mod settings;

pub use app::{AppDetails, AppEntry, DepotEntry, InstalledState, LibrarySnapshot};
pub use backend::{
    BackendHandle, Command, Event, LogLevel, PluginStatus, Toast, ToastKind, UpdateState,
};
pub use byte_format::ByteFormat;
pub use download::{
    DepotRequest, DownloadKind, DownloadOutcome, DownloadRequest, DownloadSnapshot, DownloadStatus,
};
pub use error::{Error, Result};
pub use ids::{AppId, DepotId, DownloadId, ManifestId};
pub use locale::Locale;
pub use paths::AppPaths;
pub use settings::{CloseBehavior, DownloadScheduling, Settings, ThemePreference};
