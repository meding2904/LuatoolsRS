//! `appmanifest_<appid>.acf` parsing.
//!
//! An ACF file is text VDF whose root object is `AppState`. The fields retained here are
//! the ones the UI and library scanner need: identity, install folder, size, build, update
//! time, and Steam's state flags.

use std::path::{Path, PathBuf};

use luatools_core::{AppId, Error, Result};

use crate::vdf;

/// Steam's "fully installed" bit in `StateFlags`.
pub const STATE_FULLY_INSTALLED: u32 = 4;

/// The metadata from one `appmanifest_<appid>.acf` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifest {
    /// Steam app id from the `appid` field.
    pub app_id: AppId,
    /// Display name. Falls back to the decimal app id when the file omits `name`.
    pub name: String,
    /// Folder name under `steamapps/common`.
    pub install_dir: String,
    /// Installed size in bytes from `SizeOnDisk`.
    pub size_on_disk: u64,
    /// Current build id, when the file has one.
    pub build_id: Option<String>,
    /// Last update as a Unix timestamp, when present.
    pub last_updated: Option<i64>,
    /// Raw Steam `StateFlags` bit mask.
    pub state_flags: u32,
    /// True when [`STATE_FULLY_INSTALLED`] is set in `state_flags`.
    pub installed: bool,
}

impl AppManifest {
    /// Returns the full install folder for this app inside `library_root`.
    #[must_use]
    pub fn install_path(&self, library_root: &Path) -> PathBuf {
        library_root
            .join("steamapps")
            .join("common")
            .join(&self.install_dir)
    }

    /// Returns true when Steam's fully-installed bit is set.
    #[must_use]
    pub const fn is_installed(&self) -> bool {
        self.installed
    }
}

/// Reads and parses one ACF file.
///
/// # Errors
///
/// Returns an I/O error when the file cannot be read and a parse error when it is not a
/// valid app manifest.
pub fn read_app_manifest(path: &Path) -> Result<AppManifest> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| Error::io(format!("reading app manifest {}", path.display()), error))?;
    parse_app_manifest(&text)
        .map_err(|error| error.context(format!("reading app manifest {}", path.display())))
}

/// Parses ACF text that has already been read.
///
/// The root object may be named `AppState` (the normal shape) or be flat; a flat file with
/// an `appid` field is accepted for robustness.
///
/// # Errors
///
/// Returns a parse error when the text is not VDF, the `appid` or `installdir` field is
/// missing, or a numeric field is out of range.
pub fn parse_app_manifest(text: &str) -> Result<AppManifest> {
    let root = vdf::parse(text)?;
    let state = root.table("AppState").unwrap_or(&root);

    let raw_app_id = state
        .as_u64("appid")
        .ok_or_else(|| Error::parse("acf", "missing or non-numeric appid field"))?;
    let app_id = u32::try_from(raw_app_id)
        .map_err(|_| Error::parse("acf", format!("appid {raw_app_id} does not fit in u32")))?;
    let app_id = AppId::new(app_id);

    let install_dir = state
        .as_str("installdir")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::parse("acf", "missing installdir field"))?
        .to_owned();

    let name = state
        .as_str("name")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| app_id.to_string(), ToOwned::to_owned);

    let size_on_disk = state.as_u64("SizeOnDisk").unwrap_or(0);
    let build_id = state
        .as_str("buildid")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let last_updated = state.as_i64("LastUpdated");
    let state_flags = match state.as_u64("StateFlags") {
        Some(value) => u32::try_from(value)
            .map_err(|_| Error::parse("acf", format!("StateFlags {value} does not fit in u32")))?,
        None => 0,
    };

    Ok(AppManifest {
        app_id,
        name,
        install_dir,
        size_on_disk,
        build_id,
        last_updated,
        state_flags,
        installed: state_flags & STATE_FULLY_INSTALLED != 0,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const ACF: &str = r#"
"AppState"
{
    "appid"        "730"
    "name"         "Counter-Strike 2"
    "installdir"   "Counter-Strike Global Offensive"
    "StateFlags"   "6"
    "SizeOnDisk"   "32212254720"
    "buildid"      "17800000"
    "LastUpdated"  "1700000000"
    "UserConfig"
    {
        "language" "english"
    }
}
"#;

    #[test]
    fn parses_all_retained_acf_fields() {
        let manifest = parse_app_manifest(ACF).unwrap();
        assert_eq!(manifest.app_id, AppId::new(730));
        assert_eq!(manifest.name, "Counter-Strike 2");
        assert_eq!(manifest.install_dir, "Counter-Strike Global Offensive");
        assert_eq!(manifest.size_on_disk, 32_212_254_720);
        assert_eq!(manifest.build_id.as_deref(), Some("17800000"));
        assert_eq!(manifest.last_updated, Some(1_700_000_000));
        assert_eq!(manifest.state_flags, 6);
        assert!(manifest.installed);
        assert!(manifest.is_installed());
        assert_eq!(
            manifest.install_path(Path::new("/steam")),
            PathBuf::from("/steam/steamapps/common/Counter-Strike Global Offensive")
        );
    }

    #[test]
    fn state_flags_without_the_installed_bit_are_not_installed() {
        let manifest = parse_app_manifest(
            r#""AppState" { "appid" "10" "installdir" "Ten" "StateFlags" "2" }"#,
        )
        .unwrap();
        assert_eq!(manifest.state_flags, 2);
        assert!(!manifest.installed);
        // Missing name falls back to the app id; missing numbers become zero/none.
        assert_eq!(manifest.name, "10");
        assert_eq!(manifest.size_on_disk, 0);
        assert_eq!(manifest.build_id, None);
        assert_eq!(manifest.last_updated, None);
    }

    #[test]
    fn rejects_manifests_without_identity_or_install_dir() {
        let error = parse_app_manifest(r#""AppState" { "installdir" "x" }"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("appid"), "{error}");

        let error = parse_app_manifest(r#""AppState" { "appid" "1" }"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("installdir"), "{error}");
    }

    #[test]
    fn read_app_manifest_adds_the_path_to_parse_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appmanifest_1.acf");
        std::fs::write(&path, "\"AppState\" { \"appid\" \"bad\" }").unwrap();
        let error = read_app_manifest(&path).unwrap_err().to_string();
        assert!(error.contains("appmanifest_1.acf"), "{error}");
    }
}
