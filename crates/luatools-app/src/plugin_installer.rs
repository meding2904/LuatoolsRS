//! Steam store plugin installation, update, and removal.
//!
//! Each `madoiscool/LTSP` release publishes two assets: `plugin.zip`, the store frontend
//! extracted into the application's data directory, and `winmm.dll`, a loader placed in the
//! Steam installation root where `steam.exe` loads it. This module implements the manager side:
//! it downloads both assets, verifies the SHA-256 digests GitHub publishes, extracts the archive
//! without allowing entries to escape the destination, and records the installed release in an
//! `installed.json` manifest next to the frontend.
//!
//! The loader is a Windows DLL and the rest of the integration is Windows-only; every other
//! platform reports the plugin as unsupported. Install and uninstall stop Steam before touching
//! a loader DLL and restart it afterwards when it was running, because Windows keeps the file
//! locked while the client is up.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use luatools_core::{AppPaths, Error, PluginStatus, Result};
use luatools_net::plugin::{
    FRONTEND_ASSET, LOADER_ASSET, PluginInstaller, UnsupportedPluginInstaller,
};
use serde::{Deserialize, Serialize};

/// Latest-release endpoint for the plugin repository.
const RELEASE_URL: &str = "https://api.github.com/repos/madoiscool/LTSP/releases/latest";

/// Folder under the application data directory that holds the frontend and manifest.
const FRONTEND_SUBDIR: &str = "plugin";

/// Manifest file name inside the frontend folder.
const MANIFEST_FILE: &str = "installed.json";

/// Entry point inside the frontend archive, used to confirm extraction succeeded.
const FRONTEND_ENTRY: &str = "public/luatools.js";

/// Forwarding copy the loader calls into instead of the system DLL.
const FORWARDER_ASSET: &str = "winmm_real.dll";

/// System DLL copied to [`FORWARDER_ASSET`].
const SYSTEM_LOADER: &str = "winmm.dll";

/// Loader slots shipped by older releases. Leaving one installed runs the loader more than once.
const LEGACY_LOADERS: &[&str] = &[
    "bcrypt.dll",
    "bcrypt_real.dll",
    "psapi.dll",
    "psapi_real.dll",
    "dbghelp.dll",
    "dbghelp_real.dll",
];

/// Marker Steam treats as "enable the remote debugging port".
const CDP_MARKER_NAME: &str = ".cef-enable-remote-debugging";

/// Junction target that is deliberately never created.
const CDP_MARKER_TARGET: &str = r"C:\LuaTools\cdp-marker-target-that-does-not-exist";

/// Environment override for the Steam installation root.
const STEAM_ROOT_ENV: &str = "LUATOOLS_STEAM_ROOT";

/// How long a fetched release is reused for status checks.
const RELEASE_CACHE_TTL: Duration = Duration::from_secs(300);

/// Time allowed for Steam to release the loader lock after a shutdown request.
const STEAM_EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Windows file attribute marking a junction or symlink.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// Counter making temporary directory names unique within one process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Windows installer for the Steam store plugin.
///
/// The type is available on every target so callers can name it, but only the Windows build
/// performs real work. [`installer`] decides which implementation the application receives.
pub struct PlatformPluginInstaller {
    paths: AppPaths,
    steam_root: Option<PathBuf>,
    http: reqwest::Client,
    latest: Mutex<Option<CachedRelease>>,
}

/// One cached GitHub release.
struct CachedRelease {
    fetched_at: Instant,
    release: GithubRelease,
}

/// GitHub's `releases/latest` payload, restricted to the fields this module uses.
#[derive(Debug, Clone, Default, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

impl GithubRelease {
    /// Finds an asset by name, case-insensitively.
    fn asset(&self, name: &str) -> Option<&GithubAsset> {
        self.assets
            .iter()
            .find(|asset| asset.name.eq_ignore_ascii_case(name))
    }
}

/// One downloadable release asset.
#[derive(Debug, Clone, Default, Deserialize)]
struct GithubAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    /// `sha256:<hex>` when GitHub computed a digest for the asset.
    #[serde(default)]
    digest: Option<String>,
}

/// Contents of `installed.json`.
///
/// GitHub digests are trusted for verification, but the manifest keeps the hashes that were
/// actually installed so a later run can detect a replaced loader and repair an older release
/// that did not publish a digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginManifest {
    /// Release tag that was installed.
    #[serde(default, alias = "Tag", skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    /// SHA-256 of every installed loader, keyed by asset name.
    #[serde(default, alias = "DllShas", skip_serializing_if = "BTreeMap::is_empty")]
    dll_shas: BTreeMap<String, String>,
    /// SHA-256 of the downloaded `plugin.zip`.
    #[serde(default, alias = "ZipSha", skip_serializing_if = "Option::is_none")]
    zip_sha: Option<String>,
}

impl PluginManifest {
    /// SHA-256 recorded for the loader, matching the key case-insensitively.
    fn loader_sha(&self) -> Option<&str> {
        self.dll_shas
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(LOADER_ASSET))
            .map(|(_, sha)| sha.as_str())
    }
}

/// The local installation state used to decide what [`PluginStatus`] to report.
#[derive(Debug, Clone, Default)]
struct LocalInstall {
    /// `public/luatools.js` exists.
    frontend: bool,
    /// A current or legacy loader is present.
    loader: bool,
    /// A loader from an older release is still present.
    legacy: bool,
    /// Parsed manifest, when one exists.
    manifest: Option<PluginManifest>,
    /// SHA-256 of the current loader, when it could be read.
    loader_sha: Option<String>,
}

impl PlatformPluginInstaller {
    /// Builds an installer for the current user.
    ///
    /// The application paths follow [`AppPaths`], including its `LUATOOLS_HOME` override. The
    /// Steam root comes from `LUATOOLS_STEAM_ROOT` when set and otherwise from
    /// [`luatools_steam::find_steam_root`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Other`] when the application directories cannot be resolved, or a
    /// network error when the shared HTTP client cannot be built.
    pub fn new() -> Result<Self> {
        let paths = AppPaths::discover()?;
        let http = luatools_net::http::build(&luatools_net::http::HttpClientConfig::default())?;
        Ok(Self {
            paths,
            steam_root: detect_steam_root(),
            http,
            latest: Mutex::new(None),
        })
    }

