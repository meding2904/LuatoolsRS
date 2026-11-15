//! The single-owner download queue actor.
//!
//! [`DownloadQueue::spawn`] starts one task that owns every piece of mutable queue state: the
//! item list, the set of running transfers, the in-memory history, and the id counter. Callers
//! interact through [`QueueHandle`], which sends [`QueueCommand`] messages. No state is shared
//! behind a lock, so a snapshot read through [`QueueHandle::list`] is always consistent.
//!
//! Each attempt runs a [`Transfer`] implementation on Tokio's blocking pool and reports
//! progress back to the actor. Progress events are throttled to ten per second; state changes
//! are emitted immediately.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use luatools_core::{DownloadId, DownloadRequest, DownloadSnapshot, DownloadStatus, Event};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::error::{DownloadError, Result};
use crate::history::{HistoryEntry, HistoryStore};
use crate::progress::{ProgressThrottle, TransferProgress};
use crate::rate::RateMeter;
use crate::transfer::{HttpClientTransfer, Transfer};

/// Queue limits and staging location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConfig {
    /// Maximum number of transfers that may run at once. Values below one behave as one.
    pub concurrency: usize,
    /// Maximum number of history entries kept in memory and on disk.
    pub history_cap: usize,
    /// Directory plain-HTTP transfers are staged in.
    pub staging_dir: PathBuf,
}

impl QueueConfig {
    /// Creates a configuration with the default history cap of 100 entries.
    #[must_use]
    pub fn new(concurrency: usize, staging_dir: impl Into<PathBuf>) -> Self {
        Self {
            concurrency,
            history_cap: HistoryStore::DEFAULT_CAP,
            staging_dir: staging_dir.into(),
        }
    }

    /// Overrides the history cap.
    #[must_use]
    pub fn with_history_cap(mut self, history_cap: usize) -> Self {
        self.history_cap = history_cap;
        self
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self::new(3, std::env::temp_dir().join("LuaTools").join("downloads"))
    }
}

/// A message accepted by the queue actor.
#[derive(Debug)]
#[non_exhaustive]
pub enum QueueCommand {
    /// Adds a request unless an unfinished job already has the same request key.
    Enqueue(Box<DownloadRequest>),
    /// Parks a queued or running job. A running transfer is cancelled and restarts on resume.
    Pause(DownloadId),
    /// Returns a paused job to the queue.
    Resume(DownloadId),
    /// Stops a job and settles it as `Canceled`.
    Cancel(DownloadId),
    /// Re-enqueues a failed or cancelled job as a fresh item at the tail.
    Retry(DownloadId),
    /// Drops a finished item from the live list, keeping its history row.
    Remove(DownloadId),
    /// Empties the history in memory and on disk.
    ClearHistory,
    /// Reads the current live items. Used by [`QueueHandle::list`].
    List {
        /// Reply channel for the snapshot list.
        reply: oneshot::Sender<Vec<DownloadSnapshot>>,
    },
    /// Reads the in-memory history. Used by [`QueueHandle::history`].
    History {
        /// Reply channel for the history entries.
        reply: oneshot::Sender<Vec<HistoryEntry>>,
    },
    /// Cancels every unfinished job, records it as interrupted, persists, and exits.
    Shutdown {
        /// Optional acknowledgement sent after the final history write.
        reply: Option<oneshot::Sender<()>>,
    },
}

/// Entry point for the download queue.
///
/// The type is stateless; all queue state lives inside the task started by [`DownloadQueue::spawn`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DownloadQueue;

impl DownloadQueue {
    /// Starts the queue with the default [`HttpClientTransfer`] for plain file URLs.
    ///
    /// Must be called from within a Tokio runtime. The returned handle can be cloned freely;
    /// when every handle is dropped the actor settles unfinished jobs and exits.
    #[must_use]
    pub fn spawn(
        config: QueueConfig,
        events: mpsc::UnboundedSender<Event>,
        history: Option<HistoryStore>,
    ) -> QueueHandle {
        let transfer: Arc<dyn Transfer> =
            Arc::new(HttpClientTransfer::new(config.staging_dir.clone()));
        Self::spawn_with(config, events, history, transfer)
    }

