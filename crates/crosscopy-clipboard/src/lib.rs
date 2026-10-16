//! Native clipboard access and change notification.
//!
//! Every backend reads and writes the same set of representations
//! ([`Contents`]) using the OS's own clipboard API, so a single copy can carry
//! plain text, HTML, RTF, an image and files at once. Change detection is
//! platform-specific because no portable API exists:
//!
//! - Windows: `AddClipboardFormatListener` (push, no polling)
//! - macOS: poll `NSPasteboard.changeCount` (the OS offers no notification)
//! - Linux on wlroots Wayland (Hyprland, Sway): `wlr-data-control` (push)
//! - Anything else: poll the text contents (text only)

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

#[cfg(windows)]
mod dib;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(windows, target_os = "macos")))]
mod poll;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
use poll as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("clipboard error: {0}")]
    Clipboard(String),
    #[error("os error: {0}")]
    Os(#[from] std::io::Error),
    #[error("the clipboard is in use by another app")]
    Busy,
    #[error("clipboard watcher thread exited during startup")]
    WatcherDied,
}

pub type Result<T> = std::result::Result<T, Error>;

/// What's on the clipboard, in every representation crosscopy syncs.
///
/// Text is always LF-normalized here; backends convert to and from the
/// platform's line endings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contents {
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    pub png: Option<Vec<u8>>,
    /// Copied files and folders. When set, backends write only these.
    pub files: Vec<PathBuf>,
}

impl Contents {
    pub fn text(text: &str) -> Self {
        Self { text: Some(text.to_owned()), ..Self::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.rtf.is_none() && self.png.is_none() && self.files.is_empty()
    }
}

/// Canonical text form is LF; each platform converts on write.
pub(crate) fn normalize_newlines(text: String) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n") } else { text }
}

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

pub struct Clipboard(platform::Backend);

impl Clipboard {
    pub fn new() -> Result<Self> {
        Ok(Self(platform::Backend::new()?))
    }

    /// Reads every supported representation. Formats that fail to read are
    /// left out rather than failing the whole read.
    pub fn read(&mut self) -> Result<Contents> {
        self.0.read()
    }

    /// Replaces the clipboard with `contents`.
    ///
    /// Returns what [`read`](Self::read) will now report, which can differ
    /// from `contents` when the OS converts or adds representations.
    pub fn write(&mut self, contents: &Contents) -> Result<Contents> {
        self.0.write(contents)
    }

    pub fn write_text(&mut self, text: &str) -> Result<()> {
        self.write(&Contents::text(text)).map(drop)
    }
}
