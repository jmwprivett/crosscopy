//! The clipboard actor: a single thread that owns the system clipboard.
//!
//! Keeping all reads and writes on one thread, along with a fingerprint of
//! what is currently on the clipboard, is what prevents echo loops. When we
//! write a remote clip, the watcher fires, we read back the same contents,
//! and we don't send them out again.

use crate::files;
use anyhow::{Context, Result};
use crosscopy_clipboard::{Clipboard, Contents};
use crosscopy_core::{ClipFormat, ClipItem, ContentHash, DeviceId, MAX_MESSAGE_BYTES, format_bytes, mime};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Apps often write the clipboard several times in a burst (one call per
/// format), so wait for the burst to settle before reading.
const SETTLE: Duration = Duration::from_millis(50);
const READ_ATTEMPTS: u32 = 4;
const READ_RETRY_DELAY: Duration = Duration::from_millis(20);
/// Room left in a message for everything but the payloads.
const MESSAGE_OVERHEAD: usize = 64 * 1024;

/// A copy made on this device.
#[derive(Debug)]
pub struct LocalClip {
    pub item: ClipItem,
    /// For file copies: the local path of each file in the manifest, which
    /// peers will fetch.
    pub files: Vec<PathBuf>,
}

enum Event {
    LocalChanged,
    Apply { item: ClipItem, from: String },
    /// A file copy is downloading. Anything copied or received before it
    /// finishes supersedes it.
    AwaitFiles { item: Uuid },
    ApplyFiles { item: Uuid, from: String, paths: Vec<PathBuf> },
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

    pub fn await_files(&self, item: Uuid) {
        let _ = self.events.send(Event::AwaitFiles { item });
    }

    /// Puts downloaded files on the clipboard, unless something newer
    /// arrived since the matching [`await_files`](Self::await_files).
    pub fn apply_files(&self, item: Uuid, from: String, paths: Vec<PathBuf>) {
        let _ = self.events.send(Event::ApplyFiles { item, from, paths });
    }
}

/// Starts the clipboard thread. Local copies are delivered on `outbox`.
/// File copies over `max_file_bytes` are not sent.
pub fn spawn(device: DeviceId, max_file_bytes: u64, outbox: tokio_mpsc::Sender<LocalClip>) -> Result<ClipboardHandle> {
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
        Actor { device, max_file_bytes, clipboard, outbox, current: None, awaiting: None }.run(events_rx);
    })?;
    ready_rx
        .recv()
        .context("clipboard thread exited during startup")?
        .context("opening clipboard")?;

    Ok(ClipboardHandle { events })
}

struct Actor {
    device: DeviceId,
    max_file_bytes: u64,
    clipboard: Clipboard,
    outbox: tokio_mpsc::Sender<LocalClip>,
    /// Fingerprint of what is on the clipboard right now, as far as we know.
    current: Option<ContentHash>,
    /// The file download that may still replace the clipboard.
    awaiting: Option<Uuid>,
}

