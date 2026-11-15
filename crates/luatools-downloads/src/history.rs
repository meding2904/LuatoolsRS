//! Persistent download history.
//!
//! Only terminal jobs are recorded. The file is a JSON array ordered newest first, capped at
//! [`HistoryStore::DEFAULT_CAP`] entries, and replaced atomically so an interrupted write never
//! leaves a truncated history behind.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use luatools_core::{AppId, DownloadId, DownloadKind, DownloadStatus};
use serde::{Deserialize, Serialize};

use crate::error::{DownloadError, Result};

/// Distinguishes concurrent temporary files within one process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One finished download, as persisted in the history file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Identifier the job had while it was in the queue.
    pub id: DownloadId,
    /// Steam app the job belonged to, if any.
    pub app_id: AppId,
    /// Whether the job was a depot job or a file job.
    pub kind: DownloadKind,
    /// Display name shown in the history list.
    pub title: String,
    /// Bytes transferred before the job settled.
    pub bytes: u64,
    /// Terminal state: `Completed`, `Failed`, or `Canceled`.
    pub status: DownloadStatus,
    /// Failure or cancellation text, when there is one.
    pub message: Option<String>,
    /// When the job settled.
    pub completed_at: DateTime<Utc>,
    /// File or directory the job produced, for "show in folder".
    pub output: Option<PathBuf>,
}

impl HistoryEntry {
    /// Whether this entry records a failure.
    #[must_use]
    pub const fn failed(&self) -> bool {
        matches!(self.status, DownloadStatus::Failed)
    }
}

/// Reader and atomic writer for the history JSON file.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    path: PathBuf,
    cap: usize,
}

impl HistoryStore {
    /// Maximum number of entries retained by default.
    pub const DEFAULT_CAP: usize = 100;

    /// Creates a store with the default cap of 100 entries.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            cap: Self::DEFAULT_CAP,
        }
    }

    /// Creates a store with an explicit cap. A cap of zero keeps no entries.
    #[must_use]
    pub fn with_cap(path: impl Into<PathBuf>, cap: usize) -> Self {
        Self {
            path: path.into(),
            cap,
        }
    }

    /// Path of the JSON file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Maximum number of entries retained.
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Reads the history, newest first.
    ///
    /// A missing file is an empty history, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError::Core`] wrapping an I/O error when the file cannot be read, or a
    /// serialization error when it is not valid history JSON.
    pub fn load(&self) -> Result<Vec<HistoryEntry>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(DownloadError::io(
                    format!("reading download history {}", self.path.display()),
                    error,
                ));
            }
        };

        let mut entries: Vec<HistoryEntry> = serde_json::from_slice(&bytes)?;
        sort_and_cap(&mut entries, self.cap);
        Ok(entries)
    }

    /// Replaces the history file with `entries`, newest first.
    ///
    /// The write goes to a temporary file in the same directory and is renamed into place, so
    /// a reader either sees the previous complete file or the new complete file.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError::Core`] when the parent directory cannot be created, the
    /// temporary file cannot be written, or the rename fails. The temporary file is removed on
    /// failure.
    pub fn save(&self, entries: &[HistoryEntry]) -> Result<()> {
        let mut normalised = entries.to_vec();
        sort_and_cap(&mut normalised, self.cap);

        let bytes = serde_json::to_vec_pretty(&normalised)?;

        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| {
                DownloadError::io(
                    format!("creating download history directory {}", parent.display()),
                    error,
                )
            })?;
        }

        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("history.json");
        let temp = self.path.with_file_name(format!(
            ".{file_name}.{}.{sequence}.tmp",
            std::process::id()
        ));

        if let Err(error) = write_and_replace(&temp, &self.path, &bytes) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }

        Ok(())
    }
}