    /// Overrides the detected Steam root, for a user-configured path.
    #[must_use]
    pub fn with_steam_root(mut self, root: Option<PathBuf>) -> Self {
        if root.is_some() {
            self.steam_root = root;
        }
        self
    }

    /// Resolves the Steam installation root.
    fn steam_root(&self) -> Option<&Path> {
        self.steam_root.as_deref()
    }

    /// Path of the current loader inside the Steam root.
    fn loader_path(&self) -> Option<PathBuf> {
        self.steam_root().map(|root| root.join(LOADER_ASSET))
    }

    /// Reads the frontend, loader, legacy slots, and manifest from disk.
    fn local_install(&self) -> LocalInstall {
        let manifest = read_manifest(&manifest_path(&self.paths));
        let frontend = frontend_entry(&self.paths).is_file();
        let loader_path = self.loader_path();
        let current_loader = loader_path.as_deref().filter(|path| path.is_file());
        let legacy = self.steam_root().is_some_and(has_legacy_loader);
        let loader_sha = current_loader.and_then(|path| match luatools_core::hash::sha256_file(path)
        {
            Ok(digest) => Some(digest.as_str().to_owned()),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "hashing the installed plugin loader failed");
                None
            }
        });
        LocalInstall {
            frontend,
            loader: current_loader.is_some() || legacy,
            legacy,
            manifest,
            loader_sha,
        }
    }

    /// Returns a cached release, or fetches a fresh one when the cache is cold or forced.
    #[cfg(windows)]
    fn latest_release(&self, force: bool) -> Result<GithubRelease> {
        if !force {
            let cached = self.release_cache()?;
            if let Some(cached) = cached.as_ref()
                && cached.fetched_at.elapsed() < RELEASE_CACHE_TTL
            {
                return Ok(cached.release.clone());
            }
        }

        let release = fetch_release(&self.http)?;
        let mut cached = self.release_cache()?;
        *cached = Some(CachedRelease {
            fetched_at: Instant::now(),
            release: release.clone(),
        });
        Ok(release)
    }

    /// Locks the release cache, reporting poisoning instead of panicking.
    #[cfg(windows)]
    fn release_cache(&self) -> Result<std::sync::MutexGuard<'_, Option<CachedRelease>>> {
        self.latest
            .lock()
            .map_err(|_| Error::Other("the plugin release cache is poisoned".to_owned()))
    }

    /// Reads the installed state and compares it with the latest release.
    #[cfg(windows)]
    fn status_windows(&self) -> Result<PluginStatus> {
        let local = self.local_install();

        if local.loader
            && let Some(root) = self.steam_root()
        {
            ensure_cdp_marker(root);
        }

        if let Some(root) = self.steam_root() {
            tracing::debug!(
                steam = %root.display(),
                millennium = millennium_present(root),
                "inspecting the Steam plugin installation"
            );
        }

        match self.latest_release(false) {
            Ok(release) => Ok(status_from_local(&local, &release)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the latest plugin release could not be checked; reporting the local state"
                );
                Ok(offline_status(&local, &error.to_string()))
            }
        }
    }

    /// Installs or updates the frontend and loader.
    #[cfg(windows)]
    fn install_windows(&self, progress: &mut dyn FnMut(f32)) -> Result<()> {
        progress(0.02);

        let steam_root = self
            .steam_root()
            .filter(|root| root.is_dir())
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                Error::Other("the Steam installation directory was not found".to_owned())
            })?;
        let manifest = read_manifest(&manifest_path(&self.paths));
        let release = self.latest_release(true)?;
        let tag = release.tag_name.trim().to_owned();

        let zip_asset = release
            .asset(FRONTEND_ASSET)
            .ok_or_else(|| missing_asset(&release, FRONTEND_ASSET))?;
        let loader_asset = release
            .asset(LOADER_ASSET)
            .ok_or_else(|| missing_asset(&release, LOADER_ASSET))?;
        require_https(&zip_asset.browser_download_url)?;
        require_https(&loader_asset.browser_download_url)?;
        validate_release_digest(&release, FRONTEND_ASSET)?;
        validate_release_digest(&release, LOADER_ASSET)?;
        progress(0.15);

        let downloads = TempDir::create("plugin-download")?;
        let downloaded_zip = downloads.path().join(FRONTEND_ASSET);
        let downloaded_loader = downloads.path().join(LOADER_ASSET);
        let (zip_sha, loader_sha) = download_assets(
            &self.http,
            &zip_asset.browser_download_url,
            &loader_asset.browser_download_url,
            &downloaded_zip,
            &downloaded_loader,
        )?;
        progress(0.45);

        let zip_expected = verification_digest(&release, manifest.as_ref(), FRONTEND_ASSET);
        verify_digest(FRONTEND_ASSET, &zip_sha, zip_expected.as_deref())?;
        if zip_expected.is_none() {
            tracing::warn!(
                asset = FRONTEND_ASSET,
                "the release published no SHA-256 digest; the archive was not verified"
            );
        }

        let loader_expected = verification_digest(&release, manifest.as_ref(), LOADER_ASSET);
        verify_digest(LOADER_ASSET, &loader_sha, loader_expected.as_deref())?;
        if loader_expected.is_none() {
            tracing::warn!(
                asset = LOADER_ASSET,
                "the release published no SHA-256 digest; the loader was not verified"
            );
        }

        // Extract beside the live frontend first, so a malformed archive cannot destroy a
        // working installation. The staging directory shares the data volume with its final
        // location, which keeps the rename below a same-volume move.
        let plugin_dir = frontend_dir(&self.paths);
        let staging = TempDir::create_in(&self.paths.data_dir(), "plugin-staging")?;
        extract_zip(&downloaded_zip, staging.path())?;
        normalize_frontend_layout(staging.path())?;
        let entry = staging.path().join(FRONTEND_ENTRY);
        if !entry.is_file() {
            return Err(Error::NotFound { path: entry });
        }
        replace_frontend(&plugin_dir, staging.path())?;
        progress(0.65);

        let installed_loader = steam_root.join(LOADER_ASSET);
        let forwarder = steam_root.join(FORWARDER_ASSET);
        let loader_matches = installed_loader.is_file()
            && luatools_core::hash::sha256_file(&installed_loader)
                .is_ok_and(|digest| digest.as_str().eq_ignore_ascii_case(&loader_sha));
        if !loader_matches || !forwarder.is_file() || has_legacy_loader(&steam_root) {
            let was_running = steam_is_running();
            if was_running {
                stop_steam(&steam_root);
                if !wait_for_steam_exit(STEAM_EXIT_TIMEOUT) {
                    tracing::warn!("Steam did not exit before the plugin loader update");
                }
            } else {
                // Give a stale process that tasklist did not report a moment to release handles.
                std::thread::sleep(Duration::from_millis(250));
            }

            let placement = (|| -> Result<()> {
                fs::copy(&downloaded_loader, &installed_loader).map_err(|error| {
                    Error::io(format!("installing {}", installed_loader.display()), error)
                })?;
                if let Some(system_loader) = system_loader_path() {
                    fs::copy(&system_loader, &forwarder).map_err(|error| {
                        Error::io(format!("installing {}", forwarder.display()), error)
                    })?;
                } else {
                    tracing::warn!(
                        "the system winmm.dll was not found; the forwarding copy was not updated"
                    );
                }
                remove_legacy_loaders(&steam_root);
                Ok(())
            })();

            if was_running {
                start_steam(&steam_root);
            }
            placement?;
        }

        ensure_cdp_marker(&steam_root);
        progress(0.9);

        let mut dll_shas = BTreeMap::new();
        dll_shas.insert(LOADER_ASSET.to_owned(), loader_sha.clone());
        write_manifest(
            &manifest_path(&self.paths),
            &PluginManifest {
                tag: Some(tag),
                dll_shas,
                zip_sha: Some(zip_sha),
            },
        )?;
        progress(1.0);
        Ok(())
    }

    /// Removes the loader, frontend, marker junction, and manifest.
    #[cfg(windows)]
    fn uninstall_windows(&self) -> Result<()> {
        let manifest = read_manifest(&manifest_path(&self.paths));
        if let Some(manifest) = manifest.as_ref() {
            tracing::debug!(
                tag = manifest.tag.as_deref().unwrap_or("unknown"),
                "removing the Steam plugin"
            );
        }

        if let Some(steam_root) = self.steam_root().filter(|root| root.is_dir()) {
            let loader = steam_root.join(LOADER_ASSET);
            let forwarder = steam_root.join(FORWARDER_ASSET);
            let legacy = has_legacy_loader(steam_root);
            let needs_steam_stopped = loader.is_file() || forwarder.is_file() || legacy;
            let was_running = needs_steam_stopped && steam_is_running();

            if was_running {
                stop_steam(steam_root);
                if !wait_for_steam_exit(STEAM_EXIT_TIMEOUT) {
                    tracing::warn!("Steam did not exit before the plugin loader removal");
                }
            }

            let removal = (|| -> Result<()> {
                remove_file_if_exists(&loader, "removing the plugin loader")?;
                remove_file_if_exists(&forwarder, "removing the plugin forwarding copy")?;
                // Remove old slots while Steam is still stopped so they cannot be loaded again.
                remove_legacy_loaders(steam_root);
                Ok(())
            })();

            if was_running {
                start_steam(steam_root);
            }

            if let Err(error) = removal {
                // Keep the frontend in place when the loader could not be removed, so the
                // installation still reports as present and the user can retry.
                remove_cdp_marker(steam_root);
                return Err(error);
            }

            remove_cdp_marker(steam_root);
        }

        remove_file_if_exists(&manifest_path(&self.paths), "removing the plugin manifest")?;
        remove_dir_if_exists(&frontend_dir(&self.paths), "removing the plugin frontend")?;
        Ok(())
    }
}

