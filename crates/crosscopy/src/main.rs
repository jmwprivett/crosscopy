mod clipboard;
mod config;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Paths, PeerConfig};
use crosscopy_core::{DeviceId, Message};
use crosscopy_net::pairing::{Pairing, PairingEvent, PendingPair};
use crosscopy_net::{Identity, Node, NodeOptions};
use std::collections::HashSet;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

/// How often the daemon checks the config file for changes.
const CONFIG_POLL: Duration = Duration::from_secs(2);
const PAIR_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Use this directory for config and identity instead of the default.
    #[arg(long, global = true, env = "CROSSCOPY_CONFIG_DIR")]
    config_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the sync daemon.
    Run,
    /// Pair with another device. Run this on both devices at the same time.
    Pair {
        /// Only pair with the device of this name.
        name: Option<String>,
    },
    /// Show this device's name, code and addresses.
    Id,
    /// Manage paired devices.
    #[command(subcommand)]
    Peer(PeerCommand),
    /// Print every local clipboard change (no networking).
    Watch,
    /// Put text on the local clipboard.
    Set { text: String },
}

#[derive(Subcommand)]
enum PeerCommand {
    /// Pair manually by code: `crosscopy peer add mac mac.local ABCD-EFGH-...`
    Add {
        name: String,
        /// Fallback hostname or IP (optionally with `:port`), used when
        /// discovery can't find the device.
        address: String,
        /// The code printed by `crosscopy id` on that device.
        code: String,
    },
    /// List paired devices.
    List,
    /// Unpair a device.
    Remove { name: String },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn,rustls=warn,mdns_sd=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let paths = match cli.config_dir {
        Some(dir) => Paths { config_file: dir.join("config.toml"), identity_dir: dir.join("identity") },
        None => Paths::new()?,
    };

    match cli.command {
        Command::Run => run(&paths),
        Command::Pair { name } => pair(&paths, name),
        Command::Id => show_id(&paths),
        Command::Peer(cmd) => peer(&paths, cmd),
        Command::Watch => watch(&paths),
        Command::Set { text } => {
            crosscopy_clipboard::Clipboard::new()?.write_text(&text)?;
            Ok(())
        }
    }
}

fn run(paths: &Paths) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let peers = config.peers()?;
    if peers.is_empty() {
        warn!("no devices paired yet; run `crosscopy pair` here and on the other device");
    }

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let (local_tx, mut local_rx) = tokio::sync::mpsc::channel(16);
        let clipboard = clipboard::spawn(identity.id, local_tx)?;
        let options = NodeOptions {
            listen: SocketAddr::from(([0, 0, 0, 0], config.port)),
            device_name: config.device_name(),
            discovery: true,
        };
        let (node, mut inbox) = Node::start(&identity, options, peers)?;
        info!(name = %config.device_name(), code = %identity.id, port = config.port, "crosscopy running");

        let mut config_stamp = modified(&paths.config_file);
        let mut config_poll = tokio::time::interval(CONFIG_POLL);
        loop {
            tokio::select! {
                Some(item) = local_rx.recv() => {
                    let bytes = item.text().map_or(0, str::len);
                    match node.broadcast(&Message::Clip(item)) {
                        0 => info!(bytes, "copied, but no peers are connected"),
                        n => info!(bytes, peers = n, "sent clip"),
                    }
                }
                Some(incoming) = inbox.recv() => match incoming.message {
                    Message::Hello => {}
                    Message::Clip(item) => clipboard.apply(item, incoming.from_name),
                },
                _ = config_poll.tick() => {
                    let stamp = modified(&paths.config_file);
                    if stamp != config_stamp {
                        config_stamp = stamp;
                        reload_peers(&paths.config_file, &node, config.port);
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("shutting down");
                    break;
                }
            }
        }
        node.shutdown().await;
        Ok(())
    })
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn reload_peers(path: &Path, node: &Node, running_port: u16) {
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(e) => return warn!("ignoring config change: {e:#}"),
    };
    match config.peers() {
        Ok(peers) => {
            info!(peers = peers.len(), "config changed; updating paired devices");
            node.set_peers(peers);
        }
        Err(e) => warn!("ignoring config change: {e:#}"),
    }
    if config.port != running_port {
        warn!("port changes take effect after restarting crosscopy");
    }
}

