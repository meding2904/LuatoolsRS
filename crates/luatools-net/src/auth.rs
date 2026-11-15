//! PKCE helpers, the signed-in session shape, and a file-backed secret store.
//!
//! The interactive OAuth flow is driven by the app shell; this module provides the pieces that
//! are independent of any windowing toolkit: a verifier/challenge pair, the session payload, and
//! persistence with owner-only permissions on Unix.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use luatools_core::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Length of the random verifier in bytes. Base64url expands this to 64 characters.
const VERIFIER_BYTES: usize = 48;

/// A PKCE verifier and its S256 challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// The high-entropy verifier kept by the client.
    pub verifier: String,
    /// `base64url(SHA-256(verifier))`, sent to the authorization server.
    pub challenge: String,
}

impl Pkce {
    /// Derives the challenge for an existing verifier.
    #[must_use]
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        let verifier = verifier.into();
        Self {
            challenge: code_challenge(&verifier),
            verifier,
        }
    }
}

/// Generates a fresh PKCE verifier and challenge.
///
/// The verifier comes from the operating system's cryptographic random source. If that source is
/// unavailable the call logs a warning and falls back to a SHA-256 of per-process entropy rather
/// than failing sign-in; [`try_pkce_pair`] is available when the caller would rather see the
/// error.
#[must_use]
pub fn pkce_pair() -> Pkce {
    try_pkce_pair().unwrap_or_else(|error| {
        tracing::warn!(%error, "OS random source unavailable; deriving a fallback PKCE verifier");
        Pkce::from_verifier(fallback_verifier())
    })
}

/// Generates a fresh PKCE verifier and challenge from the OS random source.
///
/// # Errors
///
/// Returns [`Error::Other`] when the operating system's random source cannot be read.
pub fn try_pkce_pair() -> Result<Pkce> {
    let mut bytes = [0_u8; VERIFIER_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::Other(format!("generating PKCE verifier: {error}")))?;
    Ok(Pkce::from_verifier(base64url(&bytes)))
}

fn fallback_verifier() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let mut hasher = Sha256::new();
    hasher.update(now.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update((&COUNTER as *const AtomicU64 as usize).to_le_bytes());
    hasher.update(format!("{:?}", std::thread::current().id()).as_bytes());
    base64url(&hasher.finalize())
}

/// Computes the S256 code challenge for `verifier`.
#[must_use]
pub fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64url(&digest)
}

/// Encodes bytes as unpadded base64url (RFC 4648 §5).
#[must_use]
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let bits = (first << 16) | (second << 8) | third;
        encoded.push(char::from(ALPHABET[(bits >> 18) as usize & 0x3f]));
        encoded.push(char::from(ALPHABET[(bits >> 12) as usize & 0x3f]));
        if chunk.len() > 1 {
            encoded.push(char::from(ALPHABET[(bits >> 6) as usize & 0x3f]));
        }
        if chunk.len() > 2 {
            encoded.push(char::from(ALPHABET[bits as usize & 0x3f]));
        }
    }
    encoded
}

/// A signed-in account session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Bearer token sent with authenticated requests.
    pub access_token: String,
    /// Token used to mint a new access token, when the provider issued one.
    pub refresh_token: Option<String>,
    /// Access-token expiry in UTC.
    pub expires_at: DateTime<Utc>,
    /// Display name shown in the interface.
    pub display_name: Option<String>,
    /// Account e-mail, when the provider shares it.
    pub email: Option<String>,
    /// Avatar image URL.
    pub avatar_url: Option<String>,
}

impl Session {
    /// True when the access token is at or past its expiry.
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }

    /// True when the token expires within `skew`, so a refresh should run before the next call.
    #[must_use]
    pub fn needs_refresh(&self, now: DateTime<Utc>, skew: ChronoDuration) -> bool {
        match now.checked_add_signed(skew) {
            Some(deadline) => self.expires_at <= deadline,
            None => true,
        }
    }

    /// True when a usable refresh token is present.
    #[must_use]
    pub fn has_refresh_token(&self) -> bool {
        self.refresh_token
            .as_deref()
            .is_some_and(|token| !token.is_empty())
    }
}

/// Persists and restores the account session.
pub trait SecretStore: Send + Sync {
    /// Loads a stored session, or `None` when none exists.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing file cannot be read or parsed. A missing file is not an
    /// error.
    fn load(&self) -> Result<Option<Session>>;

    /// Saves `session`, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be serialized or written.
    fn save(&self, session: &Session) -> Result<()>;

    /// Removes the stored session. Removing an absent session succeeds.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be removed.
    fn clear(&self) -> Result<()>;
}