impl Actor {
    fn run(mut self, events: mpsc::Receiver<Event>) {
        // Whatever was already on the clipboard at startup is not a new copy.
        self.current = self.clipboard.read().ok().map(|c| fingerprint(&c));

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
                Some(event) => {
                    // A local copy that happened first must be read (and
                    // sent) before a remote clip overwrites it.
                    if pending.take().is_some() {
                        self.local_changed();
                    }
                    self.handle(event);
                }
                None => {
                    pending = None;
                    self.local_changed();
                }
            }
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::LocalChanged => self.local_changed(),
            Event::Apply { item, from } => {
                self.awaiting = None;
                self.apply(&item, &from);
            }
            Event::AwaitFiles { item } => self.awaiting = Some(item),
            Event::ApplyFiles { item, from, paths } => {
                if self.awaiting != Some(item) {
                    info!(from, "files arrived after something newer was copied; leaving the clipboard alone");
                    return;
                }
                self.awaiting = None;
                let count = paths.len();
                match self.clipboard.write(&Contents { files: paths, ..Contents::default() }) {
                    Ok(now) => {
                        self.current = Some(fingerprint(&now));
                        info!(from, items = count, "files ready to paste");
                    }
                    Err(e) => warn!(from, "failed to put received files on the clipboard: {e}"),
                }
            }
        }
    }

    /// Reads, retrying briefly when nothing is found. On Windows a read that
    /// races another process's clipboard access (another clipboard tool, or
    /// the app that is still writing) can transiently come back empty.
    fn read_settled(&mut self) -> crosscopy_clipboard::Result<Contents> {
        for attempt in 1..=READ_ATTEMPTS {
            match self.clipboard.read() {
                Ok(contents) if contents.is_empty() && attempt < READ_ATTEMPTS => {}
                Err(_) if attempt < READ_ATTEMPTS => {}
                result => return result,
            }
            debug!(attempt, "clipboard read found nothing; retrying");
            thread::sleep(READ_RETRY_DELAY);
        }
        unreachable!("the final attempt always returns")
    }

    fn local_changed(&mut self) {
        if crosscopy_clipboard::is_excluded() {
            info!("skipped copy: source app marked it as sensitive");
            self.current = None;
            self.awaiting = None;
            return;
        }
        let contents = match self.read_settled() {
            Ok(contents) => contents,
            Err(e) => {
                warn!("failed to read clipboard: {e}");
                return;
            }
        };
        let hash = fingerprint(&contents);
        if self.current == Some(hash) {
            debug!(hash = %hash.short(), "clipboard unchanged (echo or duplicate)");
            return;
        }
        self.current = Some(hash);
        // A real local copy beats any download still in flight.
        self.awaiting = None;
        if contents.is_empty() {
            debug!("clipboard changed to unsupported content");
            return;
        }
        if let Some(clip) = self.to_clip(contents)
            && self.outbox.blocking_send(clip).is_err()
        {
            debug!("outbox closed");
        }
    }

    fn to_clip(&self, contents: Contents) -> Option<LocalClip> {
        if !contents.files.is_empty() {
            if self.max_file_bytes == 0 {
                info!("copied files, but file sync is turned off (max_file_mb = 0)");
                return None;
            }
            return match files::scan(&contents.files, self.max_file_bytes) {
                Ok(local) => {
                    let format = ClipFormat { mime: mime::FILES.into(), data: local.manifest.encode() };
                    Some(LocalClip { item: ClipItem::new(self.device, vec![format]), files: local.sources })
                }
                Err(e) => {
                    info!("not syncing copied files: {e:#}");
                    None
                }
            };
        }
        let formats = fit_message(formats_of(contents));
        if formats.is_empty() {
            return None;
        }
        Some(LocalClip { item: ClipItem::new(self.device, formats), files: Vec::new() })
    }

    fn apply(&mut self, item: &ClipItem, from: &str) {
        let contents = contents_of(item);
        if contents.is_empty() {
            debug!(from, "remote clip has no supported format");
            return;
        }
        if self.current == Some(fingerprint(&contents)) {
            debug!(from, "remote clip already on clipboard");
            return;
        }
        match self.clipboard.write(&contents) {
            Ok(now) => {
                self.current = Some(fingerprint(&now));
                info!(from, what = item.describe(), size = format_bytes(item.size() as u64), "received clip");
            }
            Err(e) => warn!(from, "failed to write clipboard: {e}"),
        }
    }
}

