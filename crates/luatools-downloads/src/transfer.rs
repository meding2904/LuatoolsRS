//! The transfer abstraction and the plain-HTTP file implementation.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use luatools_core::DownloadRequest;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH};
use tokio_util::sync::CancellationToken;

use crate::error::{DownloadError, Result};
use crate::filename::sanitize_filename;
use crate::progress::TransferProgress;

/// Size of the copy buffer used by [`HttpClientTransfer`].
const COPY_BUFFER_SIZE: usize = 80 * 1024;

/// Number of collision suffixes tried before giving up on a staging filename.
const MAX_NAME_ATTEMPTS: u32 = 1_000;

/// A unit of work the queue can run.
///
/// Implementations are synchronous and may block: the queue calls [`Transfer::transfer`] on
/// Tokio's blocking pool, one call per attempt. Implementations must observe `cancel` regularly
/// and return [`DownloadError::Cancelled`] rather than a success once it is set.
pub trait Transfer: Send + Sync {
    /// Performs the transfer, reporting byte counts through `progress`.
    ///
    /// `progress` is called from the transfer thread; the queue rate-limits event emission, so
    /// reporting once per chunk is expected.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError::Cancelled`] when `cancel` fired, and a contextual
    /// [`DownloadError`] for network, HTTP, or filesystem failures.
    fn transfer(
        &self,
        request: &DownloadRequest,
        progress: &mut dyn FnMut(TransferProgress),
        cancel: CancellationToken,
    ) -> Result<TransferOutcome>;
}

/// What a successful transfer produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    /// File that was written.
    pub path: PathBuf,
    /// Bytes written.
    pub bytes: u64,
    /// Total declared by the source, or the final byte count when no length was advertised.
    pub total_bytes: Option<u64>,
}

impl TransferOutcome {
    /// Creates an outcome.
    #[must_use]
    pub const fn new(path: PathBuf, bytes: u64, total_bytes: Option<u64>) -> Self {
        Self {
            path,
            bytes,
            total_bytes,
        }
    }
}

/// Streaming HTTP(S) download of a single file.
///
/// The response body is copied in 80 KiB chunks into [`HttpClientTransfer::staging_dir`]. The
/// filename comes from `Content-Disposition` when present, otherwise from the URL's final path
/// segment, and is always passed through [`sanitize_filename`]. A partial file is removed when
/// the transfer fails or is cancelled.
///
/// Cancellation is observed before the request, between body chunks, and after the body. A
/// connection attempt that has not produced a response yet is bounded by the 30-second connect
/// timeout rather than by the token.
///
/// The completed file remains in the staging directory; moving or installing it is the caller's
/// responsibility.
#[derive(Debug)]
pub struct HttpClientTransfer {
    staging_dir: PathBuf,
    client: parking_lot::Mutex<Option<Client>>,
}

impl HttpClientTransfer {
    /// Creates a transfer that stages files under `staging_dir`.
    ///
    /// The HTTP client is built on the first transfer so construction cannot fail.
    #[must_use]
    pub fn new(staging_dir: impl Into<PathBuf>) -> Self {
        Self {
            staging_dir: staging_dir.into(),
            client: parking_lot::Mutex::new(None),
        }
    }

    /// Creates a transfer with a caller-supplied client.
    #[must_use]
    pub fn with_client(staging_dir: impl Into<PathBuf>, client: Client) -> Self {
        Self {
            staging_dir: staging_dir.into(),
            client: parking_lot::Mutex::new(Some(client)),
        }
    }

    /// Directory downloads are staged in.
    #[must_use]
    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }

    /// Returns the shared client, building it on first use.
    fn client(&self) -> Result<Client> {
        let mut slot = self.client.lock();
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }

        let client = Client::builder()
            .user_agent(concat!("LuaTools/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                DownloadError::from(luatools_core::Error::Network {
                    context: "building download client".to_owned(),
                    message: error.to_string(),
                })
            })?;
        *slot = Some(client.clone());
        Ok(client)
    }
}

impl Transfer for HttpClientTransfer {
    fn transfer(
        &self,
        request: &DownloadRequest,
        progress: &mut dyn FnMut(TransferProgress),
        cancel: CancellationToken,
    ) -> Result<TransferOutcome> {
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }

        let url = request
            .url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                DownloadError::InvalidRequest("a plain file transfer requires a URL".to_owned())
            })?;

        let parsed = reqwest::Url::parse(url)
            .map_err(|error| DownloadError::InvalidRequest(format!("{url}: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(DownloadError::InvalidRequest(format!(
                "only http(s) file URLs are supported, got {}",
                parsed.scheme()
            )));
        }

        let client = self.client()?;
        let mut response = client.get(parsed.clone()).send().map_err(|error| {
            DownloadError::from(luatools_core::Error::Network {
                context: format!("downloading {url}"),
                message: error.to_string(),
            })
        })?;

        let status = response.status();
        if !status.is_success() {
            return Err(DownloadError::from(luatools_core::Error::Http {
                status: status.as_u16(),
                url: url.to_owned(),
            }));
        }

        let total_bytes = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());

        let header_name = response
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(filename_from_content_disposition);
        let fallback_name = filename_from_url(&parsed);
        let name = sanitize_filename(header_name.or(fallback_name).as_deref().unwrap_or(""));

        let (path, mut file) = create_staging_file(&self.staging_dir, &name)?;

        let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
        let mut written = 0_u64;

        loop {
            if cancel.is_cancelled() {
                drop(file);
                remove_partial(&path);
                return Err(DownloadError::Cancelled);
            }

            let read = match response.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    drop(file);
                    remove_partial(&path);
                    return Err(DownloadError::io(
                        format!("reading response body from {url}"),
                        error,
                    ));
                }
            };
            if read == 0 {
                break;
            }

            if let Err(error) = file.write_all(&buffer[..read]) {
                drop(file);
                remove_partial(&path);
                return Err(DownloadError::io(
                    format!("writing staging file {}", path.display()),
                    error,
                ));
            }

            written += read as u64;
            progress(TransferProgress::new(written, total_bytes));
        }

        if cancel.is_cancelled() {
            drop(file);
            remove_partial(&path);
            return Err(DownloadError::Cancelled);
        }

        if let Err(error) = file.sync_all() {
            drop(file);
            remove_partial(&path);
            return Err(DownloadError::io(
                format!("flushing staging file {}", path.display()),
                error,
            ));
        }
        drop(file);

        // One final reading settles the bar at 100% even for an empty or single-chunk body.
        let reported_total = total_bytes.unwrap_or(written);
        progress(TransferProgress::new(written, Some(reported_total)));

        Ok(TransferOutcome::new(path, written, Some(reported_total)))
    }
}

/// Extracts the `filename` or `filename*` parameter from a `Content-Disposition` value.
///
/// Returns the raw (still untrusted) name; callers must sanitise it.
#[must_use]
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    for part in value.split(';') {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let raw = raw.trim();
        let unquoted = raw.trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "filename" => {
                if !unquoted.is_empty() {
                    return Some(unquoted.to_owned());
                }
            }
            "filename*" => {
                // RFC 5987: charset'language'percent-encoded-value
                let encoded = unquoted
                    .split_once('\'')
                    .and_then(|(_, rest)| rest.split_once('\'').map(|(_, value)| value))
                    .unwrap_or(unquoted);
                let decoded = percent_decode(encoded);
                if !decoded.is_empty() {
                    return Some(decoded);
                }
            }
            _ => {}
        }
    }
    None
}

/// Returns the final URL path segment, percent-decoded, when there is a usable one.
#[must_use]
pub fn filename_from_url(url: &reqwest::Url) -> Option<String> {
    let segment = url.path_segments()?.next_back()?;
    let decoded = percent_decode(segment);
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

/// Decodes `%XX` escapes. Invalid escapes are copied through unchanged.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push((high as u8) << 4 | low as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

/// Creates a new file in `dir`, adding a numeric suffix when the name is taken.
fn create_staging_file(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File)> {
    std::fs::create_dir_all(dir).map_err(|error| {
        DownloadError::io(
            format!("creating staging directory {}", dir.display()),
            error,
        )
    })?;

    for attempt in 0..MAX_NAME_ATTEMPTS {
        let candidate = numbered_name(name, attempt);
        let path = dir.join(&candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(DownloadError::io(
                    format!("creating staging file {}", path.display()),
                    error,
                ));
            }
        }
    }

    Err(DownloadError::other(format!(
        "could not find a free staging filename for {}",
        dir.join(name).display()
    )))
}

