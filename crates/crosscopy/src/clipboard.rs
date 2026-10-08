//! The clipboard actor: a single thread that owns the system clipboard.
//!
//! Keeping all reads and writes on one thread, along with the hash of what
//! is currently on the clipboard, is what prevents echo loops. When we write
//! a remote clip, the watcher fires, we read back the same hash, and we
//! don't send it out again.

use anyhow::{Context, Result};
use crosscopy_clipboard::Clipboard;
use crosscopy_core::{ClipItem, ContentHash, DeviceId};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, info, warn};

/// Apps often write the clipboard several times in a burst (one call per
/// format), so wait for the burst to settle before reading.
const SETTLE: Duration = Duration::from_millis(50);
const READ_ATTEMPTS: u32 = 4;
const READ_RETRY_DELAY: Duration = Duration::from_millis(20);

enum Event {
    LocalChanged,
    Apply { item: ClipItem, from: String },
}

/// Handle for sending remote clips to the clipboard thread.
#[derive(Clone)]
pub struct ClipboardHandle {
    events: mpsc::Sender<Event>,
}

impl ClipboardHandle {
    pub fn apply(&self, item: ClipItem, from: String) {
        let _ = self.events.send(Event::Apply { item, from });
    }
}

/// Starts the clipboard thread. Local copies are delivered on `outbox`.
pub fn spawn(device: DeviceId, outbox: tokio_mpsc::Sender<ClipItem>) -> Result<ClipboardHandle> {
    let (events, events_rx) = mpsc::channel();

    let changes = crosscopy_clipboard::watch().context("starting clipboard watcher")?;
    let forward = events.clone();
    thread::Builder::new().name("clipboard-forward".into()).spawn(move || {
        for _ in changes {
            if forward.send(Event::LocalChanged).is_err() {
                break;
            }
        }
    })?;

    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    thread::Builder::new().name("clipboard-actor".into()).spawn(move || {
        let clipboard = match Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let _ = ready_tx.send(Ok(()));
        Actor { device, clipboard, outbox, current: None }.run(events_rx);
    })?;
    ready_rx
        .recv()
        .context("clipboard thread exited during startup")?
        .context("opening clipboard")?;

    Ok(ClipboardHandle { events })
}

struct Actor {
    device: DeviceId,
    clipboard: Clipboard,
    outbox: tokio_mpsc::Sender<ClipItem>,
    /// Hash of what is on the clipboard right now, as far as we know.
    current: Option<ContentHash>,
}

impl Actor {
    fn run(mut self, events: mpsc::Receiver<Event>) {
        // Whatever was already on the clipboard at startup is not a new copy.
        self.current = self.read_item().ok().flatten().map(|item| item.hash);

        let mut pending: Option<Instant> = None;
        loop {
            let event = match pending {
                None => match events.recv() {
                    Ok(event) => Some(event),
                    Err(_) => return,
                },
                Some(since) => match events.recv_timeout(SETTLE.saturating_sub(since.elapsed())) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            match event {
                Some(Event::LocalChanged) => {
                    pending.get_or_insert_with(Instant::now);
                }
                Some(Event::Apply { item, from }) => {
                    // A local copy that happened first must be read (and
                    // sent) before the remote clip overwrites it.
                    if pending.take().is_some() {
                        self.local_changed();
                    }
                    self.apply(item, &from);
                }
                None => {
                    pending = None;
                    self.local_changed();
                }
            }
        }
    }

    fn read_item(&mut self) -> Result<Option<ClipItem>> {
        Ok(self
            .clipboard
            .read_text()?
            .filter(|text| !text.is_empty())
            .map(|text| ClipItem::from_text(self.device, &text)))
    }

    /// Reads, retrying briefly when no text is found. On Windows a read that
    /// races another process's clipboard access (another clipboard tool, or
    /// the app that is still writing) can transiently report no text.
    fn read_item_settled(&mut self) -> Result<Option<ClipItem>> {
        for attempt in 1..=READ_ATTEMPTS {
            match self.read_item() {
                Ok(None) | Err(_) if attempt < READ_ATTEMPTS => {
                    debug!(attempt, "clipboard read found no text; retrying");
                    thread::sleep(READ_RETRY_DELAY);
                }
                result => return result,
            }
        }
        unreachable!("the final attempt always returns")
    }

    fn local_changed(&mut self) {
        if crosscopy_clipboard::is_excluded() {
            info!("skipped copy: source app marked it as sensitive");
            self.current = None;
            return;
        }
        match self.read_item_settled() {
            Ok(Some(item)) if self.current == Some(item.hash) => {
                debug!(hash = %item.hash.short(), "clipboard unchanged (echo or duplicate)");
            }
            Ok(Some(item)) => {
                self.current = Some(item.hash);
                if self.outbox.blocking_send(item).is_err() {
                    debug!("outbox closed");
                }
            }
            Ok(None) => {
                debug!("clipboard changed to unsupported content");
                self.current = None;
            }
            Err(e) => warn!("failed to read clipboard: {e}"),
        }
    }

    fn apply(&mut self, item: ClipItem, from: &str) {
        if self.current == Some(item.hash) {
            debug!(from, "remote clip already on clipboard");
            return;
        }
        let Some(text) = item.text() else {
            debug!(from, "remote clip has no supported format");
            return;
        };
        match self.clipboard.write_text(text) {
            Ok(()) => {
                self.current = Some(item.hash);
                info!(from, bytes = text.len(), hash = %item.hash.short(), "received clip");
            }
            Err(e) => warn!(from, "failed to write clipboard: {e}"),
        }
    }
}