/// Identifies clipboard contents, independent of how they were read.
fn fingerprint(contents: &Contents) -> ContentHash {
    let mut hasher = blake3::Hasher::new();
    let mut field = |tag: u8, data: Option<&[u8]>| {
        if let Some(data) = data {
            hasher.update(&[tag]);
            hasher.update(&(data.len() as u64).to_le_bytes());
            hasher.update(data);
        }
    };
    field(1, contents.text.as_deref().map(str::as_bytes));
    field(2, contents.html.as_deref().map(str::as_bytes));
    field(3, contents.rtf.as_deref());
    field(4, contents.png.as_deref());
    for path in &contents.files {
        field(5, Some(path.as_os_str().as_encoded_bytes()));
    }
    ContentHash(*hasher.finalize().as_bytes())
}

fn formats_of(contents: Contents) -> Vec<ClipFormat> {
    let format = |mime: &str, data: Vec<u8>| ClipFormat { mime: mime.to_owned(), data };
    let mut formats = Vec::new();
    if let Some(text) = contents.text.filter(|t| !t.is_empty()) {
        formats.push(format(mime::TEXT_PLAIN, text.into_bytes()));
    }
    if let Some(html) = contents.html {
        formats.push(format(mime::TEXT_HTML, html.into_bytes()));
    }
    if let Some(rtf) = contents.rtf {
        formats.push(format(mime::TEXT_RTF, rtf));
    }
    if let Some(png) = contents.png {
        formats.push(format(mime::IMAGE_PNG, png));
    }
    formats
}

/// Drops the bulkiest optional formats until the clip fits in one message.
fn fit_message(mut formats: Vec<ClipFormat>) -> Vec<ClipFormat> {
    let budget = MAX_MESSAGE_BYTES - MESSAGE_OVERHEAD;
    for droppable in [mime::IMAGE_PNG, mime::TEXT_RTF, mime::TEXT_HTML, mime::TEXT_PLAIN] {
        let size: usize = formats.iter().map(|f| f.data.len()).sum();
        if size <= budget {
            break;
        }
        if let Some(i) = formats.iter().position(|f| f.mime == droppable) {
            let dropped = formats.remove(i);
            warn!(
                format = droppable,
                size = format_bytes(dropped.data.len() as u64),
                "copied content is too large to sync; leaving this part out"
            );
        }
    }
    formats
}

fn contents_of(item: &ClipItem) -> Contents {
    let text = |mime: &str| item.format(mime).map(|data| String::from_utf8_lossy(data).into_owned());
    Contents {
        text: text(mime::TEXT_PLAIN).map(|t| crosscopy_core::normalize_newlines(&t)),
        html: text(mime::TEXT_HTML),
        rtf: item.format(mime::TEXT_RTF).map(<[u8]>::to_vec),
        png: item.format(mime::IMAGE_PNG).map(<[u8]>::to_vec),
        files: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents_survive_the_wire() {
        let contents = Contents {
            text: Some("a\nb".into()),
            html: Some("<b>a</b>".into()),
            rtf: Some(b"{\\rtf1 a}".to_vec()),
            png: Some(b"\x89PNG...".to_vec()),
            files: Vec::new(),
        };
        let item = ClipItem::new(DeviceId([1; 20]), formats_of(contents.clone()));
        assert_eq!(item.describe(), "text + html + rtf + image");
        assert_eq!(contents_of(&item), contents);
        assert_eq!(fingerprint(&contents_of(&item)), fingerprint(&contents));
    }

    #[test]
    fn fingerprint_tells_formats_apart() {
        let text = Contents::text("x");
        let html = Contents { html: Some("x".into()), ..Contents::default() };
        assert_ne!(fingerprint(&text), fingerprint(&html));
        assert_ne!(fingerprint(&Contents::default()), fingerprint(&text));
    }

    #[test]
    fn oversized_images_are_dropped_before_text() {
        let formats = vec![
            ClipFormat { mime: mime::TEXT_PLAIN.into(), data: b"caption".to_vec() },
            ClipFormat { mime: mime::IMAGE_PNG.into(), data: vec![0; MAX_MESSAGE_BYTES] },
        ];
        let kept = fit_message(formats);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].mime, mime::TEXT_PLAIN);
    }
}
