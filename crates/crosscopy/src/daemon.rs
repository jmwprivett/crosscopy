//! The sync daemon loop, shared by the CLI (`crosscopy run`) and the tray app.

use crate::clipboard::{self, ClipboardHandle, LocalClip};
use crate::config::{Config, Paths};
use crate::files::Inbox;
use anyhow::Result;
use crosscopy_core::{ClipItem, DeviceId, FileManifest, Message, format_bytes};
use crosscopy_net::{Identity, Node, NodeOptions};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// How often the daemon checks the config file and refreshes its status.
const TICK: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum Control {
    SetPaused(bool),
    /// Re-read the config now instead of waiting for the next poll.
    ReloadConfig,
    Shutdown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub running: bool,
    pub device_name: String,
    pub paired: Vec<String>,
    pub connected: Vec<String>,
    pub paused: bool,
    /// A file download in progress, e.g. "Receiving photos (12 MB) from mac · 40%".
    pub transfer: Option<String>,
}

pub type SharedStatus = Arc<Mutex<Status>>;

/// Runs until [`Control::Shutdown`] or until `control` is closed.
pub async fn run(paths: &Paths, mut control: mpsc::UnboundedReceiver<Control>, status: SharedStatus) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let peers = config.peers()?;
    if peers.is_empty() {
        warn!("no devices paired yet; run `crosscopy pair` here and on the other device");
    }
    let mut paired: Vec<String> = config.peers.iter().map(|p| p.name.clone()).collect();

    let max_file_bytes = config.max_file_bytes();
    let (local_tx, mut local_rx) = mpsc::channel(16);
    let clipboard = clipboard::spawn(identity.id, max_file_bytes, local_tx)?;
    let options = NodeOptions {
        listen: SocketAddr::from(([0, 0, 0, 0], config.port)),
        device_name: config.device_name(),
        discovery: true,
    };
    let (node, mut inbox) = Node::start(&identity, options, peers)?;
    info!(name = %config.device_name(), code = %identity.id, port = config.port, "crosscopy running");

    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let mut downloads = Downloads {
        node: node.clone(),
        clipboard: clipboard.clone(),
        inbox: Inbox::new(paths.received_dir.clone()),
        limit: max_file_bytes,
        current: None,
        done: done_tx,
    };
    let mut paused = false;
    let mut config_stamp = modified(&paths.config_file);
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            Some(LocalClip { item, files }) = local_rx.recv() => {
                // A newer local copy makes any download in flight pointless.
                downloads.cancel();
                let (what, size) = (item.describe(), format_bytes(item.size() as u64));
                if paused {
                    debug!(what, "paused; not sending copy");
                    continue;
                }
                if !files.is_empty() {
                    node.offer_files(item.id, files);
                }
                match node.broadcast(&Message::Clip(item)) {
                    0 => info!(what, size, "copied, but no peers are connected"),
                    n => info!(what, size, peers = n, "sent clip"),
                }
            }
            Some(incoming) = inbox.recv() => match incoming.message {
                Message::Hello => {}
                Message::Clip(_) if paused => debug!(from = %incoming.from_name, "paused; ignoring clip"),
                Message::Clip(item) => {
                    downloads.cancel();
                    match item.files() {
                        Some(manifest) => downloads.start(incoming.from, incoming.from_name, &item, manifest),
                        None => clipboard.apply(item, incoming.from_name),
                    }
                }
            },
            Some(finished) = done_rx.recv() => downloads.finish(finished),
            command = control.recv() => match command {
                Some(Control::SetPaused(p)) => {
                    paused = p;
                    info!(paused, "sync {}", if p { "paused" } else { "resumed" });
                }
                Some(Control::ReloadConfig) => {
                    config_stamp = modified(&paths.config_file);
                    if let Some(names) = reload_peers(&paths.config_file, &node, config.port) {
                        paired = names;
                    }
                }
                Some(Control::Shutdown) | None => {
                    info!("shutting down");
                    break;
                }
            },
            _ = tick.tick() => {
                let stamp = modified(&paths.config_file);
                if stamp != config_stamp {
                    config_stamp = stamp;
                    if let Some(names) = reload_peers(&paths.config_file, &node, config.port) {
                        paired = names;
                    }
                }
                let mut connected = node.connected_peers();
                connected.sort();
                *status.lock().unwrap() = Status {
                    running: true,
                    device_name: config.device_name(),
                    paired: paired.clone(),
                    connected,
                    paused,
                    transfer: downloads.status(),
                };
            }
        }
    }
    status.lock().unwrap().running = false;
    node.shutdown().await;
    Ok(())
}

