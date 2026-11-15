//! Steam installation discovery and library scanning.
//!
//! [`find_steam_root`] resolves the Steam installation from an explicit override, the
//! Windows registry, environment variables, or the common per-platform install locations.
//! [`discover_libraries`] reads `libraryfolders.vdf`, scans each library's
//! `appmanifest_*.acf` files, and returns the installed apps.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use luatools_core::{Error, Result};
use tracing::warn;

use crate::acf::{AppManifest, read_app_manifest};
use crate::vdf::{self, VdfValue};

/// One Steam library root and the apps installed inside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SteamLibrary {
    /// Library root (the folder containing `steamapps`).
    pub root: PathBuf,
    /// Apps whose `appmanifest_*.acf` parsed successfully, sorted by app id.
    pub apps: Vec<AppManifest>,
}

/// Resolves the Steam installation root.
///
/// An explicit override is returned without checking that it contains Steam, matching the
/// C# `EffectivePath` behaviour (`IsValid` is a separate concern). Otherwise the Windows
/// registry is consulted first, followed by environment variables and the common install
/// folders for Windows, Linux, and macOS.
///
/// On Windows the registry is read through `reg.exe` (64-bit view) so the crate needs no
/// unsafe code or platform binding.
#[must_use]
pub fn find_steam_root(override_path: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = override_path {
        let trimmed = path.to_string_lossy();
        let trimmed = trimmed.trim();
        if !trimmed.is_empty() {
            let mut path = PathBuf::from(trimmed);
            if path.is_relative()
                && let Ok(current) = std::env::current_dir()
            {
                path = current.join(path);
            }
            return Some(path);
        }
    }

    #[cfg(windows)]
    {
        for candidate in registry_steam_paths() {
            if candidate.join("steam.exe").is_file() {
                return Some(candidate);
            }
        }
    }

    let mut seen = HashSet::new();
    for candidate in candidate_roots() {
        if candidate.as_os_str().is_empty() || !seen.insert(path_key(&candidate)) {
            continue;
        }
        if looks_like_steam_root(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Discovers every Steam library and scans its app manifests.
///
/// The main Steam root is always the first library. Additional roots come from
/// `steamapps/libraryfolders.vdf`; nonexistent, duplicate, and main-root entries are
/// skipped. A missing `libraryfolders.vdf` yields just the main root. Individual unreadable
/// or malformed `appmanifest_*.acf` files are skipped with a warning.
///
/// # Errors
///
/// Returns an error when `libraryfolders.vdf` exists but cannot be read or parsed, or when
/// a library's `steamapps` directory exists but cannot be listed.
pub fn discover_libraries(root: &Path) -> Result<Vec<SteamLibrary>> {
    let mut libraries = Vec::new();
    let mut seen = HashSet::new();
    if seen.insert(path_key(root)) {
        libraries.push(SteamLibrary {
            root: root.to_path_buf(),
            apps: Vec::new(),
        });
    }

    let path = root.join("steamapps").join("libraryfolders.vdf");
    if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|error| {
            Error::io(
                format!("reading Steam library list {}", path.display()),
                error,
            )
        })?;
        let document = vdf::parse(&text).map_err(|error| {
            error.context(format!("reading Steam library list {}", path.display()))
        })?;
        let container = document.table("libraryfolders").unwrap_or(&document);

        for (_, value) in container.entries() {
            let Some(raw) = library_path(value) else {
                continue;
            };
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }

            let candidate = PathBuf::from(raw);
            if !candidate.is_dir() {
                continue;
            }
            if !seen.insert(path_key(&candidate)) {
                continue;
            }
            libraries.push(SteamLibrary {
                root: candidate,
                apps: Vec::new(),
            });
        }
    }

    for library in &mut libraries {
        library.apps = scan_apps(&library.root)?;
    }
    Ok(libraries)
}

