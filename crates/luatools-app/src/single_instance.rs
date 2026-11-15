//! Single-instance guard and protocol handoff.
//!
//! The first process takes an exclusive lock on `instance.lock` inside the data directory.
//! Later processes fail to acquire it, write their command-line URL to `pending-url.tmp`, and
//! exit so the running instance can pick it up on its next frame.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use luatools_core::error::{Error, Result};

/// An acquired single-instance lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Tries to take the lock, returning `None` when another instance holds it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the lock file cannot be created or inspected.
    pub fn acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                path: path.to_path_buf(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(Error::io(format!("locking {}", path.display()), error))
            }
        }
    }

    /// Path of the acquired lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Writes the URL a second instance wants the running instance to open.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be written.
pub fn write_pending(path: &Path, url: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
    }
    let temp = path.with_extension("tmp");
    let mut file =
        File::create(&temp).map_err(|e| Error::io(format!("creating {}", temp.display()), e))?;
    file.write_all(url.as_bytes())
        .map_err(|e| Error::io(format!("writing {}", temp.display()), e))?;
    std::fs::rename(&temp, path)
        .map_err(|e| Error::io(format!("renaming {}", path.display()), e))?;
    Ok(())
}

/// Reads and removes the pending URL, if any.
#[must_use]
pub fn take_pending(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    if std::fs::remove_file(path).is_err() {
        tracing::debug!(path = %path.display(), "pending url could not be removed");
    }
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// Parses the startup command line for a `luatools://` argument or `--minimized`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupArgs {
    /// Protocol URL supplied on the command line.
    pub url: Option<String>,
    /// Start hidden in the tray.
    pub minimized: bool,
}

/// Extracts the startup arguments.
#[must_use]
pub fn parse_startup_args<I, S>(args: I) -> StartupArgs
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = StartupArgs::default();
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--minimized" {
            out.minimized = true;
        } else if arg.starts_with("luatools://") {
            out.url = Some(arg.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_instance_does_not_get_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("instance.lock");
        let first = InstanceLock::acquire(&path).unwrap().expect("first lock");
        assert!(InstanceLock::acquire(&path).unwrap().is_none());
        drop(first);
        assert!(InstanceLock::acquire(&path).unwrap().is_some());
    }

    #[test]
    fn pending_url_round_trips_and_is_consumed_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending-url.tmp");
        write_pending(&path, "luatools://game/730").unwrap();
        assert_eq!(take_pending(&path).as_deref(), Some("luatools://game/730"));
        assert_eq!(take_pending(&path), None);
    }

    #[test]
    fn startup_args_are_extracted() {
        let args =
            parse_startup_args(["/usr/bin/luatools", "--minimized", "luatools://install/440"]);
        assert!(args.minimized);
        assert_eq!(args.url.as_deref(), Some("luatools://install/440"));
    }
}
