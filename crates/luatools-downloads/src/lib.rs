//! Download queue, generic transfer runner, and persistent history.
//!
//! The crate is deliberately narrower than the original GUI's download manager: it provides a
//! generic queue for plain file transfers, not depot-content acquisition. A [`Transfer`]
//! implementation does the bytes; [`HttpClientTransfer`] is the only implementation shipped
//! here and it accepts ordinary HTTP(S) file URLs. A request without a URL (a depot request)
//! therefore settles as `Failed` with an invalid-request message rather than downloading
//! content.
//!
//! Plain file transfers are staged, optionally paced to [`QueueConfig::speed_limit_bytes`],
//! verified against the advertised length (and hashed with SHA-256) when verification is on,
//! then moved into the request's destination directory.
//!
//! # Model
//!
//! [`DownloadQueue::spawn`] starts one Tokio task that owns all mutable state. The returned
//! [`QueueHandle`] communicates by messages, and every change is published as a
//! [`luatools_core::Event`]:
//!
//! * `DownloadUpdated` for enqueue, state changes, and throttled progress (about 10 Hz),
//! * `DownloadRemoved` when a finished row is dropped or retried,
//! * `DownloadHistoryCleared` when the history is emptied,
//! * `Error` for infrastructure failures such as a history write.
//!
//! Jobs are deduplicated by request key while unfinished, run up to [`QueueConfig::concurrency`]
//! at a time, and settle as `Completed`, `Failed`, or `Canceled`. Only terminal jobs enter
//! [`HistoryStore`], newest first and capped at 100 entries by default.
//!
//! # Example
//!
//! ```no_run
//! # async fn example() {
//! use std::path::PathBuf;
//! use luatools_core::{AppId, DownloadKind, DownloadRequest, Event};
//! use luatools_downloads::{DownloadQueue, HistoryStore, QueueConfig};
//!
//! let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel::<Event>();
//! let handle = DownloadQueue::spawn(
//!     QueueConfig::new(2, std::env::temp_dir().join("luatools-staging")),
//!     events_tx,
//!     Some(HistoryStore::new(PathBuf::from("/tmp/luatools/history.json"))),
//! );
//!
//! handle.enqueue(DownloadRequest {
//!     app_id: AppId::new(0),
//!     app_name: "LuaTools update".to_owned(),
//!     kind: DownloadKind::File,
//!     depots: Vec::new(),
//!     url: Some("https://example.com/tool.zip".to_owned()),
//!     destination: PathBuf::from("/tmp/downloads"),
//!     verify: false,
//! });
//!
//! while let Some(event) = events.recv().await {
//!     println!("{event:?}");
//! }
//! # }
//! ```
//!
//! Pillars of the implementation:
//!
//! * [`queue`] — actor, handle, deduplication, pause/resume/cancel/retry.
//! * [`transfer`] — the [`Transfer`] trait and streaming [`HttpClientTransfer`].
//! * [`history`] — atomic, capped, newest-first JSON persistence.
//! * [`filename`] — sanitisation for untrusted remote names.
//! * [`progress`] — byte readings, `fraction()`, and the 10 Hz emission throttle.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod error;
pub mod filename;
pub mod history;
pub mod progress;
pub mod queue;
mod rate;
pub mod transfer;

pub use error::{DownloadError, Result};
pub use filename::sanitize_filename;
pub use history::{HistoryEntry, HistoryStore};
pub use progress::{ProgressThrottle, TransferProgress};
pub use queue::{DownloadQueue, QueueCommand, QueueConfig, QueueHandle};
pub use transfer::{
    HttpClientTransfer, Transfer, TransferOutcome, filename_from_content_disposition,
    filename_from_url,
};