/// Recursively totals the size of a file or directory using saturating arithmetic.
///
/// Symlinks are not followed and unreadable entries contribute zero.
#[must_use]
pub fn installed_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];

    while let Some(current) = stack.pop() {
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.is_dir() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                stack.push(entry.path());
            }
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

fn library_path(value: &VdfValue) -> Option<&str> {
    match value {
        VdfValue::String(path) => Some(path),
        VdfValue::Object(object) => object.as_str("path"),
    }
}

fn scan_apps(root: &Path) -> Result<Vec<AppManifest>> {
    let steamapps = root.join("steamapps");
    let entries = match std::fs::read_dir(&steamapps) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Error::io(
                format!("listing Steam apps in {}", steamapps.display()),
                error,
            ));
        }
    };

    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("appmanifest_") && name.ends_with(".acf") {
            paths.push(entry.path());
        }
    }
    paths.sort();

    let mut apps = Vec::with_capacity(paths.len());
    for path in paths {
        match read_app_manifest(&path) {
            Ok(manifest) => apps.push(manifest),
            Err(error) => {
                warn!(
                    path = %path.display(),
                    %error,
                    "skipping unreadable Steam app manifest"
                );
            }
        }
    }
    apps.sort_by_key(|manifest| manifest.app_id);
    Ok(apps)
}

fn looks_like_steam_root(path: &Path) -> bool {
    path.is_dir()
        && (path.join("steam.exe").is_file()
            || path.join("steam.sh").is_file()
            || path.join("steamapps").is_dir())
}

fn path_key(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy();
    #[cfg(windows)]
    {
        text.replace('/', "\\").to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        text.into_owned()
    }
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    for variable in ["STEAM_ROOT", "STEAM_PATH"] {
        if let Some(value) = std::env::var_os(variable)
            && !value.is_empty()
        {
            candidates.push(PathBuf::from(value));
        }
    }

    #[cfg(windows)]
    {
        for variable in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
            if let Some(value) = std::env::var_os(variable) {
                candidates.push(PathBuf::from(value).join("Steam"));
            }
        }
        candidates.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        candidates.push(PathBuf::from(r"C:\Program Files\Steam"));
    }

    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        candidates.push(home.join(".steam").join("steam"));
        candidates.push(home.join(".steam").join("root"));
        candidates.push(home.join(".local").join("share").join("Steam"));
        candidates.push(
            home.join(".var")
                .join("app")
                .join("com.valvesoftware.Steam")
                .join("data")
                .join("Steam"),
        );
        candidates.push(
            home.join("snap")
                .join("steam")
                .join("common")
                .join(".local")
                .join("share")
                .join("Steam"),
        );
        candidates.push(
            home.join("Library")
                .join("Application Support")
                .join("Steam"),
        );
    }

    candidates
}

