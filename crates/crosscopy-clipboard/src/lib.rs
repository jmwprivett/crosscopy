//! Native clipboard access and change notification.
//!
//! Reading and writing goes through `arboard`. Change detection is
//! platform-specific because no portable API exists:
//!
//! - Windows: `AddClipboardFormatListener` (push, no polling)
//! - macOS: poll `NSPasteboard.changeCount` (the OS offers no notification)
//! - Other: poll the text contents (temporary, until X11/Wayland backends land)

use std::sync::mpsc::Receiver;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(windows, target_os = "macos")))]
mod poll;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(windows, target_os = "macos")))]
use poll as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("clipboard error: {0}")]
    Clipboard(#[from] arboard::Error),
    #[error("os error: {0}")]
    Os(#[from] std::io::Error),
    #[error("clipboard watcher thread exited during startup")]
    WatcherDied,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Signal that the system clipboard changed. Read it to see what changed.
#[derive(Debug, Clone, Copy)]
pub struct ClipboardChanged;

/// Start watching the system clipboard on a background thread.
///
/// The thread stops once the returned receiver is dropped.
pub fn watch() -> Result<Receiver<ClipboardChanged>> {
    platform::watch()
}

/// Whether the current clipboard contents were flagged by their source app
/// as sensitive or transient (password managers, etc.) and must not be synced.
///
/// Fails closed: if the flags cannot be inspected, returns `true`.
pub fn is_excluded() -> bool {
    platform::is_excluded()
}

/// Thin wrapper over `arboard` that handles platform text conventions.
pub struct Clipboard(arboard::Clipboard);

impl Clipboard {
    pub fn new() -> Result<Self> {
        Ok(Self(arboard::Clipboard::new()?))
    }

    /// Returns `None` when the clipboard holds no text (e.g. only an image).
    pub fn read_text(&mut self) -> Result<Option<String>> {
        match self.0.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes LF-normalized text, converting to the platform's line endings.
    pub fn write_text(&mut self, text: &str) -> Result<()> {
        #[cfg(windows)]
        let text = text.replace('\n', "\r\n");
        self.0.set_text(text)?;
        Ok(())
    }
}
