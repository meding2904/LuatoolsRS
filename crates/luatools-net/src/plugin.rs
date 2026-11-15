//! Steam store-plugin installation.
//!
//! The Windows installer lives in the app crate. This module defines the interface the rest of
//! the application consumes and an explicit unsupported implementation for platforms where the
//! loader DLL and Steam client layout do not apply.

use luatools_core::{Error, PluginStatus, Result};

/// Release asset carrying the plugin frontend archive.
pub const FRONTEND_ASSET: &str = "plugin.zip";

/// Release asset carrying the Windows loader DLL.
pub const LOADER_ASSET: &str = "winmm.dll";

/// Maximum length of one on-disk asset name, in bytes.
pub const MAX_ASSET_NAME_LEN: usize = 255;

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Installs, updates, and removes the Steam store plugin.
pub trait PluginInstaller: Send + Sync {
    /// Reads the current installation state.
    ///
    /// # Errors
    ///
    /// Returns an error when the local state cannot be inspected.
    fn status(&self) -> Result<PluginStatus>;

    /// Installs or updates the plugin. `progress` receives values in `0.0..=1.0`.
    ///
    /// # Errors
    ///
    /// Returns an error when the release cannot be fetched, verified, or installed.
    fn install(&self, progress: &mut dyn FnMut(f32)) -> Result<()>;

    /// Removes the plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when installed files cannot be removed.
    fn uninstall(&self) -> Result<()>;
}

/// An installer for platforms where the Steam store plugin is not available.
///
/// Every operation returns [`Error::Unsupported`]; the interface reports the plugin as absent
/// rather than pretending an install succeeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnsupportedPluginInstaller;

impl PluginInstaller for UnsupportedPluginInstaller {
    fn status(&self) -> Result<PluginStatus> {
        Err(unsupported())
    }

    fn install(&self, _progress: &mut dyn FnMut(f32)) -> Result<()> {
        Err(unsupported())
    }

    fn uninstall(&self) -> Result<()> {
        Err(unsupported())
    }
}

fn unsupported() -> Error {
    Error::Unsupported("Steam store plugin installation")
}

/// Validates a release asset name before it is used as a file name.
///
/// Rejects path separators, parent references, absolute paths, control characters, Windows
/// reserved device names, characters Windows forbids in file names, and trailing dots or spaces.
/// A valid name is returned unchanged. This is the name-level guard; extraction of an archive
/// still has to reject entries that escape the destination directory.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] describing the first rule the name violates.
pub fn safe_asset_name(name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Error::InvalidInput("asset name is empty".to_owned()));
    }
    if name.len() > MAX_ASSET_NAME_LEN {
        return Err(Error::InvalidInput(format!(
            "asset name is longer than {MAX_ASSET_NAME_LEN} bytes"
        )));
    }
    if name == "." || name == ".." {
        return Err(Error::InvalidInput(
            "asset name is a path component".to_owned(),
        ));
    }
    if name.contains(['/', '\\']) {
        return Err(Error::InvalidInput(
            "asset name contains a path separator".to_owned(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::InvalidInput(
            "asset name contains a control character".to_owned(),
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err(Error::InvalidInput(
            "asset name has a trailing dot or surrounding space".to_owned(),
        ));
    }
    if name.contains(['<', '>', ':', '"', '|', '?', '*']) {
        return Err(Error::InvalidInput(
            "asset name contains a character Windows forbids".to_owned(),
        ));
    }

    let stem = name.split('.').next().unwrap_or(name);
    if WINDOWS_RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
    {
        return Err(Error::InvalidInput(format!(
            "asset name uses the reserved Windows name {stem}"
        )));
    }

    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_installer_reports_every_operation() {
        let installer = UnsupportedPluginInstaller;
        assert!(matches!(
            installer.status(),
            Err(Error::Unsupported("Steam store plugin installation"))
        ));
        let mut progress = |_| {};
        assert!(matches!(
            installer.install(&mut progress),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(installer.uninstall(), Err(Error::Unsupported(_))));
    }

    #[test]
    fn ordinary_asset_names_are_accepted() {
        for name in [
            "plugin.zip",
            "winmm.dll",
            "LuaTools-1.4.0.zip",
            "Steam Setup.exe",
            "archive.tar.gz",
        ] {
            assert_eq!(safe_asset_name(name).expect("valid asset name"), name);
        }
    }

    #[test]
    fn traversal_and_separators_are_rejected() {
        for name in [
            "",
            "..",
            ".",
            "../plugin.zip",
            "folder/plugin.zip",
            "folder\\plugin.zip",
            "/absolute/plugin.zip",
            "C:plugin.zip",
        ] {
            assert!(safe_asset_name(name).is_err(), "{name:?} must be rejected");
        }
    }

    #[test]
    fn windows_reserved_names_are_rejected() {
        for name in [
            "CON",
            "con",
            "NUL",
            "nul.txt",
            "AUX",
            "PRN",
            "COM1",
            "com9.dll",
            "LPT1",
            "lpt9.tar.gz",
        ] {
            assert!(safe_asset_name(name).is_err(), "{name:?} must be rejected");
        }
        // Reserved words used inside a longer name are fine.
        for name in ["console.dll", "COM10.zip", "LPT10.zip", "null.txt"] {
            assert!(safe_asset_name(name).is_ok(), "{name:?} must be accepted");
        }
    }

    #[test]
    fn windows_illegal_characters_and_trailing_marks_are_rejected() {
        for name in [
            "bad:name.zip",
            "bad?name.zip",
            "bad*name.zip",
            "bad\"name.zip",
            "bad<name>.zip",
            "bad|name.zip",
            "trailing.",
            "trailing ",
            " leading.zip",
            "nul\u{0}.zip",
        ] {
            assert!(safe_asset_name(name).is_err(), "{name:?} must be rejected");
        }
    }

    #[test]
    fn overlong_names_are_rejected() {
        let name = "a".repeat(MAX_ASSET_NAME_LEN + 1);
        assert!(safe_asset_name(&name).is_err());
    }
}