    /// Starts the queue with a caller-supplied transfer implementation.
    ///
    /// This is the injection point for tests and for future transfer kinds. The queue itself
    /// stays generic; it never inspects the request beyond keying it for deduplication.
    ///
    /// Must be called from within a Tokio runtime.
    #[must_use]
    pub fn spawn_with(
        config: QueueConfig,
        events: mpsc::UnboundedSender<Event>,
        history: Option<HistoryStore>,
        transfer: Arc<dyn Transfer>,
    ) -> QueueHandle {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (job_tx, job_rx) = mpsc::unbounded_channel();

        let actor = Actor::new(config, events, history, transfer, job_tx);
        tokio::spawn(actor.run(command_rx, job_rx));

        QueueHandle { tx: command_tx }
    }
}

/// A cheap, cloneable handle to the running queue.
#[derive(Debug, Clone)]
pub struct QueueHandle {
    tx: mpsc::UnboundedSender<QueueCommand>,
}

impl QueueHandle {
    /// Queues a new job.
    ///
    /// Returns `false` when the queue has already stopped. A request whose key matches an
    /// unfinished job is ignored by the actor (deduplication), which is not visible here.
    pub fn enqueue(&self, request: DownloadRequest) -> bool {
        self.send(QueueCommand::Enqueue(Box::new(request)))
    }

    /// Pauses a queued or running job.
    ///
    /// Returns `false` when the queue has stopped.
    pub fn pause(&self, id: DownloadId) -> bool {
        self.send(QueueCommand::Pause(id))
    }

    /// Resumes a paused job. The transfer restarts from the beginning; partial bytes are not
    /// reused because [`Transfer`] has no resume-offset contract.
    ///
    /// Returns `false` when the queue has stopped.
    pub fn resume(&self, id: DownloadId) -> bool {
        self.send(QueueCommand::Resume(id))
    }

    /// Cancels a job. It settles as `Canceled` and is never reported as completed.
    ///
    /// Returns `false` when the queue has stopped.
    pub fn cancel(&self, id: DownloadId) -> bool {
        self.send(QueueCommand::Cancel(id))
    }

    /// Re-enqueues a failed or cancelled job as a fresh item.
    ///
    /// Returns `false` when the queue has stopped.
    pub fn retry(&self, id: DownloadId) -> bool {
        self.send(QueueCommand::Retry(id))
    }

    /// Removes a finished item from the live list. Active items are ignored.
    ///
    /// Returns `false` when the queue has stopped.
    pub fn remove(&self, id: DownloadId) -> bool {
        self.send(QueueCommand::Remove(id))
    }

    /// Clears the history in memory and on disk.
    ///
    /// Returns `false` when the queue has stopped.
    pub fn clear_history(&self) -> bool {
        self.send(QueueCommand::ClearHistory)
    }

    /// Returns a consistent snapshot of every live item, in scheduling order.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError::QueueClosed`] when the actor has already exited.
    pub async fn list(&self) -> Result<Vec<DownloadSnapshot>> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(QueueCommand::List { reply })
            .map_err(|_| DownloadError::QueueClosed)?;
        response.await.map_err(|_| DownloadError::QueueClosed)
    }

    /// Returns the in-memory history, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError::QueueClosed`] when the actor has already exited.
    pub async fn history(&self) -> Result<Vec<HistoryEntry>> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(QueueCommand::History { reply })
            .map_err(|_| DownloadError::QueueClosed)?;
        response.await.map_err(|_| DownloadError::QueueClosed)
    }

    /// Stops the queue, marking every unfinished job as interrupted and persisting history.
    pub async fn shutdown(&self) {
        let (reply, response) = oneshot::channel();
        if self
            .tx
            .send(QueueCommand::Shutdown { reply: Some(reply) })
            .is_ok()
        {
            let _ = response.await;
        }
    }

    /// Sends one command, reporting whether the actor is still alive.
    fn send(&self, command: QueueCommand) -> bool {
        self.tx.send(command).is_ok()
    }
}

/// A message from a running transfer back to the actor.
#[derive(Debug)]
enum JobMessage {
    /// Throttled byte progress.
    Progress {
        /// Job the reading belongs to.
        id: DownloadId,
        /// Attempt the reading belongs to.
        attempt: u64,
        /// The reading itself.
        progress: TransferProgress,
    },
    /// The attempt ended, successfully or not.
    Finished {
        /// Job that ended.
        id: DownloadId,
        /// Attempt that ended.
        attempt: u64,
        /// Transfer result.
        result: Result<crate::transfer::TransferOutcome>,
    },
}