/// Returns the platform plugin installer.
///
/// Windows receives [`PlatformPluginInstaller`]; every other platform receives
/// [`UnsupportedPluginInstaller`] because the loader is a Windows DLL.
#[must_use]
pub fn installer() -> Box<dyn PluginInstaller> {
    installer_with_root(None)
}

/// Returns the platform plugin installer with an explicit Steam-root override.
#[must_use]
pub fn installer_with_root(root: Option<&Path>) -> Box<dyn PluginInstaller> {
    #[cfg(windows)]
    {
        match PlatformPluginInstaller::new() {
            Ok(installer) => {
                return Box::new(installer.with_steam_root(root.map(Path::to_path_buf)));
            }
            Err(error) => {
                tracing::warn!(%error, "the plugin installer could not be initialised");
            }
        }
    }
    let _ = root;
    Box::new(UnsupportedPluginInstaller)
}

#[cfg(windows)]
impl PluginInstaller for PlatformPluginInstaller {
    fn status(&self) -> Result<PluginStatus> {
        self.status_windows()
    }

    fn install(&self, progress: &mut dyn FnMut(f32)) -> Result<()> {
        self.install_windows(progress)
    }

    fn uninstall(&self) -> Result<()> {
        self.uninstall_windows()
    }
}

#[cfg(not(windows))]
impl PluginInstaller for PlatformPluginInstaller {
    fn status(&self) -> Result<PluginStatus> {
        Err(Error::Unsupported("Steam store plugin installation"))
    }

    fn install(&self, _progress: &mut dyn FnMut(f32)) -> Result<()> {
        Err(Error::Unsupported("Steam store plugin installation"))
    }

    fn uninstall(&self) -> Result<()> {
        Err(Error::Unsupported("Steam store plugin installation"))
    }
}

/// Directory holding the extracted frontend.
fn frontend_dir(paths: &AppPaths) -> PathBuf {
    paths.data_dir().join(FRONTEND_SUBDIR)
}

/// Frontend entry point used for the installed check.
fn frontend_entry(paths: &AppPaths) -> PathBuf {
    frontend_dir(paths).join(FRONTEND_ENTRY)
}

/// Manifest path for the installed release.
fn manifest_path(paths: &AppPaths) -> PathBuf {
    frontend_dir(paths).join(MANIFEST_FILE)
}

/// Resolves the Steam root from the environment override or normal discovery.
fn detect_steam_root() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(STEAM_ROOT_ENV) {
        let text = raw.to_string_lossy();
        let text = text.trim();
        if !text.is_empty() {
            let path = PathBuf::from(text);
            if path.is_absolute() {
                return Some(path);
            }
            if let Ok(current) = std::env::current_dir() {
                return Some(current.join(path));
            }
            return Some(path);
        }
    }
    luatools_steam::find_steam_root(None)
}

/// Parses a GitHub `digest` field, accepting the documented `sha256:<hex>` form.
fn parse_sha256_digest(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let value = match trimmed.split_once(':') {
        Some((algorithm, value)) if algorithm.eq_ignore_ascii_case("sha256") => value.trim(),
        Some(_) => return None,
        None => trimmed,
    };
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(value.to_ascii_lowercase())
    } else {
        None
    }
}