/// Downloads of files copied on other devices. At most one runs at a time,
/// since only the latest copy can end up on the clipboard.
struct Downloads {
    node: Node,
    clipboard: ClipboardHandle,
    inbox: Inbox,
    limit: u64,
    current: Option<Download>,
    done: mpsc::UnboundedSender<Finished>,
}

struct Download {
    item: Uuid,
    from: String,
    what: String,
    total: u64,
    received: Arc<AtomicU64>,
    dest: PathBuf,
    task: AbortHandle,
}

struct Finished {
    item: Uuid,
    manifest: FileManifest,
    result: Result<()>,
}

impl Downloads {
    fn start(&mut self, peer: DeviceId, from: String, item: &ClipItem, manifest: FileManifest) {
        let (what, total) = (manifest.describe(), manifest.total_bytes());
        if self.limit == 0 {
            info!(from, what, "not receiving files: file sync is turned off (max_file_mb = 0)");
            return;
        }
        if total > self.limit {
            info!(from, what, limit = format_bytes(self.limit), "not receiving files: over the size limit (max_file_mb)");
            return;
        }
        let dest = match self.inbox.prepare(item.id) {
            Ok(dest) => dest,
            Err(e) => {
                warn!("cannot receive files: {e:#}");
                return;
            }
        };
        info!(from, what, "receiving files");
        self.clipboard.await_files(item.id);

        let received = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn({
            let (node, done, received, dest, item, limit) =
                (self.node.clone(), self.done.clone(), received.clone(), dest.clone(), item.id, self.limit);
            async move {
                let result = node.fetch_files(peer, item, &manifest, &dest, limit, &received).await;
                let _ = done.send(Finished { item, manifest, result });
            }
        });
        self.current = Some(Download { item: item.id, from, what, total, received, dest, task: task.abort_handle() });
    }

    fn finish(&mut self, finished: Finished) {
        let Some(download) = self.current.take_if(|d| d.item == finished.item) else {
            return;
        };
        match finished.result {
            Ok(()) => {
                let paths = self.inbox.completed(&download.dest, &finished.manifest);
                info!(from = download.from, what = download.what, "received files");
                self.clipboard.apply_files(download.item, download.from, paths);
            }
            Err(e) => {
                warn!(from = download.from, what = download.what, "receiving files failed: {e:#}");
                self.inbox.discard(&download.dest);
            }
        }
    }

    fn cancel(&mut self) {
        if let Some(download) = self.current.take() {
            download.task.abort();
            info!(from = download.from, what = download.what, "stopped receiving files: something newer was copied");
            self.inbox.discard(&download.dest);
        }
    }

    fn status(&self) -> Option<String> {
        let d = self.current.as_ref()?;
        let percent = (d.received.load(Ordering::Relaxed) * 100).checked_div(d.total).unwrap_or(100).min(100);
        Some(format!("Receiving {} from {} · {percent}%", d.what, d.from))
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Applies the config's peer list; returns the new peer names on success.
fn reload_peers(path: &Path, node: &Node, running_port: u16) -> Option<Vec<String>> {
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(e) => {
            warn!("ignoring config change: {e:#}");
            return None;
        }
    };
    if config.port != running_port {
        warn!("port changes take effect after restarting crosscopy");
    }
    match config.peers() {
        Ok(peers) => {
            info!(peers = peers.len(), "config changed; updating paired devices");
            node.set_peers(peers);
            Some(config.peers.iter().map(|p| p.name.clone()).collect())
        }
        Err(e) => {
            warn!("ignoring config change: {e:#}");
            None
        }
    }
}