/// A JSON secret store whose file is readable only by its owner on Unix.
///
/// The platform keychain is preferable when available; this is the portable fallback the
/// specification calls for. On Unix the file is created with mode `0600` and re-checked after
/// every write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    /// Builds a store at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The backing file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_session(&self, session: &Session) -> Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
        }

        let json = serde_json::to_vec_pretty(session)?;
        let temp = temp_path(&self.path);
        match std::fs::remove_file(&temp) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::io(
                    format!("clearing stale {}", temp.display()),
                    error,
                ));
            }
        }

        let write_result = (|| -> Result<()> {
            let mut file = create_private(&temp)?;
            file.write_all(&json)
                .map_err(|error| Error::io(format!("writing {}", temp.display()), error))?;
            file.sync_all()
                .map_err(|error| Error::io(format!("syncing {}", temp.display()), error))?;
            drop(file);
            replace_file(&temp, &self.path)
        })();

        if write_result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        write_result
    }
}

impl SecretStore for FileSecretStore {
    fn load(&self) -> Result<Option<Session>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::io(format!("reading {}", self.path.display()), error));
            }
        };
        let session = serde_json::from_slice(&bytes).map_err(|error| {
            Error::parse("auth session", format!("{}: {error}", self.path.display()))
        })?;
        Ok(Some(session))
    }

    fn save(&self, session: &Session) -> Result<()> {
        self.write_session(session)
    }

    fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io(
                format!("removing {}", self.path.display()),
                error,
            )),
        }
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| Error::io(format!("creating {}", path.display()), error))?;
    // `mode` is filtered through the process umask; set it explicitly so a permissive umask
    // cannot leave tokens world-readable.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| Error::io(format!("securing {}", path.display()), error))?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| Error::io(format!("creating {}", path.display()), error))
}

fn replace_file(temp: &Path, dest: &Path) -> Result<()> {
    if std::fs::rename(temp, dest).is_ok() {
        return Ok(());
    }
    match std::fs::remove_file(dest) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::io(format!("replacing {}", dest.display()), error));
        }
    }
    std::fs::rename(temp, dest)
        .map_err(|error| Error::io(format!("replacing {}", dest.display()), error))
}

fn temp_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    dest.with_file_name(format!("{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> Session {
        Session {
            access_token: "access-123".to_owned(),
            refresh_token: Some("refresh-456".to_owned()),
            expires_at: Utc::now() + ChronoDuration::hours(1),
            display_name: Some("Tester".to_owned()),
            email: Some("tester@example.com".to_owned()),
            avatar_url: Some("https://cdn/avatar.png".to_owned()),
        }
    }

    #[test]
    fn challenge_matches_the_rfc_7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn base64url_is_unpadded_and_url_safe() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff, 0xbf]), "-_-_");
        for byte in 0_u8..=255 {
            let encoded = base64url(&[byte, byte, byte]);
            assert!(!encoded.contains(['=', '+', '/']), "{encoded}");
        }
    }

    #[test]
    fn pkce_pair_has_a_matching_challenge_and_a_legal_length() {
        let first = pkce_pair();
        let second = pkce_pair();
        assert_eq!(first.challenge, code_challenge(&first.verifier));
        assert!((43..=128).contains(&first.verifier.len()));
        assert_eq!(first.verifier.len(), 64);
        assert_ne!(first.verifier, second.verifier);
        assert!(!first.verifier.contains(['=', '+', '/']));
    }

    #[test]
    fn session_refresh_window() {
        let now = Utc::now();
        let session = Session {
            expires_at: now + ChronoDuration::minutes(1),
            ..sample_session()
        };
        assert!(session.needs_refresh(now, ChronoDuration::minutes(2)));
        assert!(!session.needs_refresh(now, ChronoDuration::seconds(30)));
        assert!(!session.is_expired(now));
        assert!(session.is_expired(now + ChronoDuration::minutes(2)));
        assert!(session.has_refresh_token());
    }

    #[test]
    fn secret_store_round_trip_and_clear() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = FileSecretStore::new(dir.path().join("nested").join("auth.json"));
        assert_eq!(store.load().expect("missing file is not an error"), None);

        let session = sample_session();
        store.save(&session).expect("save");
        assert_eq!(store.load().expect("load"), Some(session));

        store.clear().expect("clear");
        assert_eq!(store.load().expect("load after clear"), None);
        store.clear().expect("clearing again succeeds");
    }

    #[cfg(unix)]
    #[test]
    fn secret_store_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("auth.json");
        let store = FileSecretStore::new(&path);
        store.save(&sample_session()).expect("save");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn corrupt_session_file_reports_a_parse_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("auth.json");
        let mut file = std::fs::File::create(&path).expect("create");
        file.write_all(b"{ not json").expect("write");
        let store = FileSecretStore::new(&path);
        let error = store.load().expect_err("corrupt file must fail");
        assert!(error.to_string().contains("auth session"), "{error}");
    }
}