/// Returns the verified digest for an asset, if the release or manifest supplies one.
///
/// The release digest always wins. When GitHub omitted it, the manifest is a fallback only for
/// the same release tag: those hashes describe files already installed, not a newer release.
fn verification_digest(
    release: &GithubRelease,
    manifest: Option<&PluginManifest>,
    asset_name: &str,
) -> Option<String> {
    if let Some(digest) = release
        .asset(asset_name)
        .and_then(|asset| asset.digest.as_deref())
        .and_then(parse_sha256_digest)
    {
        return Some(digest);
    }

    let manifest = manifest?;
    let same_release = manifest
        .tag
        .as_deref()
        .is_some_and(|tag| tags_match(tag, &release.tag_name));
    if !same_release {
        return None;
    }
    if asset_name.eq_ignore_ascii_case(FRONTEND_ASSET) {
        manifest.zip_sha.clone()
    } else if asset_name.eq_ignore_ascii_case(LOADER_ASSET) {
        manifest.loader_sha().map(str::to_owned)
    } else {
        None
    }
}

/// Rejects a release digest that is present but not a SHA-256 digest.
///
/// A malformed digest is a reason not to install rather than a reason to skip verification.
fn validate_release_digest(release: &GithubRelease, asset_name: &str) -> Result<()> {
    let Some(raw) = release
        .asset(asset_name)
        .and_then(|asset| asset.digest.as_deref())
    else {
        return Ok(());
    };
    if parse_sha256_digest(raw).is_some() {
        Ok(())
    } else {
        Err(Error::parse(
            "github asset digest",
            format!("{asset_name}: {raw}"),
        ))
    }
}

/// Rejects a download whose bytes do not match an expected digest.
fn verify_digest(asset: &str, actual: &str, expected: Option<&str>) -> Result<()> {
    if let Some(expected) = expected
        && !actual.eq_ignore_ascii_case(expected)
    {
        return Err(Error::Other(format!(
            "{asset} verification failed: expected sha256 {expected}, got {actual}"
        )));
    }
    Ok(())
}

/// Parses the plugin manifest.
fn parse_manifest(text: &str) -> Result<PluginManifest> {
    serde_json::from_str(text).map_err(|error| Error::parse("plugin manifest", error.to_string()))
}

/// Reads the manifest, treating a missing or malformed file as "not recorded".
fn read_manifest(path: &Path) -> Option<PluginManifest> {
    match fs::read_to_string(path) {
        Ok(text) => match parse_manifest(&text) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "ignoring an invalid plugin manifest"
                );
                None
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "the plugin manifest could not be read"
            );
            None
        }
    }
}

/// Writes the manifest, creating the frontend directory when needed.
fn write_manifest(path: &Path, manifest: &PluginManifest) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Other(format!("manifest path has no parent: {}", path.display())))?;
    fs::create_dir_all(parent)
        .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
    let text = serde_json::to_string_pretty(manifest)?;
    fs::write(path, text).map_err(|error| Error::io(format!("writing {}", path.display()), error))
}

/// Validates a zip entry path relative to the extraction root.
///
/// Empty and `.` components are ignored. Parent components, absolute paths, backslashes,
/// invalid Windows component names, and control characters are rejected.
fn safe_archive_path(name: &str) -> Result<PathBuf> {
    if name.is_empty() {
        return Err(Error::InvalidInput(
            "archive entry has an empty name".to_owned(),
        ));
    }
    if name.contains('\\') {
        return Err(Error::InvalidInput(format!(
            "archive entry contains a backslash: {name}"
        )));
    }
    if name.starts_with('/') {
        return Err(Error::InvalidInput(format!(
            "archive entry is absolute: {name}"
        )));
    }

    let mut relative = PathBuf::new();
    for component in name.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                return Err(Error::InvalidInput(format!(
                    "archive entry escapes the destination: {name}"
                )));
            }
            _ => {}
        }
        let component = luatools_net::plugin::safe_asset_name(component)
            .map_err(|error| error.context(format!("archive entry {name}")))?;
        relative.push(component);
    }

    if relative.as_os_str().is_empty() {
        return Err(Error::InvalidInput(format!(
            "archive entry has no path: {name}"
        )));
    }
    Ok(relative)
}

/// Extracts `plugin.zip` after validating every entry path.
#[cfg(windows)]
fn extract_zip(zip_path: &Path, destination: &Path) -> Result<()> {
    let file = fs::File::open(zip_path)
        .map_err(|error| Error::io(format!("opening {}", zip_path.display()), error))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| Error::parse("plugin.zip", error.to_string()))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| Error::parse("plugin.zip", error.to_string()))?;
        let name = entry.name().to_owned();
        let relative = safe_archive_path(&name)?;
        let target = destination.join(&relative);
        if !target.starts_with(destination) {
            return Err(Error::InvalidInput(format!(
                "archive entry escapes the destination: {name}"
            )));
        }

        if entry.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|error| Error::io(format!("creating {}", target.display()), error))?;
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
        }
        let mut output = fs::File::create(&target)
            .map_err(|error| Error::io(format!("creating {}", target.display()), error))?;
        io::copy(&mut entry, &mut output)
            .map_err(|error| Error::io(format!("writing {}", target.display()), error))?;
    }
    Ok(())
}

/// Hoists a single wrapper directory inside the frontend archive.
fn normalize_frontend_layout(root: &Path) -> Result<()> {
    if root.join("public").is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(root)
        .map_err(|error| Error::io(format!("reading {}", root.display()), error))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| Error::io(format!("reading {}", root.display()), error))?;
        let wrapper = entry.path();
        if !wrapper.is_dir() || !wrapper.join("public").is_dir() {
            continue;
        }

        let children = fs::read_dir(&wrapper)
            .map_err(|error| Error::io(format!("reading {}", wrapper.display()), error))?;
        for child in children {
            let child = child
                .map_err(|error| Error::io(format!("reading {}", wrapper.display()), error))?;
            let source = child.path();
            let Some(name) = source.file_name() else {
                continue;
            };
            let destination = root.join(name);
            if destination.exists() {
                return Err(Error::Other(format!(
                    "the frontend archive contains both {name:?} and a wrapper directory"
                )));
            }
            fs::rename(&source, &destination).map_err(|error| {
                Error::io(
                    format!("moving {} to {}", source.display(), destination.display()),
                    error,
                )
            })?;
        }
        remove_dir_if_exists(&wrapper, "removing the frontend wrapper directory")?;
        return Ok(());
    }
    Ok(())
}