/// Builds `stem-N.ext` for attempt `N`, leaving attempt zero untouched.
fn numbered_name(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return name.to_owned();
    }

    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) => format!("{stem}-{attempt}.{extension}"),
        None => format!("{stem}-{attempt}"),
    }
}

/// Best-effort removal of a partially written file.
fn remove_partial(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(
            path = %path.display(),
            %error,
            "could not remove partial staging file"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_plain_filename_is_extracted() {
        let value = r#"attachment; filename="../../evil.zip""#;
        assert_eq!(
            filename_from_content_disposition(value),
            Some("../../evil.zip".to_owned())
        );
        assert_eq!(
            sanitize_filename(&filename_from_content_disposition(value).unwrap_or_default()),
            "evil.zip"
        );
    }

    #[test]
    fn content_disposition_extended_filename_is_decoded() {
        let value = "attachment; filename*=UTF-8''caf%C3%A9.zip";
        let name = filename_from_content_disposition(value).expect("filename");
        assert_eq!(name, "café.zip");
    }

    #[test]
    fn content_disposition_without_filename_is_none() {
        assert_eq!(filename_from_content_disposition("attachment"), None);
        assert_eq!(
            filename_from_content_disposition("attachment; size=12"),
            None
        );
    }

    #[test]
    fn url_last_segment_becomes_the_fallback_name() {
        let url =
            reqwest::Url::parse("https://example.com/files/tool%20v2.zip?token=1").expect("url");
        assert_eq!(filename_from_url(&url), Some("tool v2.zip".to_owned()));

        let directory = reqwest::Url::parse("https://example.com/files/").expect("url");
        assert_eq!(filename_from_url(&directory), None);
    }

    #[test]
    fn staging_names_are_unique_per_attempt() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (first, _file) = create_staging_file(dir.path(), "tool.zip").expect("first");
        let (second, _file) = create_staging_file(dir.path(), "tool.zip").expect("second");
        let (third, _file) = create_staging_file(dir.path(), "tool.zip").expect("third");

        assert_eq!(first.file_name().and_then(|n| n.to_str()), Some("tool.zip"));
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("tool-1.zip")
        );
        assert_eq!(
            third.file_name().and_then(|n| n.to_str()),
            Some("tool-2.zip")
        );
    }

    #[test]
    fn transfer_rejects_requests_without_a_plain_url() {
        let transfer = HttpClientTransfer::new(std::env::temp_dir());
        let mut request = luatools_core::DownloadRequest {
            app_id: luatools_core::AppId::new(0),
            app_name: "test".to_owned(),
            kind: luatools_core::DownloadKind::File,
            depots: Vec::new(),
            url: None,
            destination: std::env::temp_dir(),
            verify: false,
        };
        let mut noop = |_: TransferProgress| {};
        let error = transfer
            .transfer(&request, &mut noop, CancellationToken::new())
            .expect_err("missing url must fail");
        assert!(matches!(error, DownloadError::InvalidRequest(_)));

        request.url = Some("ftp://example.com/file".to_owned());
        let error = transfer
            .transfer(&request, &mut noop, CancellationToken::new())
            .expect_err("non-http scheme must fail");
        assert!(matches!(error, DownloadError::InvalidRequest(_)));
    }

    #[test]
    fn cancelled_transfer_never_starts_work() {
        let transfer = HttpClientTransfer::new(std::env::temp_dir());
        let request = luatools_core::DownloadRequest {
            app_id: luatools_core::AppId::new(0),
            app_name: "test".to_owned(),
            kind: luatools_core::DownloadKind::File,
            depots: Vec::new(),
            url: Some("https://example.com/file.zip".to_owned()),
            destination: std::env::temp_dir(),
            verify: false,
        };
        let token = CancellationToken::new();
        token.cancel();
        let mut noop = |_: TransferProgress| {};
        assert!(matches!(
            transfer.transfer(&request, &mut noop, token),
            Err(DownloadError::Cancelled)
        ));
    }
}
