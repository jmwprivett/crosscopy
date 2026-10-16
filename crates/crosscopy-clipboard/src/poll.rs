//! Fallback backend for desktops without a native one (X11, GNOME's Wayland
//! session): polls the clipboard text and syncs text only.

use crate::{ClipboardChanged, Contents, Error, Result, normalize_newlines};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn watch() -> Result<Receiver<ClipboardChanged>> {
    let mut clipboard = arboard::Clipboard::new().map_err(other)?;
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

#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn is_excluded() -> bool {
    false
}

pub struct Backend(arboard::Clipboard);

impl Backend {
    pub fn new() -> Result<Self> {
        Ok(Self(arboard::Clipboard::new().map_err(other)?))
    }

    pub fn read(&mut self) -> Result<Contents> {
        match self.0.get_text() {
            Ok(text) => Ok(Contents { text: Some(normalize_newlines(text)), ..Contents::default() }),
            Err(arboard::Error::ContentNotAvailable) => Ok(Contents::default()),
            Err(e) => Err(other(e)),
        }
    }

    pub fn write(&mut self, contents: &Contents) -> Result<Contents> {
        let Some(text) = contents.text.clone() else {
            return Err(Error::Clipboard("only text can be synced on this desktop".into()));
        };
        self.0.set_text(text.clone()).map_err(other)?;
        Ok(Contents { text: Some(text), ..Contents::default() })
    }
}

fn other(e: arboard::Error) -> Error {
    Error::Clipboard(e.to_string())
}
