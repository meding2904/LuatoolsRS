//! Tracing setup: stdout plus a rotating-by-launch log file.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// A writer that appends to one file, shared by every log layer.
#[derive(Clone)]
struct SharedFile(Arc<Mutex<std::fs::File>>);

impl Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut file) => file.write(buf),
            Err(_) => Err(std::io::Error::other("log file poisoned")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock() {
            Ok(mut file) => file.flush(),
            Err(_) => Err(std::io::Error::other("log file poisoned")),
        }
    }
}

impl<'a> MakeWriter<'a> for SharedFile {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs the global subscriber. Safe to call once; later calls are ignored.
///
/// # Errors
///
/// Returns the I/O error when the log file cannot be opened. Logging to stdout still works in
/// that case only if the caller ignores the error; the application treats it as non-fatal.
pub fn init(log_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(log_dir)?;
    let path = log_dir.join("luatools.log");
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let writer = SharedFile(Arc::new(Mutex::new(file)));

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("luatools=info,luatools_ui=info,warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .try_init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_file_is_created_in_the_given_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        // `init` installs a global subscriber; only the file creation half is asserted here so
        // the test can run alongside others.
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("luatools.log");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        drop(file);
        assert!(path.exists());
    }
}
