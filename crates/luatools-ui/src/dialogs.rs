//! Native folder picker, run off the UI thread.
//!
//! `rfd` drives the XDG desktop portal on Linux and the native dialogs on Windows/macOS. The
//! dialog blocks the calling thread until the user answers, so it runs on its own thread and
//! the page polls the returned receiver once per frame.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

/// Opens a folder picker and returns a receiver for the result.
///
/// The receiver yields `None` when the user cancels or the dialog cannot be shown.
#[must_use]
pub fn pick_folder(initial: Option<PathBuf>) -> Receiver<Option<PathBuf>> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("luatools-folder-picker".to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let result = runtime.ok().and_then(|runtime| {
                runtime.block_on(async move {
                    let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose a folder");
                    if let Some(initial) = initial {
                        dialog = dialog.set_directory(initial);
                    }
                    dialog
                        .pick_folder()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                })
            });
            let _ = tx.send(result);
        })
        .map_err(|error| tracing::warn!(%error, "could not start the folder picker"))
        .ok();
    rx
}

/// Polls a picker without blocking; consumes the receiver once a result is available.
#[must_use]
pub fn poll(receiver: &mut Option<Receiver<Option<PathBuf>>>) -> Option<Option<PathBuf>> {
    let rx = receiver.as_ref()?;
    match rx.try_recv() {
        Ok(result) => {
            *receiver = None;
            Some(result)
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => None,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            *receiver = None;
            Some(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn poll_reports_disconnection() {
        let (tx, rx) = channel();
        drop(tx);
        let mut receiver = Some(rx);
        assert_eq!(poll(&mut receiver), Some(None));
        assert!(receiver.is_none());
    }

    #[test]
    fn poll_is_non_blocking_while_empty() {
        let (_tx, rx) = channel::<Option<PathBuf>>();
        let mut receiver = Some(rx);
        assert_eq!(poll(&mut receiver), None);
        assert!(receiver.is_some());
        drop(receiver);
        // No dialog is opened by this test.
        std::thread::sleep(Duration::from_millis(1));
    }
}
