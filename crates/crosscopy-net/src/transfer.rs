//! File contents for a copied-files clip, fetched by the receiver on a
//! bidirectional stream of the existing connection:
//!
//! - request: a postcard [`FetchRequest`], then end of stream
//! - response: one status byte, then for every file in manifest order a
//!   little-endian `u64` length (or [`MISSING`]) followed by that many bytes
//!
//! Only files the sender explicitly offered can be requested.

use anyhow::{Context, Result, bail, ensure};
use crosscopy_core::{EntryKind, FileManifest, format_bytes, safe_relative_path};
use quinn::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;
use uuid::Uuid;

/// How many recent copies stay fetchable.
pub(crate) const MAX_OFFERS: usize = 8;
const STATUS_OK: u8 = 0;
const STATUS_UNKNOWN: u8 = 1;
/// Length sent in place of a file the sender could no longer read.
const MISSING: u64 = u64::MAX;
const CHUNK: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct FetchRequest {
    item: Uuid,
}

/// Source paths of an offered clip's files, in manifest order.
pub(crate) type Offer = Arc<[PathBuf]>;

/// Answers one fetch request. `lookup` finds the offered files for a clip.
pub(crate) async fn serve(
    mut send: SendStream,
    mut recv: RecvStream,
    lookup: impl FnOnce(Uuid) -> Option<Offer>,
) -> Result<()> {
    let request: FetchRequest = postcard::from_bytes(&recv.read_to_end(1024).await?)?;
    let Some(sources) = lookup(request.item) else {
        send.write_all(&[STATUS_UNKNOWN]).await?;
        send.finish()?;
        return Ok(());
    };
    send.write_all(&[STATUS_OK]).await?;

    let mut buf = vec![0u8; CHUNK];
    for path in sources.iter() {
        let (mut file, len) = match open(path).await {
            Ok(opened) => opened,
            Err(e) => {
                warn!("cannot send {}: {e}", path.display());
                send.write_all(&MISSING.to_le_bytes()).await?;
                continue;
            }
        };
        send.write_all(&len.to_le_bytes()).await?;
        let mut left = len;
        while left > 0 {
            let n = file.read(&mut buf[..chunk(left)]).await?;
            if n == 0 {
                // The receiver was promised `len` bytes; abort rather than
                // let it misread the rest of the stream.
                let _ = send.reset(1u32.into());
                bail!("{} got shorter while being sent", path.display());
            }
            send.write_all(&buf[..n]).await?;
            left -= n as u64;
        }
    }
    send.finish()?;
    // Wait for the receiver to have everything before dropping the stream.
    let _ = send.stopped().await;
    Ok(())
}

async fn open(path: &Path) -> std::io::Result<(tokio::fs::File, u64)> {
    let file = tokio::fs::File::open(path).await?;
    let meta = file.metadata().await?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok((file, meta.len()))
}

fn chunk(left: u64) -> usize {
    usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK))
}

/// Downloads an offered clip's files into `dest`, recreating the manifest's
/// folder structure. Adds received bytes to `progress` as they arrive.
pub(crate) async fn fetch(
    conn: &Connection,
    item: Uuid,
    manifest: &FileManifest,
    dest: &Path,
    limit: u64,
    progress: &AtomicU64,
) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&postcard::to_allocvec(&FetchRequest { item })?).await?;
    send.finish()?;

    let mut status = [0u8];
    recv.read_exact(&mut status).await.context("reading transfer status")?;
    ensure!(status[0] == STATUS_OK, "the other device no longer offers these files; copy them again");

    let mut total = 0u64;
    let mut buf = vec![0u8; CHUNK];
    let mut dirs = Vec::new();
    for entry in &manifest.entries {
        let relative = safe_relative_path(&entry.path).with_context(|| format!("refusing unsafe path {:?}", entry.path))?;
        let path = dest.join(relative);
        let EntryKind::File { executable, .. } = entry.kind else {
            tokio::fs::create_dir_all(&path).await.with_context(|| format!("creating {}", path.display()))?;
            dirs.push((path, entry.modified_ms));
            continue;
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| format!("creating {}", parent.display()))?;
        }

        let mut len = [0u8; 8];
        recv.read_exact(&mut len).await.context("transfer ended early")?;
        let len = u64::from_le_bytes(len);
        if len == MISSING {
            warn!(file = entry.path, "the other device could not read this file; skipping it");
            continue;
        }
        total = total.saturating_add(len);
        ensure!(total <= limit, "transfer is larger than the {} limit", format_bytes(limit));

        let mut file = tokio::fs::File::create(&path).await.with_context(|| format!("creating {}", path.display()))?;
        let mut left = len;
        while left > 0 {
            let n = recv.read(&mut buf[..chunk(left)]).await?.context("transfer ended early")?;
            file.write_all(&buf[..n]).await.with_context(|| format!("writing {}", path.display()))?;
            left -= n as u64;
            progress.fetch_add(n as u64, Ordering::Relaxed);
        }
        file.flush().await?;
        let file = file.into_std().await;
        set_metadata(&file, entry.modified_ms, executable);
    }
    // Last, since creating their contents bumped the folders' times.
    for (path, modified_ms) in dirs.iter().rev() {
        if let Ok(dir) = open_dir(path) {
            set_metadata(&dir, *modified_ms, false);
        }
    }
    Ok(())
}

/// Best effort: a missing timestamp or permission bit isn't worth failing on.
fn set_metadata(file: &std::fs::File, modified_ms: Option<u64>, executable: bool) {
    if let Some(ms) = modified_ms {
        let _ = file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
    }
    if executable {
        set_executable(file);
    }
}

#[cfg(unix)]
fn set_executable(file: &std::fs::File) {
    use std::os::unix::fs::PermissionsExt;
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_executable(_: &std::fs::File) {}

#[cfg(windows)]
fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    // Required to open a folder rather than a file.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(not(windows))]
fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}
