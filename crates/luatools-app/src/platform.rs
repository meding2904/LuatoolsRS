//! Platform shell integration: open paths, reveal files, start the Steam client.

use std::path::Path;
use std::process::Command;

use luatools_core::error::{Error, Result};

/// Opens a file or directory with the desktop default handler.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] on platforms without a known opener, or [`Error::Other`] when
/// the opener process cannot be started.
pub fn open_path(path: &Path) -> Result<()> {
    let program = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    spawn(program, &[path.as_os_str()])
}

/// Reveals a path in the file manager, selecting it where the platform supports that.
///
/// # Errors
///
/// Same conditions as [`open_path`].
pub fn reveal_path(path: &Path) -> Result<()> {
    if cfg!(target_os = "windows") {
        return spawn(
            "explorer",
            &[std::ffi::OsStr::new("/select,"), path.as_os_str()],
        );
    }
    let target = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    open_path(target)
}

/// Starts a Steam app through the `steam://` protocol.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when the platform has no protocol handler configured.
pub fn launch_steam_app(app_id: u32) -> Result<()> {
    let url = format!("steam://rungameid/{app_id}");
    if cfg!(target_os = "windows") {
        return spawn(
            "cmd",
            &[
                std::ffi::OsStr::new("/C"),
                std::ffi::OsStr::new("start"),
                std::ffi::OsStr::new(&url),
            ],
        );
    }
    open_path(Path::new(&url))
}

/// Asks Steam to install or update an app.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when the platform has no protocol handler.
pub fn install_steam_app(app_id: u32) -> Result<()> {
    open_steam_url(&format!("steam://install/{app_id}"))
}

/// Opens an app's Steam store page.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when the platform has no protocol handler.
pub fn open_store_page(app_id: u32) -> Result<()> {
    open_steam_url(&format!("steam://store/{app_id}"))
}

/// Closes Steam and relaunches it.
///
/// Launch options written to `localconfig.vdf` are only read when the client starts, so the
/// user must restart Steam for them to take effect.
///
/// # Errors
///
/// Returns [`Error::Other`] when the Steam binary cannot be found or started.
pub fn restart_steam() -> Result<()> {
    let steam = if cfg!(target_os = "windows") {
        "steam.exe"
    } else {
        "steam"
    };
    // Best effort shutdown; Steam may not be running.
    let _ = Command::new(steam).arg("-shutdown").status();
    Command::new(steam)
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::Other(format!("starting {steam}: {e}")))
}

fn open_steam_url(url: &str) -> Result<()> {
    if cfg!(target_os = "windows") {
        return spawn(
            "cmd",
            &[
                std::ffi::OsStr::new("/C"),
                std::ffi::OsStr::new("start"),
                std::ffi::OsStr::new(url),
            ],
        );
    }
    open_path(Path::new(url))
}

/// Opens a URL in the default browser.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for non-http(s) URLs and [`Error::Other`] when the opener
/// cannot be started.
pub fn open_url(url: &str) -> Result<()> {
    validate_url(url)?;
    open_path(Path::new(url))
}

/// Rejects anything that is not plain http(s).
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for other schemes.
fn validate_url(url: &str) -> Result<()> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!("refusing to open {url}")))
    }
}

fn spawn(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    Command::new(program)
        .args(args.iter().copied())
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::Other(format!("starting {program}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_http_urls_are_rejected() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("luatools://game/730").is_err());
    }

    #[test]
    fn localhost_urls_pass_validation() {
        assert!(validate_url("http://127.0.0.1:6767/health").is_ok());
        assert!(validate_url("https://store.steampowered.com").is_ok());
    }
}
