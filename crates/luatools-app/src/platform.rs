//! Platform shell integration: open paths, reveal files, start the Steam client.

use std::path::Path;
use std::process::Command;

use luatools_core::error::{Error, Result};

/// Registers the `luatools://` URL scheme with the operating system.
///
/// Best effort: a failure is logged by the caller and never blocks startup. Windows uses the
/// per-user registry, Linux writes a desktop entry and asks `xdg-mime` to make it the default.
pub fn register_protocol() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|error| Error::Other(format!("resolving executable: {error}")))?;
    #[cfg(windows)]
    {
        let key = "HKCU\\Software\\Classes\\luatools";
        let _ = Command::new("reg")
            .args(["add", key, "/ve", "/d", "URL:LuaTools Protocol", "/f"])
            .status();
        let _ = Command::new("reg")
            .args(["add", key, "/v", "URL Protocol", "/d", "", "/f"])
            .status();
        let command = format!("\"{}\" \"%1\"", exe.display());
        return Command::new("reg")
            .args([
                "add",
                &format!("{key}\\shell\\open\\command"),
                "/ve",
                "/d",
                &command,
                "/f",
            ])
            .status()
            .map(|_| ())
            .map_err(|error| Error::Other(format!("registering protocol: {error}")));
    }
    #[cfg(target_os = "linux")]
    {
        let base = directories::BaseDirs::new()
            .ok_or_else(|| Error::Other("no home directory".to_owned()))?;
        let applications = base.data_dir().join("applications");
        let _ = std::fs::create_dir_all(base.config_dir());
        std::fs::create_dir_all(&applications)
            .map_err(|error| Error::io(format!("creating {}", applications.display()), error))?;
        let entry = applications.join("luatools.desktop");
        let body = format!(
            "[Desktop Entry]\nType=Application\nName=LuaTools\nComment=Steam library companion\nExec=\"{}\" %u\nTerminal=false\nMimeType=x-scheme-handler/luatools;\n",
            exe.display()
        );
        std::fs::write(&entry, body)
            .map_err(|error| Error::io(format!("writing {}", entry.display()), error))?;
        let _ = Command::new("xdg-mime")
            .args(["default", "luatools.desktop", "x-scheme-handler/luatools"])
            .status();
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err(Error::Unsupported("protocol registration"))
}

/// Enables or disables starting LuaTools with the operating system.
///
/// Best effort, matching the original application: a failed registry or desktop-file write is
/// surfaced to the caller but never blocks startup.
pub fn set_start_with_os(enabled: bool) -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|error| Error::Other(format!("resolving executable: {error}")))?;
    #[cfg(windows)]
    {
        let key = "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run";
        if enabled {
            let command = format!("\"{}\" --minimized", exe.display());
            return Command::new("reg")
                .args(["add", key, "/v", "LuaTools", "/d", &command, "/f"])
                .status()
                .map(|_| ())
                .map_err(|error| Error::Other(format!("enabling autostart: {error}")));
        }
        return Command::new("reg")
            .args(["delete", key, "/v", "LuaTools", "/f"])
            .status()
            .map(|_| ())
            .map_err(|error| Error::Other(format!("disabling autostart: {error}")));
    }
    #[cfg(target_os = "linux")]
    {
        let base = directories::BaseDirs::new()
            .ok_or_else(|| Error::Other("no home directory".to_owned()))?;
        let autostart = base.config_dir().join("autostart");
        let entry = autostart.join("luatools.desktop");
        if !enabled {
            return match std::fs::remove_file(&entry) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(Error::io(format!("removing {}", entry.display()), error)),
            };
        }
        std::fs::create_dir_all(&autostart)
            .map_err(|error| Error::io(format!("creating {}", autostart.display()), error))?;
        let body = format!(
            "[Desktop Entry]\nType=Application\nName=LuaTools\nExec=\"{}\" --minimized\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        return std::fs::write(&entry, body)
            .map_err(|error| Error::io(format!("writing {}", entry.display()), error));
    }
    #[allow(unreachable_code)]
    Err(Error::Unsupported("start with the operating system"))
}

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

/// Returns whether Steam is currently running.
///
/// Best effort: a platform without a process probe reports `false`, which means an appinfo
/// write may fail rather than the application closing a client it cannot restart.
#[must_use]
pub fn steam_is_running() -> bool {
    #[cfg(target_os = "linux")]
    {
        return Command::new("pgrep")
            .args(["-x", "steam"])
            .status()
            .is_ok_and(|status| status.success());
    }
    #[cfg(target_os = "macos")]
    {
        return Command::new("pgrep")
            .args(["-x", "steam_osx"])
            .status()
            .is_ok_and(|status| status.success());
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq steam.exe", "/NH"])
            .output();
        return output.is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("steam.exe")
        });
    }
    #[allow(unreachable_code)]
    false
}

/// Asks Steam to shut down and waits briefly for it to exit.
///
/// Returns true when the process appears to be gone, or when it was not running.
pub fn stop_steam() -> bool {
    if !steam_is_running() {
        return true;
    }
    let steam = if cfg!(windows) { "steam.exe" } else { "steam" };
    let _ = Command::new(steam).arg("-shutdown").status();
    for _ in 0..20 {
        if !steam_is_running() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    !steam_is_running()
}

/// Starts Steam in the background.
///
/// # Errors
///
/// Returns [`Error::Other`] when the Steam binary cannot be started.
pub fn start_steam() -> Result<()> {
    let steam = if cfg!(windows) { "steam.exe" } else { "steam" };
    Command::new(steam)
        .spawn()
        .map(|_| ())
        .map_err(|error| Error::Other(format!("starting {steam}: {error}")))
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
