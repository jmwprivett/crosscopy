use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crosscopy_clipboard::Clipboard;
use crosscopy_core::{ClipItem, ContentHash, DeviceId};
use std::time::Duration;
use tracing::{info, warn};

/// Apps often write the clipboard several times in a burst (one call per
/// format), so wait for the burst to settle before reading.
const SETTLE: Duration = Duration::from_millis(50);

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print every local clipboard change.
    Watch,
    /// Put text on the local clipboard.
    Set { text: String },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Watch => watch(),
        Command::Set { text } => {
            Clipboard::new()?.write_text(&text)?;
            Ok(())
        }
    }
}

fn watch() -> Result<()> {
    let device = DeviceId::random();
    let mut clipboard = Clipboard::new().context("opening clipboard")?;
    let changes = crosscopy_clipboard::watch().context("starting clipboard watcher")?;
    let mut last: Option<ContentHash> = None;

    info!("watching clipboard (Ctrl+C to stop)");
    while changes.recv().is_ok() {
        std::thread::sleep(SETTLE);
        while changes.try_recv().is_ok() {}

        if crosscopy_clipboard::is_excluded() {
            info!("skipped: source app marked content as sensitive/transient");
            continue;
        }
        let text = match clipboard.read_text() {
            Ok(Some(text)) => text,
            Ok(None) => {
                info!("changed (no text content)");
                continue;
            }
            Err(e) => {
                warn!("failed to read clipboard: {e}");
                continue;
            }
        };

        let item = ClipItem::from_text(device, &text);
        if last == Some(item.hash) {
            continue;
        }
        last = Some(item.hash);
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