/// One live queue row plus the state the actor needs to schedule it.
struct Item {
    snapshot: DownloadSnapshot,
    key: String,
    cancel: CancellationToken,
    attempt: u64,
    rate: RateMeter,
    output: Option<PathBuf>,
}

/// The state owner.
struct Actor {
    config: QueueConfig,
    events: mpsc::UnboundedSender<Event>,
    transfer: Arc<dyn Transfer>,
    history_store: Option<HistoryStore>,
    history: Vec<HistoryEntry>,
    history_cap: usize,
    items: Vec<Item>,
    running: HashSet<DownloadId>,
    next_id: u64,
    job_tx: mpsc::UnboundedSender<JobMessage>,
}

impl Actor {
    fn new(
        config: QueueConfig,
        events: mpsc::UnboundedSender<Event>,
        history_store: Option<HistoryStore>,
        transfer: Arc<dyn Transfer>,
        job_tx: mpsc::UnboundedSender<JobMessage>,
    ) -> Self {
        let history_cap = config
            .history_cap
            .min(history_store.as_ref().map_or(usize::MAX, HistoryStore::cap));

        let history = match history_store.as_ref() {
            Some(store) => match store.load() {
                Ok(entries) => entries,
                Err(error) => {
                    let _ = events.send(Event::Error {
                        context: "loading download history".to_owned(),
                        message: error.to_string(),
                    });
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        Self {
            config,
            events,
            transfer,
            history_store,
            history,
            history_cap,
            items: Vec::new(),
            running: HashSet::new(),
            next_id: 0,
            job_tx,
        }
    }

    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<QueueCommand>,
        mut jobs: mpsc::UnboundedReceiver<JobMessage>,
    ) {
        loop {
            tokio::select! {
                biased;
                command = commands.recv() => {
                    match command {
                        Some(command) => {
                            if self.handle_command(command) {
                                break;
                            }
                        }
                        None => {
                            self.shutdown_all();
                            break;
                        }
                    }
                }
                message = jobs.recv() => {
                    if let Some(message) = message {
                        self.handle_job(message);
                    }
                }
            }

            self.pump();
        }
    }

    /// Applies one command. Returns true when the actor should stop.
    fn handle_command(&mut self, command: QueueCommand) -> bool {
        match command {
            QueueCommand::Enqueue(request) => self.enqueue(*request),
            QueueCommand::Pause(id) => self.pause(id),
            QueueCommand::Resume(id) => self.resume(id),
            QueueCommand::Cancel(id) => self.cancel(id),
            QueueCommand::Retry(id) => self.retry(id),
            QueueCommand::Remove(id) => self.remove(id),
            QueueCommand::ClearHistory => self.clear_history(),
            QueueCommand::List { reply } => {
                let snapshots = self
                    .items
                    .iter()
                    .map(|item| item.snapshot.clone())
                    .collect();
                let _ = reply.send(snapshots);
            }
            QueueCommand::History { reply } => {
                let _ = reply.send(self.history.clone());
            }
            QueueCommand::Shutdown { reply } => {
                self.shutdown_all();
                if let Some(reply) = reply {
                    let _ = reply.send(());
                }
                return true;
            }
        }

        false
    }

    /// Adds a request unless an unfinished job already carries the same key.
    fn enqueue(&mut self, request: DownloadRequest) {
        let key = request_key(&request);
        if self
            .items
            .iter()
            .any(|item| !item.snapshot.status.is_finished() && item.key.eq_ignore_ascii_case(&key))
        {
            tracing::debug!(key = %key, "download already queued; ignoring duplicate");
            return;
        }

        self.next_id = self.next_id.saturating_add(1);
        let id = DownloadId::new(self.next_id);
        let snapshot = DownloadSnapshot::queued(id, request, Utc::now());
        self.items.push(Item {
            snapshot,
            key,
            cancel: CancellationToken::new(),
            attempt: 0,
            rate: RateMeter::new(),
            output: None,
        });
        self.publish(self.items.len() - 1);
    }

    /// Parks a job and stops its transfer, keeping its bytes on disk.
    fn pause(&mut self, id: DownloadId) {
        let Some(index) = self.index_of(id) else {
            return;
        };

        if !self.items[index].snapshot.status.can_pause() {
            return;
        }

        {
            let item = &mut self.items[index];
            item.snapshot.status = DownloadStatus::Paused;
            item.snapshot.bytes_per_second = 0;
            item.snapshot.eta_seconds = None;
            item.snapshot.updated_at = Utc::now();
            item.cancel.cancel();
            item.attempt = item.attempt.wrapping_add(1);
            item.rate.reset();
        }
        self.publish(index);
    }

    /// Returns a paused job to the queue.
    fn resume(&mut self, id: DownloadId) {
        let Some(index) = self.index_of(id) else {
            return;
        };

        if self.items[index].snapshot.status != DownloadStatus::Paused {
            return;
        }

        {
            let item = &mut self.items[index];
            item.cancel = CancellationToken::new();
            item.attempt = item.attempt.wrapping_add(1);
            item.rate.reset();
            item.snapshot.status = DownloadStatus::Queued;
            item.snapshot.received_bytes = 0;
            item.snapshot.total_bytes = None;
            item.snapshot.bytes_per_second = 0;
            item.snapshot.eta_seconds = None;
            item.snapshot.error = None;
            item.snapshot.updated_at = Utc::now();
        }
        self.publish(index);
    }

    /// Settles a job as cancelled. The transfer notices the token and returns.
    fn cancel(&mut self, id: DownloadId) {
        let Some(index) = self.index_of(id) else {
            return;
        };

        if self.items[index].snapshot.status.is_finished() {
            return;
        }

        {
            let item = &mut self.items[index];
            item.cancel.cancel();
            item.attempt = item.attempt.wrapping_add(1);
            item.rate.reset();
            item.snapshot.status = DownloadStatus::Canceled;
            item.snapshot.bytes_per_second = 0;
            item.snapshot.eta_seconds = None;
            item.snapshot.error = Some("cancelled by user".to_owned());
            item.snapshot.updated_at = Utc::now();
        }
        self.publish(index);
        self.record_history(index);
        self.persist_history();
    }

    /// Removes a failed or cancelled item and enqueues its request as a fresh item.
    fn retry(&mut self, id: DownloadId) {
        let Some(index) = self.index_of(id) else {
            return;
        };

        let request = self.items[index].snapshot.request.clone();
        if !self.items[index].snapshot.status.can_retry() {
            return;
        }

        self.items.remove(index);
        self.emit(Event::DownloadRemoved(id));
        self.enqueue(request);
    }

    /// Drops a finished item from the live list. Its history row is untouched.
    fn remove(&mut self, id: DownloadId) {
        let Some(index) = self.index_of(id) else {
            return;
        };

        if !self.items[index].snapshot.status.is_finished() {
            return;
        }

        self.items.remove(index);
        self.emit(Event::DownloadRemoved(id));
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.persist_history();
        self.emit(Event::DownloadHistoryCleared);
    }

    /// Starts as many queued items as the concurrency limit allows.
    fn pump(&mut self) {
        let limit = self.config.concurrency.max(1);
        while self.running.len() < limit {
            let candidate = self.items.iter().position(|item| {
                item.snapshot.status == DownloadStatus::Queued
                    && !self.running.contains(&item.snapshot.id)
            });
            let Some(index) = candidate else {
                break;
            };
            self.start(index);
        }
    }

    /// Spawns the blocking transfer for one item.
    fn start(&mut self, index: usize) {
        let Some(item) = self.items.get_mut(index) else {
            return;
        };

        item.attempt = item.attempt.wrapping_add(1);
        item.rate.reset();
        item.snapshot.status = DownloadStatus::Downloading;
        item.snapshot.bytes_per_second = 0;
        item.snapshot.eta_seconds = None;
        item.snapshot.error = None;
        item.snapshot.updated_at = Utc::now();

        let id = item.snapshot.id;
        let attempt = item.attempt;
        let request = item.snapshot.request.clone();
        let cancel = item.cancel.clone();

        self.running.insert(id);
        self.publish(index);

        let transfer = Arc::clone(&self.transfer);
        let progress_tx = self.job_tx.clone();
        let finished_tx = self.job_tx.clone();

        tokio::spawn(async move {
            let handle = tokio::task::spawn_blocking(move || {
                let mut throttle = ProgressThrottle::default();
                let mut report = |progress: TransferProgress| {
                    if throttle.allow(Instant::now()) {
                        let _ = progress_tx.send(JobMessage::Progress {
                            id,
                            attempt,
                            progress,
                        });
                    }
                };
                transfer.transfer(&request, &mut report, cancel)
            });

            let result = match handle.await {
                Ok(result) => result,
                Err(error) => Err(DownloadError::Transfer(format!(
                    "transfer task did not finish: {error}"
                ))),
            };

            let _ = finished_tx.send(JobMessage::Finished {
                id,
                attempt,
                result,
            });
        });
    }

    /// Applies a message from a running transfer.
    fn handle_job(&mut self, message: JobMessage) {
        match message {
            JobMessage::Progress {
                id,
                attempt,
                progress,
            } => {
                let Some(index) = self.index_of(id) else {
                    return;
                };

                {
                    let item = &mut self.items[index];
                    if item.attempt != attempt
                        || item.snapshot.status != DownloadStatus::Downloading
                    {
                        return;
                    }

                    item.snapshot.received_bytes = progress.bytes_read;
                    item.snapshot.total_bytes = progress.total_bytes.or(item.snapshot.total_bytes);
                    if let Some((rate, eta)) = item.rate.record(
                        Instant::now(),
                        progress.bytes_read,
                        item.snapshot.total_bytes,
                    ) {
                        item.snapshot.bytes_per_second = rate;
                        item.snapshot.eta_seconds = eta;
                    }
                    item.snapshot.updated_at = Utc::now();
                }
                self.publish(index);
            }
            JobMessage::Finished {
                id,
                attempt,
                result,
            } => {
                self.running.remove(&id);

                let Some(index) = self.index_of(id) else {
                    return;
                };

                if self.items[index].attempt != attempt
                    || self.items[index].snapshot.status != DownloadStatus::Downloading
                {
                    // Paused, cancelled, or superseded by a resumed attempt.
                    return;
                }

                {
                    let item = &mut self.items[index];
                    match result {
                        Ok(outcome) => {
                            item.snapshot.received_bytes = outcome.bytes;
                            item.snapshot.total_bytes = outcome.total_bytes.or(Some(outcome.bytes));
                            item.snapshot.status = DownloadStatus::Completed;
                            item.snapshot.error = None;
                            item.output = Some(outcome.path);
                        }
                        Err(error) if error.is_cancelled() => {
                            item.snapshot.status = DownloadStatus::Canceled;
                            item.snapshot.error = Some(error.to_string());
                        }
                        Err(error) => {
                            item.snapshot.status = DownloadStatus::Failed;
                            item.snapshot.error = Some(error.to_string());
                        }
                    }
                    item.snapshot.bytes_per_second = 0;
                    item.snapshot.eta_seconds = None;
                    item.snapshot.updated_at = Utc::now();
                }

                self.publish(index);
                self.record_history(index);
                self.persist_history();
            }
        }
    }

    /// Cancels every unfinished item, recording each as interrupted.
    fn shutdown_all(&mut self) {
        let now = Utc::now();
        let indices: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.snapshot.status.is_finished())
            .map(|(index, _)| index)
            .collect();

        for index in indices {
            {
                let item = &mut self.items[index];
                item.cancel.cancel();
                item.attempt = item.attempt.wrapping_add(1);
                item.rate.reset();
                item.snapshot.status = DownloadStatus::Canceled;
                item.snapshot.bytes_per_second = 0;
                item.snapshot.eta_seconds = None;
                item.snapshot.error = Some("interrupted by shutdown".to_owned());
                item.snapshot.updated_at = now;
            }
            self.publish(index);
            self.record_history(index);
        }

        if !self.history.is_empty() {
            self.persist_history();
        }
        self.running.clear();
    }

    /// Copies a settled item into the history list, newest first.
    fn record_history(&mut self, index: usize) {
        let Some(item) = self.items.get(index) else {
            return;
        };

        let status = item.snapshot.status;
        if !status.is_finished() {
            return;
        }

        let entry = HistoryEntry {
            id: item.snapshot.id,
            app_id: item.snapshot.request.app_id,
            kind: item.snapshot.request.kind,
            title: item.snapshot.request.app_name.clone(),
            bytes: item.snapshot.received_bytes,
            status,
            message: item.snapshot.error.clone(),
            completed_at: Utc::now(),
            output: item
                .output
                .clone()
                .or_else(|| Some(item.snapshot.request.destination.clone())),
        };

        self.history.insert(0, entry);
        self.history.truncate(self.history_cap);
    }

    /// Writes the history file, reporting rather than propagating a failure.
    fn persist_history(&self) {
        let Some(store) = self.history_store.as_ref() else {
            return;
        };

        if let Err(error) = store.save(&self.history) {
            let _ = self.events.send(Event::Error {
                context: "saving download history".to_owned(),
                message: error.to_string(),
            });
        }
    }

    /// Emits the current snapshot for one item.
    fn publish(&self, index: usize) {
        if let Some(item) = self.items.get(index) {
            self.emit(Event::DownloadUpdated(Box::new(item.snapshot.clone())));
        }
    }

    /// Sends an event, ignoring a closed interface channel.
    fn emit(&self, event: Event) {
        if self.events.send(event).is_err() {
            tracing::debug!("download event receiver closed");
        }
    }

    fn index_of(&self, id: DownloadId) -> Option<usize> {
        self.items.iter().position(|item| item.snapshot.id == id)
    }
}

/// Stable key used to deduplicate unfinished jobs.
///
/// File jobs are keyed by app, URL, and destination; depot jobs additionally include the
/// depot/manifest set. Comparison is case-insensitive, mirroring the source queue.
fn request_key(request: &DownloadRequest) -> String {
    let kind = match request.kind {
        luatools_core::DownloadKind::Depot => "depot",
        luatools_core::DownloadKind::File => "file",
    };

    let mut depots: Vec<String> = request
        .depots
        .iter()
        .map(|depot| {
            format!(
                "{}:{}",
                depot.depot_id,
                depot
                    .manifest_id
                    .map_or_else(String::new, |manifest| manifest.to_string())
            )
        })
        .collect();
    depots.sort();

    format!(
        "{kind}|{}|{}|{}|{}",
        request.app_id,
        request
            .url
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase(),
        depots.join(","),
        request.destination.to_string_lossy().to_ascii_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use luatools_core::{AppId, DownloadKind};

    use super::*;
    use crate::transfer::TransferOutcome;

    /// Deterministic in-process transfer. No sockets are involved.
    #[derive(Default)]
    struct MockTransfer {
        active: AtomicUsize,
        max_active: AtomicUsize,
        attempts: AtomicUsize,
        fail_first: AtomicBool,
        block: AtomicBool,
        release: AtomicBool,
    }

    impl MockTransfer {
        fn active(&self) -> usize {
            self.active.load(Ordering::SeqCst)
        }

        fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }

        fn max_active(&self) -> usize {
            self.max_active.load(Ordering::SeqCst)
        }
    }

    impl Transfer for MockTransfer {
        fn transfer(
            &self,
            request: &DownloadRequest,
            progress: &mut dyn FnMut(TransferProgress),
            cancel: CancellationToken,
        ) -> Result<TransferOutcome> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);

            progress(TransferProgress::new(1, None));

            let fail = self.fail_first.load(Ordering::SeqCst) && attempt == 1;
            let result = if fail {
                Err(DownloadError::Transfer("mock failure".to_owned()))
            } else if self.block.load(Ordering::SeqCst) {
                loop {
                    if cancel.is_cancelled() {
                        break Err(DownloadError::Cancelled);
                    }
                    if self.release.load(Ordering::SeqCst) {
                        progress(TransferProgress::new(2, Some(2)));
                        break Ok(TransferOutcome::new(
                            PathBuf::from(format!("/tmp/mock-{}.bin", request.app_id)),
                            2,
                            Some(2),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            } else {
                std::thread::sleep(Duration::from_millis(30));
                progress(TransferProgress::new(2, Some(2)));
                Ok(TransferOutcome::new(
                    PathBuf::from(format!("/tmp/mock-{}.bin", request.app_id)),
                    2,
                    Some(2),
                ))
            };

            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    struct Harness {
        handle: QueueHandle,
        events: mpsc::UnboundedReceiver<Event>,
        mock: Arc<MockTransfer>,
        _dir: tempfile::TempDir,
    }

    fn harness(config: QueueConfig) -> Harness {
        let dir = tempfile::tempdir().expect("temp dir");
        let mock = Arc::new(MockTransfer::default());
        let (events_tx, events) = mpsc::unbounded_channel();
        let handle =
            DownloadQueue::spawn_with(config, events_tx, None, mock.clone() as Arc<dyn Transfer>);
        Harness {
            handle,
            events,
            mock,
            _dir: dir,
        }
    }

    fn request(app: u32) -> DownloadRequest {
        DownloadRequest {
            app_id: AppId::new(app),
            app_name: format!("App {app}"),
            kind: DownloadKind::File,
            depots: Vec::new(),
            url: Some(format!("https://example.com/tool-{app}.zip")),
            destination: PathBuf::from("/tmp/luatools-tests"),
            verify: false,
        }
    }

    async fn wait_for<F>(handle: &QueueHandle, mut predicate: F) -> Vec<DownloadSnapshot>
    where
        F: FnMut(&[DownloadSnapshot]) -> bool,
    {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let items = handle.list().await.expect("queue responds");
                if predicate(&items) {
                    return items;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("queue state did not settle in time")
    }

    async fn wait_until_idle(mock: &MockTransfer) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while mock.active() != 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("mock transfer did not stop");
    }

    async fn wait_until_attempts(mock: &MockTransfer, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while mock.attempts() < count {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("mock transfer did not start");
    }

    fn drain_events(events: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut drained = Vec::new();
        while let Ok(event) = events.try_recv() {
            drained.push(event);
        }
        drained
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrency_limit_is_respected() {
        let harness = harness(QueueConfig::new(2, std::env::temp_dir()));

        for app in 1..=6 {
            assert!(harness.handle.enqueue(request(app)));
        }

        let items = wait_for(&harness.handle, |items| {
            items.len() == 6
                && items
                    .iter()
                    .all(|item| item.status == DownloadStatus::Completed)
        })
        .await;

        assert_eq!(items.len(), 6);
        assert_eq!(harness.mock.max_active(), 2);
        harness.handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn duplicate_active_requests_are_deduped() {
        let harness = harness(QueueConfig::new(1, std::env::temp_dir()));

        harness.handle.enqueue(request(7));
        harness.handle.enqueue(request(7));

        let items = wait_for(&harness.handle, |items| {
            items.len() == 1 && items[0].status == DownloadStatus::Completed
        })
        .await;
        assert_eq!(items.len(), 1);
        assert_eq!(harness.mock.attempts(), 1);

        // Once finished, the same request may be queued again.
        harness.handle.enqueue(request(7));
        let items = wait_for(&harness.handle, |items| {
            items.len() == 2
                && items
                    .iter()
                    .all(|item| item.status == DownloadStatus::Completed)
        })
        .await;
        assert_eq!(items.len(), 2);
        harness.handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pause_then_resume_restarts_and_completes() {
        let harness = harness(QueueConfig::new(1, std::env::temp_dir()));
        harness.mock.block.store(true, Ordering::SeqCst);

        harness.handle.enqueue(request(11));
        let items = wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Downloading)
        })
        .await;
        let id = items[0].id;

        wait_until_attempts(&harness.mock, 1).await;
        harness.handle.pause(id);
        wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Paused)
        })
        .await;
        wait_until_idle(&harness.mock).await;
        assert_eq!(harness.mock.attempts(), 1);

        harness.mock.release.store(true, Ordering::SeqCst);
        harness.handle.resume(id);
        let items = wait_for(&harness.handle, |items| {
            items.len() == 1 && items[0].status == DownloadStatus::Completed
        })
        .await;

        assert_eq!(items[0].id, id);
        assert_eq!(items[0].received_bytes, 2);
        assert_eq!(harness.mock.attempts(), 2);
        harness.handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_mid_transfer_settles_as_cancelled_and_never_completed() {
        let mut harness = harness(QueueConfig::new(1, std::env::temp_dir()));
        harness.mock.block.store(true, Ordering::SeqCst);

        harness.handle.enqueue(request(13));
        let items = wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Downloading)
        })
        .await;
        let id = items[0].id;

        harness.handle.cancel(id);
        let items = wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Canceled)
        })
        .await;
        assert_eq!(items[0].status, DownloadStatus::Canceled);

        // Give a late transfer result time to (incorrectly) arrive.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let items = harness.handle.list().await.expect("list");
        assert_eq!(items[0].status, DownloadStatus::Canceled);
        assert_ne!(items[0].status, DownloadStatus::Completed);

        let history = harness.handle.history().await.expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadStatus::Canceled);

        let events = drain_events(&mut harness.events);
        let completed_events = events
            .iter()
            .filter(|event| match event {
                Event::DownloadUpdated(snapshot) => {
                    snapshot.id == id && snapshot.status == DownloadStatus::Completed
                }
                _ => false,
            })
            .count();
        assert_eq!(completed_events, 0);
        harness.handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn retry_re_enqueues_a_failed_job() {
        let harness = harness(QueueConfig::new(1, std::env::temp_dir()));
        harness.mock.fail_first.store(true, Ordering::SeqCst);

        harness.handle.enqueue(request(17));
        let items = wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Failed)
        })
        .await;
        let failed_id = items[0].id;

        harness.handle.retry(failed_id);
        let items = wait_for(&harness.handle, |items| {
            items.len() == 1 && items[0].status == DownloadStatus::Completed
        })
        .await;

        assert_ne!(items[0].id, failed_id, "retry creates a fresh item");
        assert_eq!(harness.mock.attempts(), 2);

        let history = harness.handle.history().await.expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, DownloadStatus::Completed);
        assert_eq!(history[1].status, DownloadStatus::Failed);
        harness.handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_only_accepts_finished_items() {
        let mut harness = harness(QueueConfig::new(1, std::env::temp_dir()));
        harness.mock.block.store(true, Ordering::SeqCst);

        harness.handle.enqueue(request(19));
        let items = wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Downloading)
        })
        .await;
        let id = items[0].id;

        harness.handle.remove(id);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            harness.handle.list().await.expect("list").len(),
            1,
            "an active item cannot be removed"
        );

        harness.handle.cancel(id);
        wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Canceled)
        })
        .await;

        harness.handle.remove(id);
        wait_for(&harness.handle, |items| items.is_empty()).await;

        let events = drain_events(&mut harness.events);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::DownloadRemoved(removed) if *removed == id)),
            "removal must be announced"
        );
        harness.handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn history_keeps_the_newest_entries_up_to_the_cap() {
        let harness = harness(QueueConfig::new(1, std::env::temp_dir()).with_history_cap(2));

        for app in 20..23 {
            harness.handle.enqueue(request(app));
        }

        let items = wait_for(&harness.handle, |items| {
            items.len() == 3
                && items
                    .iter()
                    .all(|item| item.status == DownloadStatus::Completed)
        })
        .await;

        let history = harness.handle.history().await.expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, items[2].id);
        assert_eq!(history[1].id, items[1].id);
        harness.handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clear_history_empties_memory_and_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = HistoryStore::new(dir.path().join("history.json"));
        let mock = Arc::new(MockTransfer::default());
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let handle = DownloadQueue::spawn_with(
            QueueConfig::new(1, dir.path().join("staging")),
            events_tx,
            Some(store.clone()),
            mock as Arc<dyn Transfer>,
        );

        handle.enqueue(request(41));
        wait_for(&handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Completed)
        })
        .await;
        assert_eq!(handle.history().await.expect("history").len(), 1);
        assert_eq!(store.load().expect("load").len(), 1);

        handle.clear_history();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if handle.history().await.expect("history").is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("history was not cleared");

        assert!(store.load().expect("load").is_empty());
        let events = drain_events(&mut events);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::DownloadHistoryCleared)),
            "clearing must be announced"
        );
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn history_write_failure_is_reported_as_an_error_event() {
        let dir = tempfile::tempdir().expect("temp dir");
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").expect("seed file");
        let store = HistoryStore::new(blocked.join("history.json"));

        let mock = Arc::new(MockTransfer::default());
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let handle = DownloadQueue::spawn_with(
            QueueConfig::new(1, dir.path().join("staging")),
            events_tx,
            Some(store),
            mock as Arc<dyn Transfer>,
        );

        handle.enqueue(request(43));
        wait_for(&handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Completed)
        })
        .await;

        let events = drain_events(&mut events);
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::Error { context, .. } if context == "saving download history"
            )),
            "a history write failure must surface as an error event"
        );
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unknown_total_keeps_the_snapshot_fraction_empty() {
        let harness = harness(QueueConfig::new(1, std::env::temp_dir()));
        harness.mock.block.store(true, Ordering::SeqCst);

        harness.handle.enqueue(request(30));
        let items = wait_for(&harness.handle, |items| {
            items
                .iter()
                .any(|item| item.status == DownloadStatus::Downloading)
        })
        .await;

        assert_eq!(items[0].total_bytes, None);
        assert_eq!(items[0].fraction(), None);

        harness.handle.cancel(items[0].id);
        harness.handle.shutdown().await;
    }
}