/// Replaces the installed frontend with a freshly extracted directory.
#[cfg(windows)]
fn replace_frontend(plugin_dir: &Path, staging: &Path) -> Result<()> {
    match fs::symlink_metadata(plugin_dir) {
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(plugin_dir)
                .map_err(|error| Error::io(format!("removing {}", plugin_dir.display()), error))?;
        }
        Ok(_) => {
            fs::remove_file(plugin_dir)
                .map_err(|error| Error::io(format!("removing {}", plugin_dir.display()), error))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(Error::io(
                format!("inspecting {}", plugin_dir.display()),
                error,
            ));
        }
    }

    if let Some(parent) = plugin_dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
    }
    fs::rename(staging, plugin_dir)
        .map_err(|error| Error::io(format!("installing {}", plugin_dir.display()), error))
}

/// Paths of loaders shipped by older releases.
fn legacy_loader_paths(steam_root: &Path) -> Vec<PathBuf> {
    LEGACY_LOADERS
        .iter()
        .map(|name| steam_root.join(name))
        .collect()
}

/// Returns true when any legacy loader is still installed.
fn has_legacy_loader(steam_root: &Path) -> bool {
    legacy_loader_paths(steam_root)
        .iter()
        .any(|path| path.is_file())
}

/// Removes legacy loader slots, logging failures instead of aborting the install.
#[cfg(windows)]
fn remove_legacy_loaders(steam_root: &Path) {
    for path in legacy_loader_paths(steam_root) {
        match fs::remove_file(&path) {
            Ok(()) => {
                tracing::debug!(path = %path.display(), "removed a legacy plugin loader");
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "a legacy plugin loader could not be removed"
                );
            }
        }
    }
}

/// Removes a file if it exists.
fn remove_file_if_exists(path: &Path, context: &str) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(format!("{context}: {}", path.display()), error)),
    }
}

/// Removes a directory tree if it exists.
fn remove_dir_if_exists(path: &Path, context: &str) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(format!("{context}: {}", path.display()), error)),
    }
}

/// Rejects release asset URLs that are not HTTPS.
fn require_https(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "release asset URL is not https: {url}"
        )))
    }
}

/// Builds the error for a release that lacks a required asset.
fn missing_asset(release: &GithubRelease, name: &str) -> Error {
    Error::Other(format!(
        "release {} does not contain the required asset {name}",
        release.tag_name
    ))
}

/// Normalizes a release tag so `v1.2.3`, `V1.2.3`, and `1.2.3` compare equally.
fn normalized_tag(tag: &str) -> String {
    let trimmed = tag.trim();
    let without_prefix = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    without_prefix.to_ascii_lowercase()
}

/// Returns true when two release tags name the same version.
fn tags_match(installed: &str, available: &str) -> bool {
    normalized_tag(installed) == normalized_tag(available)
}

/// Returns true when `available` is a newer semantic version than `installed`.
fn release_is_newer(installed: &str, available: &str) -> bool {
    luatools_net::update::is_newer(installed, available)
}

/// Decides the status from local state and a release, without touching the network.
fn status_from_local(local: &LocalInstall, release: &GithubRelease) -> PluginStatus {
    if !(local.frontend && local.loader) {
        return PluginStatus::NotInstalled;
    }

    let latest_tag = release.tag_name.trim();
    let installed_tag = local
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.tag.as_deref())
        .map(str::trim)
        .filter(|tag| !tag.is_empty());

    let expected_loader_sha = release
        .asset(LOADER_ASSET)
        .and_then(|asset| asset.digest.as_deref())
        .and_then(parse_sha256_digest)
        .or_else(|| {
            local
                .manifest
                .as_ref()
                .and_then(PluginManifest::loader_sha)
                .map(str::to_owned)
        });
    let loader_matches = match (local.loader_sha.as_deref(), expected_loader_sha.as_deref()) {
        (Some(actual), Some(expected)) => actual.eq_ignore_ascii_case(expected),
        (Some(_), None) => true,
        (None, _) => false,
    };

    let tag_changed = installed_tag.is_none_or(|tag| !tags_match(tag, latest_tag));
    if let Some(installed) = installed_tag
        && tag_changed
        && !release_is_newer(installed, latest_tag)
    {
        tracing::debug!(
            installed = %installed,
            available = latest_tag,
            "the plugin release tag changed without a newer semantic version"
        );
    }

    if tag_changed || !loader_matches || local.legacy {
        PluginStatus::UpdateAvailable {
            installed: installed_tag.unwrap_or("unknown").to_owned(),
            available: latest_tag.to_owned(),
        }
    } else {
        PluginStatus::Installed {
            version: installed_tag.unwrap_or(latest_tag).to_owned(),
        }
    }
}

/// Reports the locally known state when GitHub cannot be reached.
///
/// An existing installation keeps its version; without one there is nothing to show but the
/// failed check, because the update state cannot be determined offline.
fn offline_status(local: &LocalInstall, reason: &str) -> PluginStatus {
    if local.frontend && local.loader {
        PluginStatus::Installed {
            version: installed_version(local),
        }
    } else {
        PluginStatus::Failed {
            reason: format!("could not check the latest plugin release: {reason}"),
        }
    }
}

/// Manifest tag, or a placeholder when the manifest predates tag recording.
fn installed_version(local: &LocalInstall) -> String {
    local
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.tag.as_deref())
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .unwrap_or("unknown")
        .to_owned()
}

/// Parses `releases/latest`, rejecting a payload without a tag.
fn parse_release(text: &str) -> Result<GithubRelease> {
    let release: GithubRelease = serde_json::from_str(text)
        .map_err(|error| Error::parse("github release", error.to_string()))?;
    if release.tag_name.trim().is_empty() {
        return Err(Error::parse("github release", "missing tag_name"));
    }
    Ok(release)
}

/// Fetches the latest release through the direct route and GitHub mirrors.
#[cfg(windows)]
fn fetch_release(http: &reqwest::Client) -> Result<GithubRelease> {
    let http = http.clone();
    block_on_background(async move {
        let text = luatools_net::github::fetch_text(&http, RELEASE_URL).await?;
        parse_release(&text)
    })
}