#[cfg(windows)]
fn registry_steam_paths() -> Vec<PathBuf> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const LOCATIONS: [(&str, &str); 3] = [
        (r"HKCU\SOFTWARE\Valve\Steam", "SteamPath"),
        (r"HKLM\SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"),
        (r"HKLM\SOFTWARE\Valve\Steam", "InstallPath"),
    ];

    let mut paths = Vec::new();
    for (key, value) in LOCATIONS {
        let Ok(output) = std::process::Command::new("reg")
            .args(["query", key, "/v", value, "/reg:64"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }

        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let mut columns = line.split_whitespace();
            let Some(name) = columns.next() else {
                continue;
            };
            if !name.eq_ignore_ascii_case(value) {
                continue;
            }
            let Some(kind) = columns.next() else {
                continue;
            };
            if !kind.eq_ignore_ascii_case("REG_SZ") && !kind.eq_ignore_ascii_case("REG_EXPAND_SZ") {
                continue;
            }
            let raw = line
                .split_once(kind)
                .map(|(_, rest)| rest.trim())
                .unwrap_or_default();
            if raw.is_empty() {
                continue;
            }

            let mut path = PathBuf::from(raw.replace('/', "\\"));
            if path.is_relative()
                && let Ok(current) = std::env::current_dir()
            {
                path = current.join(path);
            }
            paths.push(path);
        }
    }
    paths
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use luatools_core::AppId;

    fn escape(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    fn write_manifest(root: &Path, app_id: u32, name: &str, install_dir: &str) {
        let steamapps = root.join("steamapps");
        std::fs::create_dir_all(&steamapps).unwrap();
        let body = format!(
            r#""AppState" {{ "appid" "{}" "name" "{}" "installdir" "{}" "StateFlags" "4" "SizeOnDisk" "1000" }}"#,
            app_id, name, install_dir
        );
        std::fs::write(steamapps.join(format!("appmanifest_{app_id}.acf")), body).unwrap();
    }

    #[test]
    fn discovers_libraries_and_deduplicates_roots() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("Steam");
        let extra = dir.path().join("SteamLibrary");
        let missing = dir.path().join("Missing");
        std::fs::create_dir_all(main.join("steamapps")).unwrap();
        std::fs::create_dir_all(extra.join("steamapps")).unwrap();

        write_manifest(&main, 730, "Counter-Strike 2", "cs2");
        write_manifest(&extra, 440, "Team Fortress 2", "tf2");

        let vdf = format!(
            r#""libraryfolders"
{{
    "0" {{ "path" "{}" }}
    "1" {{ "path" "{}" }}
    "2" {{ "path" "{}" }}
    "3" {{ "path" "{}" }}
    "4" {{ "path" "{}" }}
    "5" {{ "path" "{}" }}
}}"#,
            escape(&main.to_string_lossy()),
            escape(&extra.to_string_lossy()),
            escape(&extra.to_string_lossy()),
            escape(&main.to_string_lossy()),
            escape(&missing.to_string_lossy()),
            escape(&extra.to_string_lossy()),
        );
        std::fs::write(main.join("steamapps").join("libraryfolders.vdf"), vdf).unwrap();

        let libraries = discover_libraries(&main).unwrap();
        assert_eq!(libraries.len(), 2);
        assert_eq!(libraries[0].root, main);
        assert_eq!(libraries[0].apps.len(), 1);
        assert_eq!(libraries[0].apps[0].app_id, AppId::new(730));
        assert_eq!(libraries[1].root, extra);
        assert_eq!(libraries[1].apps.len(), 1);
        assert_eq!(libraries[1].apps[0].app_id, AppId::new(440));
        assert_eq!(libraries[1].apps[0].install_dir, "tf2");
    }

    #[test]
    fn missing_library_file_still_returns_the_main_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Steam");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        let libraries = discover_libraries(&root).unwrap();
        assert_eq!(libraries.len(), 1);
        assert_eq!(libraries[0].root, root);
        assert!(libraries[0].apps.is_empty());
    }

    #[test]
    fn malformed_library_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Steam");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        std::fs::write(
            root.join("steamapps").join("libraryfolders.vdf"),
            "\"libraryfolders\" {",
        )
        .unwrap();
        let error = discover_libraries(&root).unwrap_err().to_string();
        assert!(error.contains("libraryfolders.vdf"), "{error}");
    }

    #[test]
    fn installed_size_totals_nested_files_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("root.bin"), vec![0u8; 11]).unwrap();
        std::fs::write(dir.path().join("a/one.bin"), vec![0u8; 13]).unwrap();
        std::fs::write(dir.path().join("a/b/two.bin"), vec![0u8; 17]).unwrap();

        assert_eq!(installed_size(dir.path()), 41);
        assert_eq!(installed_size(&dir.path().join("root.bin")), 11);
        assert_eq!(installed_size(&dir.path().join("absent")), 0);
    }

    #[test]
    fn override_path_is_returned_without_a_steam_check() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = dir.path().join("NotSteam");
        assert_eq!(find_steam_root(Some(&chosen)), Some(chosen));
    }
}
