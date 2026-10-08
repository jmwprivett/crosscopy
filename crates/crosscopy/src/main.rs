use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crosscopy::config::{Config, Paths, PeerConfig};
use crosscopy::daemon::{self, Control, SharedStatus};
use crosscopy::{clipboard, pair};
use crosscopy_core::DeviceId;
use crosscopy_net::Identity;
use std::io::Write;
use std::path::PathBuf;
use tracing::info;

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
    /// Run the sync daemon in the foreground (the tray app does this too).
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
    let paths = Paths::new(cli.config_dir)?;

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
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let (control, control_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = control.send(Control::Shutdown);
            }
        });
        daemon::run(paths, control_rx, SharedStatus::default()).await
    })
}

fn pair(paths: &Paths, only: Option<String>) -> Result<()> {
    let config = Config::load(&paths.config_file)?;
    let identity = Identity::load_or_create(&paths.identity_dir)?;
    let name = config.device_name();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        println!("Pairing as \"{name}\". Run `crosscopy pair` on the other device too.");
        println!("Searching the local network (up to {} s, Ctrl+C to cancel)...", pair::PAIR_TIMEOUT.as_secs());
        let found = pair::find(&identity, &name, only.as_deref(), |device, wanted| {
            if wanted {
                println!("Found \"{device}\".");
            } else {
                println!("Ignoring \"{device}\".");
            }
        });
        let (_pairing, pending) = tokio::select! {
            result = found => result?,
            _ = tokio::signal::ctrl_c() => bail!("cancelled"),
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
        let saved_as = pair::save(paths, peer_id, &peer_name, peer_ip)?;
        println!("Paired with \"{saved_as}\". A running crosscopy picks this up automatically.");
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
    let addresses: Vec<String> = crosscopy_net::local_addresses().iter().map(ToString::to_string).collect();

    println!("Name:        {}", config.device_name());
    println!("Device code: {}", identity.id);
    println!("UDP port:    {}", config.port);
    println!("Addresses:   {}", if addresses.is_empty() { "(none found)".to_owned() } else { addresses.join(", ") });
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
            if !config.remove_peer(&name) {
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
