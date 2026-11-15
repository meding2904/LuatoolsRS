//! Errors shared by the workspace.

use std::path::PathBuf;

/// The workspace error type.
///
/// Backend crates add context with [`Error::context`] and convert their own errors with
/// `From`; the UI only needs the rendered message.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O operation failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done when the operation failed.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A file was missing.
    #[error("file not found: {}", path.display())]
    NotFound {
        /// The missing path.
        path: PathBuf,
    },

    /// Text or binary data did not match the expected format.
    #[error("invalid {format}: {message}")]
    Parse {
        /// Format name, e.g. `vdf`, `appinfo`, `manifest`.
        format: &'static str,
        /// What was wrong.
        message: String,
    },

    /// A serialization step failed.
    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),

    /// A network request failed.
    #[error("{context}: {message}")]
    Network {
        /// What was being requested.
        context: String,
        /// Rendered transport or status error.
        message: String,
    },

    /// The remote answered with an error status.
    #[error("http {status} for {url}")]
    Http {
        /// Status code.
        status: u16,
        /// Requested URL.
        url: String,
    },

    /// The operation is not available on this platform.
    #[error("unsupported on this platform: {0}")]
    Unsupported(&'static str),

    /// A user-supplied value was rejected.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A background task was cancelled.
    #[error("cancelled")]
    Cancelled,

    /// Any other failure, already rendered.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Adds a human-readable context line to an I/O error.
    #[must_use]
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    /// Wraps an error with additional context.
    #[must_use]
    pub fn context(self, context: impl Into<String>) -> Self {
        let context = context.into();
        match self {
            Self::Io {
                context: inner,
                source,
            } => Self::Io {
                context: format!("{context}: {inner}"),
                source,
            },
            Self::Network {
                context: inner,
                message,
            } => Self::Network {
                context: format!("{context}: {inner}"),
                message,
            },
            other => Self::Other(format!("{context}: {other}")),
        }
    }

    /// Builds a parse error.
    #[must_use]
    pub fn parse(format: &'static str, message: impl Into<String>) -> Self {
        Self::Parse {
            format,
            message: message.into(),
        }
    }

    /// Returns true when the caller cancelled the work.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Self::Io {
            context: "io".to_owned(),
            source,
        }
    }
}

/// Convenience alias used across the workspace.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_prepended_to_io_errors() {
        let err = Error::io(
            "reading library",
            std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        )
        .context("scanning steam");
        assert!(
            err.to_string()
                .starts_with("scanning steam: reading library")
        );
    }

    #[test]
    fn cancellation_is_detected() {
        assert!(Error::Cancelled.is_cancelled());
        assert!(!Error::Other("boom".into()).is_cancelled());
    }
}
