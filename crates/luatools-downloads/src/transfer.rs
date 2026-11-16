//! The transfer abstraction and the plain-HTTP file implementation.

use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use luatools_core::DownloadRequest;
use luatools_core::hash::{HashDigest, sha256_file};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH};
use tokio_util::sync::CancellationToken;

use crate::error::{DownloadError, Result};
use crate::filename::sanitize_filename;
use crate::progress::TransferProgress;

/// Size of the copy buffer used by [`HttpClientTransfer`].
const COPY_BUFFER_SIZE: usize = 80 * 1024;

/// Number of collision suffixes tried before giving up on a staging or destination filename.
const MAX_NAME_ATTEMPTS: u32 = 1_000;

/// Longest single sleep while waiting on the rate limiter, so cancellation is noticed promptly.
const RATE_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    /// File that was written, in the request's destination directory.
    pub path: PathBuf,
    /// Bytes written.
    pub bytes: u64,
    /// Total declared by the source, or the final byte count when no length was advertised.
    pub total_bytes: Option<u64>,
    /// SHA-256 of the received file, when the request asked for verification.
    ///
    /// Verification currently covers the file length; the digest is exposed so callers can
    /// display or persist it, but no caller-supplied digest is compared against.
    pub sha256: Option<HashDigest>,
}

impl TransferOutcome {
    /// Creates an outcome without a digest.
    #[must_use]
    pub const fn new(path: PathBuf, bytes: u64, total_bytes: Option<u64>) -> Self {
        Self {
            path,
            bytes,
            total_bytes,
            sha256: None,
        }
    }

    /// Attaches the SHA-256 digest computed during verification.
    #[must_use]
    pub fn with_sha256(mut self, sha256: HashDigest) -> Self {
        self.sha256 = Some(sha256);
        self
    }
}

/// Streaming HTTP(S) download of a single file.
///
/// The response body is copied in 80 KiB chunks into [`HttpClientTransfer::staging_dir`]. The
/// filename comes from `Content-Disposition` when present, otherwise from the URL's final path
/// segment, and is always passed through [`sanitize_filename`]. A partial file is removed when
/// the transfer fails or is cancelled.
///
/// When [`DownloadRequest::verify`] is true, the received byte count is checked against an
/// advertised `Content-Length` and a SHA-256 digest is attached to the outcome. The verified
/// file is then moved into [`DownloadRequest::destination`], which is treated as a directory:
/// it is created when missing, existing files are never overwritten (a numeric suffix is added
/// instead), and the staging copy is removed once the move succeeds.
///
/// Cancellation is observed before the request, between body chunks, while waiting on the speed
/// limit, and after the body. A connection attempt that has not produced a response yet is
/// bounded by the 30-second connect timeout rather than by the token.
#[derive(Debug)]
pub struct HttpClientTransfer {
    staging_dir: PathBuf,
    speed_limit_bytes: Option<u64>,
    client: parking_lot::Mutex<Option<Client>>,
}

impl HttpClientTransfer {
    /// Creates a transfer that stages files under `staging_dir`, with no speed limit.
    ///
    /// The HTTP client is built on the first transfer so construction cannot fail.
    #[must_use]
    pub fn new(staging_dir: impl Into<PathBuf>) -> Self {
        Self {
            staging_dir: staging_dir.into(),
            speed_limit_bytes: None,
            client: parking_lot::Mutex::new(None),
        }
    }

    /// Creates a transfer with a caller-supplied client.
    #[must_use]
    pub fn with_client(staging_dir: impl Into<PathBuf>, client: Client) -> Self {
        Self {
            staging_dir: staging_dir.into(),
            speed_limit_bytes: None,
            client: parking_lot::Mutex::new(Some(client)),
        }
    }

    /// Caps the average transfer rate at `speed_limit_bytes` per second.
    ///
    /// [`None`] and zero disable the limit. The cap is applied per transfer call.
    #[must_use]
    pub fn with_speed_limit(mut self, speed_limit_bytes: Option<u64>) -> Self {
        self.speed_limit_bytes = speed_limit_bytes.filter(|limit| *limit > 0);
        self
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

        self.receive(
            &mut response,
            total_bytes,
            &name,
            request,
            progress,
            &cancel,
        )
    }
}

impl HttpClientTransfer {
    /// Copies a response body into staging, verifies it when asked, and installs it in the
    /// request's destination directory.
    ///
    /// Kept separate from [`Transfer::transfer`] so the body loop can be exercised with an
    /// in-memory reader instead of a live socket.
    fn receive(
        &self,
        body: &mut dyn Read,
        declared_total: Option<u64>,
        name: &str,
        request: &DownloadRequest,
        progress: &mut dyn FnMut(TransferProgress),
        cancel: &CancellationToken,
    ) -> Result<TransferOutcome> {
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }

        let name = sanitize_filename(name);
        let source = request.url.as_deref().unwrap_or(name.as_str());
        let (path, mut file) = create_staging_file(&self.staging_dir, &name)?;

        let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
        let mut written = 0_u64;
        let limiter = SpeedLimiter::new(self.speed_limit_bytes);

        loop {
            if cancel.is_cancelled() {
                drop(file);
                remove_partial(&path);
                return Err(DownloadError::Cancelled);
            }

            let read = match body.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    drop(file);
                    remove_partial(&path);
                    return Err(DownloadError::io(
                        format!("reading response body from {source}"),
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

            if let Some(delay) = limiter.remaining(Instant::now(), written)
                && !sleep_unless_cancelled(delay, cancel)
            {
                drop(file);
                remove_partial(&path);
                return Err(DownloadError::Cancelled);
            }

            progress(TransferProgress::new(written, declared_total));
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

        if request.verify
            && let Some(expected) = declared_total
            && expected != written
        {
            remove_partial(&path);
            return Err(DownloadError::other(format!(
                "verification failed for {source}: Content-Length declared {expected} bytes, received {written}"
            )));
        }

        let digest = if request.verify {
            match sha256_file(&path) {
                Ok(digest) => Some(digest),
                Err(error) => {
                    remove_partial(&path);
                    return Err(DownloadError::from(error));
                }
            }
        } else {
            None
        };

        if cancel.is_cancelled() {
            remove_partial(&path);
            return Err(DownloadError::Cancelled);
        }

        let destination_path = match move_to_destination(&path, &name, &request.destination) {
            Ok(path) => path,
            Err(error) => {
                remove_partial(&path);
                return Err(error);
            }
        };

        if cancel.is_cancelled() {
            remove_partial(&destination_path);
            return Err(DownloadError::Cancelled);
        }

        // One final reading settles the bar at 100% even for an empty or single-chunk body.
        let reported_total = declared_total.unwrap_or(written);
        progress(TransferProgress::new(written, Some(reported_total)));

        let outcome = TransferOutcome::new(destination_path, written, Some(reported_total));
        Ok(match digest {
            Some(digest) => outcome.with_sha256(digest),
            None => outcome,
        })
    }
}

/// Limits the average transfer rate using elapsed time versus bytes written.
///
/// There is no token bucket and no background thread: after each chunk the transfer asks how
/// much time it still owes the configured rate and sleeps that long, observing cancellation
/// while it waits.
#[derive(Debug)]
struct SpeedLimiter {
    limit_bytes_per_second: Option<u64>,
    started: Instant,
}

impl SpeedLimiter {
    /// Creates a limiter whose clock starts now. Zero and `None` mean unlimited.
    fn new(limit_bytes_per_second: Option<u64>) -> Self {
        Self {
            limit_bytes_per_second: limit_bytes_per_second.filter(|limit| *limit > 0),
            started: Instant::now(),
        }
    }

    /// Returns how long `written` bytes still owe at `now`, or `None` when the limit is off or
    /// the transfer is already at or behind the target pace.
    fn remaining(&self, now: Instant, written: u64) -> Option<Duration> {
        let limit = self.limit_bytes_per_second?;
        #[allow(clippy::cast_precision_loss)]
        let target = Duration::try_from_secs_f64(written as f64 / limit as f64).ok()?;
        let delay = target.saturating_sub(now.saturating_duration_since(self.started));
        (!delay.is_zero()).then_some(delay)
    }
}

/// Sleeps for `duration` in short slices, returning false when cancellation fires first.
///
/// Slicing keeps a cancelled job from waiting out a large throttle delay, which can be many
/// seconds when the limit is low. The wait is a real sleep, never a spin.
fn sleep_unless_cancelled(mut duration: Duration, cancel: &CancellationToken) -> bool {
    while !duration.is_zero() {
        if cancel.is_cancelled() {
            return false;
        }
        let slice = duration.min(RATE_POLL_INTERVAL);
        std::thread::sleep(slice);
        duration = duration.saturating_sub(slice);
    }
    !cancel.is_cancelled()
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

/// Moves a completed staging file into `destination`, treating it as a directory.
///
/// The destination directory is created when missing. The sanitized name is claimed with the
/// first free numeric suffix so an existing file is never overwritten.
fn move_to_destination(staged: &Path, name: &str, destination: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(destination).map_err(|error| {
        DownloadError::io(
            format!("creating destination directory {}", destination.display()),
            error,
        )
    })?;

    let name = sanitize_filename(name);
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let target = destination.join(numbered_name(&name, attempt));
        match claim_and_move(staged, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(DownloadError::io(
                    format!("moving {} to {}", staged.display(), target.display()),
                    error,
                ));
            }
        }
    }

    Err(DownloadError::other(format!(
        "could not find a free destination filename in {} for {name}",
        destination.display()
    )))
}

/// Renames `staged` onto `target`, refusing to replace an existing file.
///
/// A cross-filesystem move falls back to a copy into a newly created target and then removes
/// the staged original.
fn claim_and_move(staged: &Path, target: &Path) -> std::io::Result<()> {
    if target.try_exists()? {
        return Err(std::io::Error::from(ErrorKind::AlreadyExists));
    }

    match std::fs::rename(staged, target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::CrossesDevices => copy_into_place(staged, target),
        Err(error) => Err(error),
    }
}

/// Copies `staged` into a brand-new `target`, then removes the staged original.
fn copy_into_place(staged: &Path, target: &Path) -> std::io::Result<()> {
    let mut source = std::fs::File::open(staged)?;
    let mut sink = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;

    let copied = std::io::copy(&mut source, &mut sink).and_then(|_| sink.sync_all());
    drop(source);
    drop(sink);

    if let Err(error) = copied {
        remove_partial(target);
        return Err(error);
    }

    if let Err(error) = std::fs::remove_file(staged) {
        remove_partial(target);
        return Err(error);
    }
    Ok(())
}

/// Best-effort removal of a partially written file.
fn remove_partial(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(
            path = %path.display(),
            %error,
            "could not remove partial file"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// Temporary staging and destination directories plus request construction helpers.
    struct TestEnvironment {
        _dir: tempfile::TempDir,
        staging: PathBuf,
        destination: PathBuf,
    }

    impl TestEnvironment {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            Self {
                staging: dir.path().join("staging"),
                destination: dir.path().join("destination"),
                _dir: dir,
            }
        }

        fn transfer(&self) -> HttpClientTransfer {
            HttpClientTransfer::new(&self.staging)
        }

        fn request(&self, verify: bool) -> DownloadRequest {
            DownloadRequest {
                app_id: luatools_core::AppId::new(1),
                app_name: "test".to_owned(),
                kind: luatools_core::DownloadKind::File,
                depots: Vec::new(),
                url: Some("https://example.com/tool.bin".to_owned()),
                destination: self.destination.clone(),
                verify,
            }
        }

        /// Runs the body loop against an in-memory response.
        fn receive(
            &self,
            transfer: &HttpClientTransfer,
            body: &[u8],
            declared_total: Option<u64>,
            name: &str,
            verify: bool,
        ) -> Result<TransferOutcome> {
            let request = self.request(verify);
            let mut reader = Cursor::new(body.to_vec());
            let mut noop = |_: TransferProgress| {};
            transfer.receive(
                &mut reader,
                declared_total,
                name,
                &request,
                &mut noop,
                &CancellationToken::new(),
            )
        }

        fn staging_entries(&self) -> Vec<PathBuf> {
            match std::fs::read_dir(&self.staging) {
                Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
                Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
                Err(error) => panic!("could not list staging directory: {error}"),
            }
        }
    }

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

    #[test]
    fn a_completed_transfer_moves_into_the_destination_directory() {
        let environment = TestEnvironment::new();
        let transfer = environment.transfer();

        let outcome = environment
            .receive(&transfer, b"payload", Some(7), "tool.bin", false)
            .expect("transfer");

        let expected = environment.destination.join("tool.bin");
        assert_eq!(outcome.path, expected);
        assert_eq!(
            std::fs::read(&expected).expect("destination contents"),
            b"payload"
        );
        assert!(
            environment.staging_entries().is_empty(),
            "the staging copy must be removed after a successful move"
        );
    }

    #[test]
    fn destination_collisions_get_a_numeric_suffix_without_overwriting() {
        let environment = TestEnvironment::new();
        std::fs::create_dir_all(&environment.destination).expect("destination");
        let occupied = environment.destination.join("tool.bin");
        std::fs::write(&occupied, b"original").expect("seed destination");

        let transfer = environment.transfer();
        let outcome = environment
            .receive(&transfer, b"payload", None, "tool.bin", false)
            .expect("transfer");

        assert_eq!(
            outcome.path.file_name().and_then(|name| name.to_str()),
            Some("tool-1.bin")
        );
        assert_eq!(
            std::fs::read(&occupied).expect("original file"),
            b"original",
            "an existing file must never be overwritten"
        );
        assert_eq!(
            std::fs::read(&outcome.path).expect("moved file"),
            b"payload"
        );
    }

    #[test]
    fn verification_rejects_a_content_length_mismatch_and_discards_the_file() {
        let environment = TestEnvironment::new();
        let transfer = environment.transfer();

        let error = environment
            .receive(&transfer, b"short", Some(64), "tool.bin", true)
            .expect_err("a size mismatch must fail verification");

        let message = error.to_string();
        assert!(message.contains("verification failed"), "{message}");
        assert!(message.contains("64"), "{message}");
        assert!(message.contains('5'), "{message}");
        assert!(environment.staging_entries().is_empty());
        assert!(!environment.destination.join("tool.bin").exists());
    }

    #[test]
    fn verification_is_skipped_when_not_requested() {
        let environment = TestEnvironment::new();
        let transfer = environment.transfer();

        let outcome = environment
            .receive(&transfer, b"short", Some(64), "tool.bin", false)
            .expect("verification is off, so the length mismatch is tolerated");

        assert_eq!(outcome.bytes, 5);
        assert_eq!(std::fs::read(&outcome.path).expect("moved file"), b"short");
    }

    #[test]
    fn verification_exposes_the_sha256_digest() {
        let environment = TestEnvironment::new();
        let transfer = environment.transfer();

        let outcome = environment
            .receive(&transfer, b"abc", Some(3), "tool.bin", true)
            .expect("verified transfer");

        assert_eq!(
            outcome.sha256,
            Some(luatools_core::hash::sha256_bytes(b"abc"))
        );
    }

    #[test]
    fn destination_names_are_sanitized() {
        let environment = TestEnvironment::new();
        let transfer = environment.transfer();

        let outcome = environment
            .receive(&transfer, b"payload", None, "../../evil.bin", false)
            .expect("transfer");

        assert_eq!(outcome.path, environment.destination.join("evil.bin"));
    }

    #[test]
    fn a_failed_move_removes_the_staged_file() {
        let environment = TestEnvironment::new();
        // A regular file where the destination directory should be makes create_dir_all fail.
        std::fs::write(&environment.destination, b"not a directory").expect("block destination");
        let transfer = environment.transfer();

        let error = environment
            .receive(&transfer, b"payload", None, "tool.bin", false)
            .expect_err("the move must fail");

        assert!(error.to_string().contains("destination"), "{error}");
        assert!(
            environment.staging_entries().is_empty(),
            "a failed move must not leave the staged file behind"
        );
    }

    #[test]
    fn cancel_mid_transfer_removes_the_partial_file() {
        let environment = TestEnvironment::new();
        let transfer = environment.transfer();
        let request = environment.request(false);

        // Two chunks: the first progress callback cancels, so the second read never happens.
        let body = vec![0x5a_u8; COPY_BUFFER_SIZE * 2];
        let mut reader = Cursor::new(body);
        let token = CancellationToken::new();
        let cancel_from_progress = token.clone();
        let mut progress = move |reading: TransferProgress| {
            if reading.bytes_read > 0 {
                cancel_from_progress.cancel();
            }
        };

        let error = transfer
            .receive(
                &mut reader,
                None,
                "tool.bin",
                &request,
                &mut progress,
                &token,
            )
            .expect_err("cancellation must win over a complete body");

        assert!(matches!(error, DownloadError::Cancelled));
        assert!(environment.staging_entries().is_empty());
        assert!(!environment.destination.join("tool.bin").exists());
    }

    #[test]
    fn speed_limiter_reports_the_time_still_owed() {
        let start = Instant::now();
        let limiter = SpeedLimiter {
            limit_bytes_per_second: Some(1_000),
            started: start,
        };

        assert_eq!(limiter.remaining(start, 0), None);
        assert_eq!(
            limiter.remaining(start + Duration::from_millis(100), 250),
            Some(Duration::from_millis(150))
        );
        assert_eq!(
            limiter.remaining(start + Duration::from_secs(1), 500),
            None,
            "a transfer ahead of the limit must not wait"
        );
        assert_eq!(SpeedLimiter::new(Some(0)).limit_bytes_per_second, None);
        assert_eq!(SpeedLimiter::new(None).limit_bytes_per_second, None);
    }

    #[test]
    fn a_throttled_transfer_waits_for_the_configured_rate() {
        let environment = TestEnvironment::new();
        let transfer = environment.transfer().with_speed_limit(Some(80_000));
        let minimum = Duration::from_millis(50);

        let started = Instant::now();
        let outcome = environment
            .receive(&transfer, &[0x42; 4_000], Some(4_000), "tool.bin", false)
            .expect("throttled transfer");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= minimum.saturating_sub(Duration::from_millis(2)),
            "4 000 bytes at 80 000 B/s should take at least {minimum:?}, took {elapsed:?}"
        );
        assert_eq!(outcome.bytes, 4_000);
    }

    #[test]
    fn a_cancelled_wait_stops_immediately() {
        let token = CancellationToken::new();
        token.cancel();

        let started = Instant::now();
        assert!(!sleep_unless_cancelled(Duration::from_secs(30), &token));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