/// Writes `bytes` to `temp`, syncs it, and renames it over `target`.
fn write_and_replace(temp: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(temp)
        .map_err(|error| DownloadError::io(format!("creating {}", temp.display()), error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| DownloadError::io(format!("writing {}", temp.display()), error))?;
    drop(file);

    match std::fs::rename(temp, target) {
        Ok(()) => Ok(()),
        Err(first) => {
            // Windows cannot rename over an existing file. Fall back to remove-then-rename;
            // readers may briefly see no file, but never a partial one.
            if target.exists() {
                std::fs::remove_file(target).map_err(|error| {
                    DownloadError::io(format!("replacing {}", target.display()), error)
                })?;
                std::fs::rename(temp, target).map_err(|error| {
                    DownloadError::io(format!("renaming {}", target.display()), error)
                })
            } else {
                Err(DownloadError::io(
                    format!("renaming {} to {}", temp.display(), target.display()),
                    first,
                ))
            }
        }
    }
}

/// Sorts newest first and truncates to `cap`.
fn sort_and_cap(entries: &mut Vec<HistoryEntry>, cap: usize) {
    entries.sort_by(|left, right| {
        right
            .completed_at
            .cmp(&left.completed_at)
            .then_with(|| right.id.cmp(&left.id))
    });
    entries.truncate(cap);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn entry(index: u64) -> HistoryEntry {
        HistoryEntry {
            id: DownloadId::new(index),
            app_id: AppId::new(730),
            kind: DownloadKind::File,
            title: format!("download {index}"),
            bytes: index * 10,
            status: DownloadStatus::Completed,
            message: None,
            completed_at: Utc::now() + ChronoDuration::seconds(index as i64),
            output: Some(PathBuf::from(format!("/tmp/file-{index}.zip"))),
        }
    }

    fn temp_store() -> (tempfile::TempDir, HistoryStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = HistoryStore::new(dir.path().join("history.json"));
        (dir, store)
    }

    #[test]
    fn missing_file_is_an_empty_history() {
        let (_dir, store) = temp_store();
        assert_eq!(store.load().expect("load"), Vec::new());
    }

    #[test]
    fn entries_round_trip_through_disk() {
        let (_dir, store) = temp_store();
        let entries = vec![entry(2), entry(1)];
        store.save(&entries).expect("save");
        assert_eq!(store.load().expect("load"), entries);
    }

    #[test]
    fn save_orders_newest_first() {
        let (_dir, store) = temp_store();
        // Deliberately shuffled input.
        store
            .save(&[entry(2), entry(5), entry(1), entry(4), entry(3)])
            .expect("save");
        let loaded = store.load().expect("load");
        let ids: Vec<u64> = loaded.iter().map(|e| e.id.get()).collect();
        assert_eq!(ids, vec![5, 4, 3, 2, 1]);
    }

    #[test]
    fn default_cap_keeps_the_newest_hundred_entries() {
        let (_dir, store) = temp_store();
        let entries: Vec<HistoryEntry> = (1..=105).map(entry).collect();
        store.save(&entries).expect("save");

        let loaded = store.load().expect("load");
        assert_eq!(loaded.len(), HistoryStore::DEFAULT_CAP);
        assert_eq!(loaded.first().map(|e| e.id.get()), Some(105));
        assert_eq!(loaded.last().map(|e| e.id.get()), Some(6));
    }

    #[test]
    fn explicit_cap_is_applied_on_load_and_save() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = HistoryStore::with_cap(dir.path().join("history.json"), 3);
        store
            .save(&(1..=10).map(entry).collect::<Vec<_>>())
            .expect("save");
        let loaded = store.load().expect("load");
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].id.get(), 10);
        assert_eq!(loaded[2].id.get(), 8);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temporary_files() {
        let (dir, store) = temp_store();
        store.save(&[entry(1)]).expect("first save");
        store.save(&[entry(2), entry(1)]).expect("second save");

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|item| {
                item.expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["history.json".to_owned()]);

        let loaded = store.load().expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id.get(), 2);
    }

    #[test]
    fn corrupt_history_reports_an_error() {
        let (dir, store) = temp_store();
        std::fs::write(dir.path().join("history.json"), b"not json").expect("seed");
        assert!(store.load().is_err());
    }

    #[test]
    fn a_write_failure_does_not_leave_a_temporary_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The target is a directory, so the rename cannot replace it.
        let target = dir.path().join("history.json");
        std::fs::create_dir(&target).expect("seed directory");
        let store = HistoryStore::new(target);

        assert!(store.save(&[entry(1)]).is_err());

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|item| {
                item.expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");
    }
}