/// Downloads both release assets, returning their computed SHA-256 digests.
#[cfg(windows)]
fn download_assets(
    http: &reqwest::Client,
    zip_url: &str,
    loader_url: &str,
    zip_path: &Path,
    loader_path: &Path,
) -> Result<(String, String)> {
    let http = http.clone();
    let zip_url = zip_url.to_owned();
    let loader_url = loader_url.to_owned();
    let zip_path = zip_path.to_path_buf();
    let loader_path = loader_path.to_path_buf();
    block_on_background(async move {
        let zip = luatools_net::github::download(&http, &zip_url, &zip_path, |_, _| {}).await?;
        let loader =
            luatools_net::github::download(&http, &loader_url, &loader_path, |_, _| {}).await?;
        Ok((zip.as_str().to_owned(), loader.as_str().to_owned()))
    })
}

/// Runs a network future on a private runtime and thread.
///
/// The backend actor already runs inside a Tokio runtime. Blocking on a future directly there
/// can panic, so the installer gives its network work its own thread and runtime; callers on any
/// thread are safe. A panic in the worker is turned into an error instead of propagating.
#[cfg(windows)]
fn block_on_background<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::scope(|scope| {
        let handle = scope.spawn(move || -> Result<T> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    Error::Other(format!("starting the plugin installer runtime: {error}"))
                })?;
            runtime.block_on(future)
        });
        match handle.join() {
            Ok(result) => result,
            Err(_) => Err(Error::Other(
                "the plugin installer network worker panicked".to_owned(),
            )),
        }
    })
}

/// Returns true when the Millennium mod is installed in the Steam root.
#[cfg(windows)]
fn millennium_present(steam_root: &Path) -> bool {
    steam_root
        .join("millennium")
        .join("lib")
        .join("millennium.dll")
        .is_file()
}

/// Creates the CDP marker junction when it is missing or stale.
#[cfg(windows)]
fn ensure_cdp_marker(steam_root: &Path) {
    let path = steam_root.join(CDP_MARKER_NAME);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if is_reparse_point(&metadata) => return,
        Ok(metadata) => {
            let removed = if metadata.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(error) = removed {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "a stale CDP marker could not be cleared"
                );
                return;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "the CDP marker could not be inspected"
            );
            return;
        }
    }
    let command = format!("mklink /j \"{}\" \"{CDP_MARKER_TARGET}\"", path.display());
    run_cmd(&command, "creating the CDP marker junction");
}

/// Removes the CDP marker junction with `rmdir`, which severs the link without following it.
#[cfg(windows)]
fn remove_cdp_marker(steam_root: &Path) {
    let path = steam_root.join(CDP_MARKER_NAME);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {
            let command = format!("rmdir \"{}\"", path.display());
            run_cmd(&command, "removing the CDP marker junction");
        }
        Ok(_) => {
            if let Err(error) = fs::remove_file(&path) {
                // A junction can be reported without the directory bit on some Windows
                // versions; `rmdir` still severs the link without following its target.
                tracing::debug!(
                    path = %path.display(),
                    %error,
                    "the CDP marker was not a removable file; trying rmdir"
                );
                let command = format!("rmdir \"{}\"", path.display());
                run_cmd(&command, "removing the CDP marker");
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "the CDP marker could not be inspected"
            );
        }
    }
}

/// Returns true when a path carries the Windows reparse-point attribute.
#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Runs a `cmd.exe` command, logging but not propagating failures.
#[cfg(windows)]
fn run_cmd(command: &str, context: &str) {
    match std::process::Command::new("cmd")
        .args(["/c", command])
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(context = %context, %stderr, "a plugin installer command failed");
        }
        Err(error) => {
            tracing::warn!(
                context = %context,
                %error,
                "a plugin installer command could not be started"
            );
        }
    }
}

/// Returns the system `winmm.dll` used for the forwarding copy.
#[cfg(windows)]
fn system_loader_path() -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let path = root.join("System32").join(SYSTEM_LOADER);
    path.is_file().then_some(path)
}

/// Returns the `steam.exe` to invoke, preferring the discovered installation.
#[cfg(windows)]
fn steam_exe(steam_root: &Path) -> PathBuf {
    let candidate = steam_root.join("steam.exe");
    if candidate.is_file() {
        candidate
    } else {
        PathBuf::from("steam.exe")
    }
}

/// Returns true when `steam.exe` appears in the process list.
#[cfg(windows)]
fn steam_is_running() -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq steam.exe", "/NH"])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains("steam.exe"),
        Ok(_) => false,
        Err(error) => {
            tracing::warn!(%error, "the Steam process list could not be read");
            false
        }
    }
}

/// Asks Steam to exit.
#[cfg(windows)]
fn stop_steam(steam_root: &Path) {
    match std::process::Command::new(steam_exe(steam_root))
        .arg("-shutdown")
        .status()
    {
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "asking Steam to shut down failed");
        }
    }
}

/// Waits until `steam.exe` is no longer listed, up to `timeout`.
#[cfg(windows)]
fn wait_for_steam_exit(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !steam_is_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    !steam_is_running()
}

/// Starts Steam again after an operation that stopped it.
#[cfg(windows)]
fn start_steam(steam_root: &Path) {
    if let Err(error) = std::process::Command::new(steam_exe(steam_root)).spawn() {
        tracing::warn!(%error, "restarting Steam failed");
    }
}

/// A temporary directory removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a uniquely named directory under the system temporary directory.
    fn create(prefix: &str) -> Result<Self> {
        Self::create_in(&std::env::temp_dir(), prefix)
    }

    /// Creates a uniquely named directory under `parent`.
    fn create_in(parent: &Path, prefix: &str) -> Result<Self> {
        fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
        let path = parent.join(format!("luatools-{prefix}-{}", unique_suffix()));
        fs::create_dir_all(&path)
            .map_err(|error| Error::io(format!("creating {}", path.display()), error))?;
        Ok(Self { path })
    }

    /// Directory path.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::debug!(
                    path = %self.path.display(),
                    %error,
                    "a temporary plugin directory could not be removed"
                );
            }
        }
    }
}

