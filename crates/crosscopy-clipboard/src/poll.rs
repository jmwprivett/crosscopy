//! Fallback backend: poll the clipboard text. Placeholder for Linux until
//! dedicated X11 (XFixes) and Wayland (wlr-data-control) backends exist.

use crate::{ClipboardChanged, Result};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn watch() -> Result<Receiver<ClipboardChanged>> {
    let mut clipboard = arboard::Clipboard::new()?;
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("clipboard-watcher".into())
        .spawn(move || {
            let mut last = clipboard.get_text().ok();
            loop {
                thread::sleep(POLL_INTERVAL);
                let now = clipboard.get_text().ok();
                if now != last {
                    last = now;
                    if tx.send(ClipboardChanged).is_err() {
                        break;
                    }
                }
            }
        })?;
    Ok(rx)
}

pub fn is_excluded() -> bool {
    // TODO: honor KDE's `x-kde-passwordManagerHint` once on a native backend.
    false
}
