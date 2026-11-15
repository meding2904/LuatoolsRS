//! Errors raised by the download queue, transfers, and history store.
//!
//! Recoverable failures carry the operation that failed plus the underlying cause;
//! [`luatools_core::Error`] covers I/O, parsing, and HTTP transport failures, while this
//! crate adds the queue-lifecycle cases.

use luatools_core::Error;

/// Result alias used throughout the crate.
pub type Result<T, E = DownloadError> = std::result::Result<T, E>;

/// A download queue, transfer, or history failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DownloadError {
    /// A shared core error: I/O, serialization, network, or HTTP.
    #[error(transparent)]
    Core(#[from] Error),

    /// The queue actor has stopped, so no further messages can be delivered.
    #[error("download queue is not running")]
    QueueClosed,

    /// A [`crate::Transfer`] implementation reported a failure.
    #[error("transfer failed: {0}")]
    Transfer(String),

    /// The request cannot be served by the configured transfer.
    #[error("invalid download request: {0}")]
    InvalidRequest(String),

    /// The work was cancelled, either by the user or by shutdown.
    #[error("cancelled")]
    Cancelled,
}

/// Allows `?` on JSON operations to surface as the shared serialization error.
impl From<serde_json::Error> for DownloadError {
    fn from(error: serde_json::Error) -> Self {
        Self::Core(Error::from(error))
    }
}

impl DownloadError {
    /// Whether this error represents cancellation rather than a genuine failure.
    ///
    /// The queue uses this to settle an interrupted job as `Canceled` instead of `Failed`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::Core(error) => error.is_cancelled(),
            Self::QueueClosed | Self::Transfer(_) | Self::InvalidRequest(_) => false,
        }
    }

    /// Builds an I/O error with the operation that failed.
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Core(Error::io(context, source))
    }

    /// Builds an "other" error with an already-rendered message.
    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self::Core(Error::Other(message.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_recognised_through_the_core_error() {
        assert!(DownloadError::Cancelled.is_cancelled());
        assert!(DownloadError::Core(Error::Cancelled).is_cancelled());
        assert!(!DownloadError::Transfer("boom".to_owned()).is_cancelled());
        assert!(!DownloadError::QueueClosed.is_cancelled());
    }

    #[test]
    fn io_errors_keep_their_context() {
        let error = DownloadError::io(
            "writing staging file",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        let rendered = error.to_string();
        assert!(rendered.contains("writing staging file"));
        assert!(rendered.contains("denied"));
    }
}
