//! Download queue view models shared by the UI and the downloader.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{AppId, DepotId, DownloadId, ManifestId};

/// What a job is transferring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DownloadKind {
    /// Steam depot content.
    Depot,
    /// A single file (updater, plugin, manifest).
    File,
}

/// One depot pinned to a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotRequest {
    /// Depot to fetch.
    pub depot_id: DepotId,
    /// Manifest to pin, when the caller chose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_id: Option<ManifestId>,
    /// Display name.
    pub name: String,
}

/// A queued download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    /// Steam app the depots belong to.
    pub app_id: AppId,
    /// Display name shown in the queue.
    pub app_name: String,
    /// What to transfer.
    pub kind: DownloadKind,
    /// Depot list for `Depot` jobs.
    #[serde(default)]
    pub depots: Vec<DepotRequest>,
    /// Source URL for `File` jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Destination directory.
    pub destination: PathBuf,
    /// Verify hashes after transfer.
    #[serde(default)]
    pub verify: bool,
}

/// Lifecycle of one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DownloadStatus {
    /// Waiting for a worker.
    Queued,
    /// Resolving manifests and connecting.
    Preparing,
    /// Transferring bytes.
    Downloading,
    /// Paused by the user.
    Paused,
    /// Checking hashes on disk.
    Verifying,
    /// Finished successfully.
    Completed,
    /// Stopped with an error.
    Failed,
    /// Cancelled by the user.
    Canceled,
}

impl DownloadStatus {
    /// Whether the job occupies a worker slot.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Preparing | Self::Downloading | Self::Verifying
        )
    }

    /// Whether the job is finished, successfully or not.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }

    /// Whether the user may pause it.
    #[must_use]
    pub const fn can_pause(self) -> bool {
        matches!(self, Self::Queued | Self::Preparing | Self::Downloading)
    }

    /// Whether the user may resume it.
    #[must_use]
    pub const fn can_resume(self) -> bool {
        matches!(self, Self::Paused)
    }

    /// Whether the user may retry it.
    #[must_use]
    pub const fn can_retry(self) -> bool {
        matches!(self, Self::Failed | Self::Canceled)
    }
}

/// One immutable row in the queue list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSnapshot {
    /// Stable job id.
    pub id: DownloadId,
    /// Original request.
    pub request: DownloadRequest,
    /// Current lifecycle state.
    pub status: DownloadStatus,
    /// Bytes already written.
    pub received_bytes: u64,
    /// Total bytes when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// Current throughput in bytes per second.
    #[serde(default)]
    pub bytes_per_second: u64,
    /// Estimated seconds remaining.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_seconds: Option<u64>,
    /// Files finished.
    #[serde(default)]
    pub files_done: u64,
    /// Files expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_total: Option<u64>,
    /// Localized error for `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the job was enqueued.
    pub queued_at: DateTime<Utc>,
    /// When it last changed state.
    pub updated_at: DateTime<Utc>,
}

impl DownloadSnapshot {
    /// Creates a queued snapshot for a new request.
    #[must_use]
    pub fn queued(id: DownloadId, request: DownloadRequest, now: DateTime<Utc>) -> Self {
        Self {
            id,
            request,
            status: DownloadStatus::Queued,
            received_bytes: 0,
            total_bytes: None,
            bytes_per_second: 0,
            eta_seconds: None,
            files_done: 0,
            files_total: None,
            error: None,
            queued_at: now,
            updated_at: now,
        }
    }

    /// Fraction complete in `0.0..=1.0`, or `None` while the total is unknown.
    #[must_use]
    pub fn fraction(&self) -> Option<f32> {
        match self.total_bytes {
            Some(total) if total > 0 => {
                Some((self.received_bytes as f32 / total as f32).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }

    /// Human-readable progress, e.g. `1.4 GB of 2.0 GB`.
    #[must_use]
    pub fn progress_label(&self) -> String {
        use crate::byte_format::ByteFormat;
        match self.total_bytes {
            Some(total) => format!(
                "{} of {}",
                ByteFormat::new(self.received_bytes),
                ByteFormat::new(total)
            ),
            None => ByteFormat::new(self.received_bytes).to_string(),
        }
    }
}

/// Final outcome reported when a job leaves the worker pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// Every requested file was written and verified.
    Completed {
        /// Bytes written.
        bytes: u64,
    },
    /// The job failed.
    Failed {
        /// Localized reason.
        reason: String,
    },
    /// The user cancelled.
    Canceled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> DownloadRequest {
        DownloadRequest {
            app_id: AppId::new(730),
            app_name: "Counter-Strike 2".to_owned(),
            kind: DownloadKind::Depot,
            depots: vec![DepotRequest {
                depot_id: DepotId::new(731),
                manifest_id: Some(ManifestId::new(42)),
                name: "CS2 content".to_owned(),
            }],
            url: None,
            destination: PathBuf::from("/games/cs2"),
            verify: true,
        }
    }

    #[test]
    fn new_jobs_start_queued_and_active() {
        let snap = DownloadSnapshot::queued(DownloadId::new(1), request(), Utc::now());
        assert_eq!(snap.status, DownloadStatus::Queued);
        assert!(snap.status.is_active());
        assert_eq!(snap.fraction(), None);
    }

    #[test]
    fn fraction_is_clamped_and_optional() {
        let mut snap = DownloadSnapshot::queued(DownloadId::new(1), request(), Utc::now());
        snap.total_bytes = Some(200);
        snap.received_bytes = 50;
        assert_eq!(snap.fraction(), Some(0.25));
        snap.received_bytes = 500;
        assert_eq!(snap.fraction(), Some(1.0));
        snap.total_bytes = Some(0);
        assert_eq!(snap.fraction(), None);
    }

    #[test]
    fn status_transitions_advertise_the_right_actions() {
        assert!(DownloadStatus::Downloading.can_pause());
        assert!(!DownloadStatus::Downloading.can_resume());
        assert!(DownloadStatus::Paused.can_resume());
        assert!(DownloadStatus::Failed.can_retry());
        assert!(!DownloadStatus::Completed.can_retry());
        assert!(DownloadStatus::Completed.is_finished());
        assert!(!DownloadStatus::Queued.is_finished());
    }

    #[test]
    fn progress_label_scales_units() {
        let mut snap = DownloadSnapshot::queued(DownloadId::new(1), request(), Utc::now());
        snap.total_bytes = Some(2_000_000_000);
        snap.received_bytes = 1_400_000_000;
        assert_eq!(snap.progress_label(), "1.4 GB of 2.0 GB");
    }
}