fn pair(paths: &Paths, only: Option<String>) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let name = config.device_name();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let mut pairing = Pairing::start(&identity, &name)?;
        println!("Pairing as \"{name}\". Run `crosscopy pair` on the other device too.");
        println!("Searching the local network (up to {} s, Ctrl+C to cancel)...", PAIR_TIMEOUT.as_secs());

        let wanted = |device: &str| only.as_ref().is_none_or(|n| n.eq_ignore_ascii_case(device));
        // mDNS re-announces per interface; only mention each device once.
        let mut seen = HashSet::new();
        let deadline = tokio::time::sleep(PAIR_TIMEOUT);
        tokio::pin!(deadline);
        let pending: PendingPair = loop {
            tokio::select! {
                event = pairing.next() => match event {
                    Some(PairingEvent::Found(candidate)) if wanted(&candidate.name) => {
                        if seen.insert(candidate.id) {
                            println!("Found \"{}\".", candidate.name);
                        }
                        // Exactly one side starts the session, so both
                        // devices end up confirming the same one.
                        if identity.id < candidate.id {
                            match pairing.connect(&candidate).await {
                                Ok(pending) => break pending,
                                Err(e) => println!("Couldn't reach \"{}\": {e:#}", candidate.name),
                            }
                        }
                    }
                    Some(PairingEvent::Found(candidate)) => {
                        if seen.insert(candidate.id) {
                            println!("Ignoring \"{}\".", candidate.name);
                        }
                    }
                    Some(PairingEvent::Incoming(pending)) if wanted(&pending.peer_name) => break pending,
                    Some(PairingEvent::Incoming(pending)) => {
                        tokio::spawn(pending.finish(false));
                    }
                    Some(PairingEvent::Lost(_)) => {}
                    None => bail!("pairing stopped unexpectedly"),
                },
                _ = &mut deadline => bail!(
                    "no device found. Is `crosscopy pair` running on the other device, on the same network?"
                ),
                _ = tokio::signal::ctrl_c() => bail!("cancelled"),
            }
        };

        println!();
        println!("  Pairing with \"{}\"", pending.peer_name);
        println!("  Code:  {}", pending.code);
        println!();
        let confirmed = ask("Is the same code shown on the other device? [y/N] ").await?;
        let (peer_id, peer_name, peer_ip) = (pending.peer_id, pending.peer_name.clone(), pending.peer_addr.ip());
        if !confirmed {
            let _ = pending.finish(false).await;
            println!("Pairing cancelled.");
            return Ok(());
        }
        println!("Waiting for the other device to confirm...");
        if !pending.finish(true).await? {
            println!("The other device declined. Nothing was saved.");
            return Ok(());
        }

        // Reload in case the config changed while we were waiting.
        let mut config = Config::load(&paths.config_file)?;
        let saved_as = config.upsert_peer(peer_id, &peer_name, Some(peer_ip.to_string()));
        config.save(&paths.config_file)?;
        println!("Paired with \"{saved_as}\". A running `crosscopy run` picks this up automatically.");
        Ok(())
    })
}

/// Reads a yes/no answer from stdin without blocking the runtime.
async fn ask(prompt: &str) -> Result<bool> {
    let prompt = prompt.to_owned();
    tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
    })
    .await?
}

fn show_id(paths: &Paths) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let addresses = crosscopy_net::local_addresses();
    let shown: Vec<String> = addresses.iter().map(ToString::to_string).collect();

    println!("Name:        {}", config.device_name());
    println!("Device code: {}", identity.id);
    println!("UDP port:    {}", config.port);
    println!("Addresses:   {}", if shown.is_empty() { "(none found)".to_owned() } else { shown.join(", ") });
    println!("Config:      {}", paths.config_file.display());
    println!();
    println!("To pair, run `crosscopy pair` on both devices.");
    Ok(())
}

fn peer(paths: &Paths, cmd: PeerCommand) -> Result<()> {
    let mut config = Config::load(&paths.config_file)?;
    match cmd {
        PeerCommand::Add { name, address, code } => {
            let id: DeviceId = code.parse()?;
            let own = Identity::load_or_create(&paths.identity_dir)?;
            if id == own.id {
                bail!("that is this device's own code; use the code from the other device");
            }
            if config.peers.iter().any(|p| p.name == name) {
                bail!("a peer named {name:?} already exists; remove it first");
            }
            if let Some(existing) = config.peers.iter().find(|p| p.device_id().ok() == Some(id)) {
                bail!("that device is already paired as {:?}", existing.name);
            }
            config.peers.push(PeerConfig { name: name.clone(), address: Some(address), code: id.to_string() });
            config.save(&paths.config_file)?;
            println!("Paired with {name}.");
        }
        PeerCommand::List => {
            if config.peers.is_empty() {
                println!("No devices paired.");
            }
            for p in &config.peers {
                println!("{}\t{}\t{}", p.name, p.address.as_deref().unwrap_or("(discovered)"), p.code);
            }
        }
        PeerCommand::Remove { name } => {
            let before = config.peers.len();
            config.peers.retain(|p| p.name != name);
            if config.peers.len() == before {
                bail!("no peer named {name:?}");
            }
            config.save(&paths.config_file)?;
            println!("Removed {name}.");
        }
    }
    Ok(())
}

fn watch(paths: &Paths) -> Result<()> {
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let _clipboard = clipboard::spawn(identity.id, tx).context("starting clipboard")?;
    info!("watching clipboard (Ctrl+C to stop)");
    while let Some(item) = rx.blocking_recv() {
        info!(
            hash = %item.hash.short(),
            bytes = item.formats[0].data.len(),
            "text: {}",
            preview(item.text().unwrap_or_default()),
        );
    }
    Ok(())
}

fn preview(text: &str) -> String {
    const MAX: usize = 60;
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    match flat.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}
