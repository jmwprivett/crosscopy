//! The sync daemon loop, shared by the CLI (`crosscopy run`) and the tray app.

use crate::clipboard;
use crate::config::{Config, Paths};
use anyhow::Result;
use crosscopy_core::Message;
use crosscopy_net::{Identity, Node, NodeOptions};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

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

    let (local_tx, mut local_rx) = mpsc::channel(16);
    let clipboard = clipboard::spawn(identity.id, local_tx)?;
    let options = NodeOptions {
        listen: SocketAddr::from(([0, 0, 0, 0], config.port)),
        device_name: config.device_name(),
        discovery: true,
    };
    let (node, mut inbox) = Node::start(&identity, options, peers)?;
    info!(name = %config.device_name(), code = %identity.id, port = config.port, "crosscopy running");

    let mut paused = false;
    let mut config_stamp = modified(&paths.config_file);
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            Some(item) = local_rx.recv() => {
                let bytes = item.text().map_or(0, str::len);
                if paused {
                    debug!(bytes, "paused; not sending copy");
                    continue;
                }
                match node.broadcast(&Message::Clip(item)) {
                    0 => info!(bytes, "copied, but no peers are connected"),
                    n => info!(bytes, peers = n, "sent clip"),
                }
            }
            Some(incoming) = inbox.recv() => match incoming.message {
                Message::Hello => {}
                Message::Clip(_) if paused => debug!(from = %incoming.from_name, "paused; ignoring clip"),
                Message::Clip(item) => clipboard.apply(item, incoming.from_name),
            },
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
                };
            }
        }
    }
    status.lock().unwrap().running = false;
    node.shutdown().await;
    Ok(())
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