/// Builds a process-unique suffix for temporary directories.
fn unique_suffix() -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}-{nanos}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn release(tag: &str, loader_digest: Option<&str>) -> GithubRelease {
        GithubRelease {
            tag_name: tag.to_owned(),
            assets: vec![
                GithubAsset {
                    name: FRONTEND_ASSET.to_owned(),
                    browser_download_url: "https://example.invalid/plugin.zip".to_owned(),
                    digest: None,
                },
                GithubAsset {
                    name: LOADER_ASSET.to_owned(),
                    browser_download_url: "https://example.invalid/winmm.dll".to_owned(),
                    digest: loader_digest.map(str::to_owned),
                },
            ],
        }
    }

    fn manifest(tag: &str, loader_sha: &str) -> PluginManifest {
        let mut dll_shas = BTreeMap::new();
        dll_shas.insert(LOADER_ASSET.to_owned(), loader_sha.to_owned());
        PluginManifest {
            tag: Some(tag.to_owned()),
            dll_shas,
            zip_sha: Some(digest('b')),
        }
    }

    fn installed(tag: &str, loader_sha: &str, legacy: bool) -> LocalInstall {
        LocalInstall {
            frontend: true,
            loader: true,
            legacy,
            manifest: Some(manifest(tag, loader_sha)),
            loader_sha: Some(loader_sha.to_owned()),
        }
    }

    #[test]
    fn plugin_paths_live_under_the_data_directory() {
        let paths = AppPaths::new("home");
        assert_eq!(
            frontend_dir(&paths),
            PathBuf::from("home").join("data").join("plugin")
        );
        assert_eq!(
            manifest_path(&paths),
            PathBuf::from("home")
                .join("data")
                .join("plugin")
                .join(MANIFEST_FILE)
        );
        assert_eq!(
            frontend_entry(&paths),
            PathBuf::from("home")
                .join("data")
                .join("plugin")
                .join("public")
                .join("luatools.js")
        );
    }

    #[test]
    fn archive_paths_inside_the_destination_are_accepted() {
        assert_eq!(
            safe_archive_path("public/luatools.js").expect("valid entry"),
            PathBuf::from("public").join("luatools.js")
        );
        assert_eq!(
            safe_archive_path("./public//luatools.js").expect("valid entry"),
            PathBuf::from("public").join("luatools.js")
        );
        assert_eq!(
            safe_archive_path("public/").expect("directory entry"),
            PathBuf::from("public")
        );
    }

    #[test]
    fn archive_entries_that_escape_are_rejected() {
        for name in [
            "",
            ".",
            "./",
            "..",
            "../evil.js",
            "public/../../evil.js",
            "/etc/passwd",
            "\\\\server\\share\\evil.js",
            "public\\luatools.js",
            "C:/Windows/win.ini",
            "C:\\Windows\\win.ini",
            "public/\0evil.js",
            "public/CON",
            "public/bad:name.js",
        ] {
            assert!(
                safe_archive_path(name).is_err(),
                "{name:?} must be rejected"
            );
        }
    }

    #[test]
    fn sha256_digests_are_normalized() {
        let upper = digest('A');
        assert_eq!(
            parse_sha256_digest(&format!("sha256:{upper}")),
            Some(digest('a'))
        );
        assert_eq!(
            parse_sha256_digest(&format!("  SHA256:{upper}  ")),
            Some(digest('a'))
        );
        assert_eq!(
            parse_sha256_digest(&digest('A')),
            Some(digest('a')),
            "a bare digest is accepted"
        );
    }

    #[test]
    fn malformed_digests_are_ignored() {
        for raw in [
            "",
            "sha256:",
            "sha256:abc",
            "md5:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "sha512:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(parse_sha256_digest(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn manifests_parse_both_casings() {
        let camel =
            parse_manifest(r#"{"tag":"v1.2.3","dllShas":{"winmm.dll":"abc"},"zipSha":"def"}"#)
                .expect("camelCase manifest");
        assert_eq!(camel.tag.as_deref(), Some("v1.2.3"));
        assert_eq!(camel.loader_sha(), Some("abc"));
        assert_eq!(camel.zip_sha.as_deref(), Some("def"));

        let pascal =
            parse_manifest(r#"{"Tag":"v1.2.3","DllShas":{"winmm.dll":"abc"},"ZipSha":"def"}"#)
                .expect("PascalCase manifest");
        assert_eq!(pascal, camel);
    }

    #[test]
    fn loader_hash_lookup_ignores_key_case() {
        let text = r#"{"tag":"v1","dllShas":{"WINMM.DLL":"abc"}}"#;
        let manifest = parse_manifest(text).expect("manifest");
        assert_eq!(manifest.loader_sha(), Some("abc"));
    }

    #[test]
    fn empty_manifests_use_defaults() {
        assert_eq!(
            parse_manifest("{}").expect("empty manifest"),
            PluginManifest::default()
        );
    }

    #[test]
    fn invalid_manifests_are_rejected() {
        assert!(parse_manifest("not json").is_err());
        assert!(parse_manifest(r#"{"tag": 7}"#).is_err());
    }

    #[test]
    fn release_digest_wins_over_the_manifest() {
        let release = release("v2.0.0", Some(&digest('a')));
        let manifest = manifest("v1.0.0", &digest('c'));
        assert_eq!(
            verification_digest(&release, Some(&manifest), LOADER_ASSET),
            Some(digest('a'))
        );
    }

    #[test]
    fn manifest_digest_is_a_fallback_for_the_same_tag() {
        let release = release("v1.0.0", None);
        let manifest = manifest("1.0.0", &digest('c'));
        assert_eq!(
            verification_digest(&release, Some(&manifest), LOADER_ASSET),
            Some(digest('c'))
        );
        assert_eq!(
            verification_digest(&release, Some(&manifest), FRONTEND_ASSET),
            Some(digest('b'))
        );
    }

    #[test]
    fn manifest_digest_is_ignored_for_a_different_tag() {
        let release = release("v2.0.0", None);
        let manifest = manifest("v1.0.0", &digest('c'));
        assert_eq!(
            verification_digest(&release, Some(&manifest), LOADER_ASSET),
            None
        );
    }

    #[test]
    fn malformed_release_digests_are_rejected() {
        let valid = release("v1.0.0", Some(&format!("sha256:{}", digest('a'))));
        assert!(validate_release_digest(&valid, LOADER_ASSET).is_ok());

        let malformed = release("v1.0.0", Some("sha256:abc"));
        assert!(validate_release_digest(&malformed, LOADER_ASSET).is_err());

        let missing = release("v1.0.0", None);
        assert!(validate_release_digest(&missing, LOADER_ASSET).is_ok());
    }

    #[test]
    fn mismatched_digests_are_rejected() {
        assert!(verify_digest("asset", &digest('a'), Some(&digest('a'))).is_ok());
        assert!(verify_digest("asset", &digest('a'), Some(&digest('b'))).is_err());
        assert!(verify_digest("asset", &digest('a'), None).is_ok());
    }

    #[test]
    fn tags_compare_case_and_prefix_insensitively() {
        assert!(tags_match("v1.2.3", "1.2.3"));
        assert!(tags_match("V1.2.3", "v1.2.3"));
        assert!(tags_match(" 1.2.3 ", "1.2.3"));
        assert!(!tags_match("1.2.3", "1.2.4"));
        assert!(!tags_match("1.2.3-beta", "1.2.3"));
    }

    #[test]
    fn semantic_version_comparison_uses_the_shared_rule() {
        assert!(release_is_newer("1.2.3", "1.10.0"));
        assert!(!release_is_newer("1.10.0", "1.2.3"));
        assert!(!release_is_newer("v1.2.3", "1.2.3"));
    }

    #[test]
    fn missing_local_parts_report_not_installed() {
        let release = release("v1.0.0", Some(&digest('a')));
        let mut local = installed("v1.0.0", &digest('a'), false);
        local.frontend = false;
        assert_eq!(
            status_from_local(&local, &release),
            PluginStatus::NotInstalled
        );
        local.frontend = true;
        local.loader = false;
        assert_eq!(
            status_from_local(&local, &release),
            PluginStatus::NotInstalled
        );
    }

    #[test]
    fn a_current_install_reports_its_version() {
        let loader_sha = digest('a');
        let release = release("v1.0.0", Some(&loader_sha));
        assert_eq!(
            status_from_local(&installed("1.0.0", &loader_sha, false), &release),
            PluginStatus::Installed {
                version: "1.0.0".to_owned()
            }
        );
    }

    #[test]
    fn a_new_release_reports_an_update() {
        let loader_sha = digest('a');
        let release = release("v2.0.0", Some(&loader_sha));
        assert_eq!(
            status_from_local(&installed("v1.0.0", &loader_sha, false), &release),
            PluginStatus::UpdateAvailable {
                installed: "v1.0.0".to_owned(),
                available: "v2.0.0".to_owned()
            }
        );
    }

    #[test]
    fn a_replaced_loader_reports_an_update() {
        let release = release("v1.0.0", Some(&digest('a')));
        assert_eq!(
            status_from_local(&installed("v1.0.0", &digest('c'), false), &release),
            PluginStatus::UpdateAvailable {
                installed: "v1.0.0".to_owned(),
                available: "v1.0.0".to_owned()
            }
        );
    }

    #[test]
    fn a_legacy_loader_reports_an_update() {
        let loader_sha = digest('a');
        let release = release("v1.0.0", Some(&loader_sha));
        assert_eq!(
            status_from_local(&installed("v1.0.0", &loader_sha, true), &release),
            PluginStatus::UpdateAvailable {
                installed: "v1.0.0".to_owned(),
                available: "v1.0.0".to_owned()
            }
        );
    }

    #[test]
    fn missing_manifest_tag_reports_an_update() {
        let loader_sha = digest('a');
        let release = release("v1.0.0", Some(&loader_sha));
        let mut local = installed("v1.0.0", &loader_sha, false);
        local.manifest = None;
        assert_eq!(
            status_from_local(&local, &release),
            PluginStatus::UpdateAvailable {
                installed: "unknown".to_owned(),
                available: "v1.0.0".to_owned()
            }
        );
    }

    #[test]
    fn offline_status_reports_the_local_install() {
        let local = installed("v1.0.0", &digest('a'), false);
        assert_eq!(
            offline_status(&local, "offline"),
            PluginStatus::Installed {
                version: "v1.0.0".to_owned()
            }
        );
        let mut missing = local;
        missing.loader = false;
        assert!(matches!(
            offline_status(&missing, "offline"),
            PluginStatus::Failed { reason } if reason.contains("offline")
        ));
    }

    #[test]
    fn assets_are_found_case_insensitively() {
        let release = release("v1.0.0", None);
        assert!(release.asset("PLUGIN.ZIP").is_some());
        assert!(release.asset("WINMM.DLL").is_some());
        assert!(release.asset("missing.dll").is_none());
    }

    #[test]
    fn release_json_is_parsed_and_validated() {
        let parsed = parse_release(
            r#"{"tag_name":"v1.4.0","assets":[{"name":"plugin.zip","browser_download_url":"https://example.invalid/plugin.zip","digest":"sha256:abcd"}]}"#,
        )
        .expect("valid release");
        assert_eq!(parsed.tag_name, "v1.4.0");
        assert_eq!(parsed.assets.len(), 1);
        assert!(parse_release(r#"{"tag_name":"  "}"#).is_err());
        assert!(parse_release("{}").is_err());
    }

    #[test]
    fn insecure_asset_urls_are_rejected() {
        assert!(require_https("https://github.com/o/r/plugin.zip").is_ok());
        assert!(require_https("http://github.com/o/r/plugin.zip").is_err());
        assert!(require_https("file:///plugin.zip").is_err());
    }

    #[test]
    fn a_wrapped_frontend_directory_is_hoisted() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path();
        let wrapper = root.join("luatools-frontend");
        fs::create_dir_all(wrapper.join("public")).expect("create wrapper");
        fs::write(wrapper.join("public").join("luatools.js"), b"// plugin")
            .expect("write entry point");

        normalize_frontend_layout(root).expect("normalize layout");

        assert!(root.join("public").join("luatools.js").is_file());
        assert!(!wrapper.exists());
    }

    #[test]
    fn an_already_flat_frontend_is_left_alone() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let root = temp.path();
        fs::create_dir_all(root.join("public")).expect("create public");
        fs::write(root.join("public").join("luatools.js"), b"// plugin")
            .expect("write entry point");

        normalize_frontend_layout(root).expect("normalize layout");

        assert!(root.join("public").join("luatools.js").is_file());
    }

    #[test]
    fn staging_directories_are_created_under_the_requested_parent() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let staging = TempDir::create_in(temp.path(), "staging").expect("staging directory");
        assert_eq!(staging.path().parent(), Some(temp.path()));
    }

    #[test]
    fn temporary_directories_are_removed() {
        let path = {
            let temp = TempDir::create("test").expect("temporary directory");
            let path = temp.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists());
    }
}
