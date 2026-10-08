mod clipboard;
mod config;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::{Config, Paths, PeerConfig};
use crosscopy_core::{DeviceId, Message};
use crosscopy_net::{Identity, Node};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use tracing::{info, warn};

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
    /// Show this device's code and addresses, for pairing.
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
    /// Pair with a device: `crosscopy peer add mac mac.local ABCD-EFGH-...`
    Add {
        name: String,
        /// Hostname or IP, optionally with `:port`.
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
                .unwrap_or_else(|_| "info,quinn=warn,rustls=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let paths = match cli.config_dir {
        Some(dir) => Paths { config_file: dir.join("config.toml"), identity_dir: dir.join("identity") },
        None => Paths::new()?,
    };

    match cli.command {
        Command::Run => run(&paths),
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
    let peers = config
        .peers
        .iter()
        .map(PeerConfig::to_peer)
        .collect::<Result<Vec<_>>>()?;
    if peers.is_empty() {
        warn!("no peers paired yet; see `crosscopy id` and `crosscopy peer add`");
    }
    let names: HashMap<DeviceId, String> = peers.iter().map(|p| (p.id, p.name.clone())).collect();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let (local_tx, mut local_rx) = tokio::sync::mpsc::channel(16);
        let clipboard = clipboard::spawn(identity.id, local_tx)?;
        let listen = SocketAddr::from(([0, 0, 0, 0], config.port));
        let (node, mut inbox) = Node::start(&identity, listen, peers)?;
        info!(code = %identity.id, port = config.port, "crosscopy running");

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
                    Message::Clip(item) => {
                        let from = names.get(&incoming.from).cloned().unwrap_or_else(|| incoming.from.to_string());
                        clipboard.apply(item, from);
                    }
                },
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

fn show_id(paths: &Paths) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let addresses: Vec<IpAddr> = if_addrs::get_if_addrs()
        .context("listing network interfaces")?
        .into_iter()
        .map(|iface| iface.ip())
        .filter(|ip| matches!(ip, IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local()))
        .collect();

    println!("Device code: {}", identity.id);
    println!("UDP port:    {}", config.port);
    println!("Addresses:   {}", join(&addresses));
    println!("Config:      {}", paths.config_file.display());
    println!();
    println!("To pair, run this on the other device (pick any name):");
    let address = addresses.first().map_or_else(|| "<this-device-ip>".to_owned(), IpAddr::to_string);
    println!("  crosscopy peer add <name> {address} {}", identity.id);
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
            config.peers.push(PeerConfig { name: name.clone(), address, code: id.to_string() });
            config.save(&paths.config_file)?;
            println!("Paired with {name}. Restart `crosscopy run` to connect.");
        }
        PeerCommand::List => {
            if config.peers.is_empty() {
                println!("No peers paired.");
            }
            for p in &config.peers {
                println!("{}\t{}\t{}", p.name, p.address, p.code);
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
    let _clipboard = clipboard::spawn(identity.id, tx)?;
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

fn join(items: &[IpAddr]) -> String {
    if items.is_empty() {
        return "(none found)".to_owned();
    }
    items.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}
