//! Copied files: describing local copies for peers, and the folder that
//! files received from peers are downloaded into.

use anyhow::{Context, Result, bail};
use crosscopy_core::{EntryKind, FileEntry, FileManifest, format_bytes, safe_relative_path};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};
use uuid::Uuid;

/// Keeps a stray copy of a huge folder from producing an enormous manifest.
const MAX_ENTRIES: usize = 50_000;

/// A local file copy, ready to offer to peers.
#[derive(Debug, Default)]
pub struct LocalFiles {
    pub manifest: FileManifest,
    /// Local path of each file in the manifest, in manifest order.
    pub sources: Vec<PathBuf>,
}

/// Lists the copied `roots` and everything inside the copied folders.
/// Symlinks inside folders are skipped so a link loop can't recurse forever.
pub fn scan(roots: &[PathBuf], limit: u64) -> Result<LocalFiles> {
    let mut scan = Scan { out: LocalFiles::default(), total: 0, limit };
    for root in roots {
        // Following a symlink that was itself copied is what users expect.
        let meta = fs::metadata(root).with_context(|| format!("reading {}", root.display()))?;
        let name = root
            .file_name()
            .with_context(|| format!("{} can't be copied (it is a drive or root folder)", root.display()))?;
        scan.visit(root, name.to_string_lossy().into_owned(), &meta)?;
    }
    Ok(scan.out)
}

struct Scan {
    out: LocalFiles,
    total: u64,
    limit: u64,
}

impl Scan {
    fn visit(&mut self, path: &Path, relative: String, meta: &Metadata) -> Result<()> {
        if self.out.manifest.entries.len() >= MAX_ENTRIES {
            bail!("more than {MAX_ENTRIES} files and folders");
        }
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);

        if meta.is_file() {
            self.total += meta.len();
            if self.total > self.limit {
                bail!("larger than the {} limit (max_file_mb in the config)", format_bytes(self.limit));
            }
            let kind = EntryKind::File { size: meta.len(), executable: is_executable(meta) };
            self.out.manifest.entries.push(FileEntry { path: relative, kind, modified_ms });
            self.out.sources.push(path.to_owned());
        } else if meta.is_dir() {
            self.out.manifest.entries.push(FileEntry { path: relative.clone(), kind: EntryKind::Dir, modified_ms });
            let mut children: Vec<fs::DirEntry> = match fs::read_dir(path) {
                Ok(entries) => entries.filter_map(Result::ok).collect(),
                Err(e) => {
                    warn!("skipping the contents of {}: {e}", path.display());
                    return Ok(());
                }
            };
            children.sort_by_key(|c| c.file_name());
            for child in children {
                let is_symlink = child.file_type().map(|t| t.is_symlink()).unwrap_or(true);
                let Ok(child_meta) = child.metadata() else { continue };
                if is_symlink {
                    debug!("skipping symlink {}", child.path().display());
                    continue;
                }
                let name = child.file_name().to_string_lossy().into_owned();
                self.visit(&child.path(), format!("{relative}/{name}"), &child_meta)?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn is_executable(meta: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &Metadata) -> bool {
    false
}

/// The folder received files are downloaded into, one subfolder per copy.
///
/// The latest completed download stays (it is probably still on the
/// clipboard); older ones are removed when a new download starts.
pub struct Inbox {
    dir: PathBuf,
    keep: Option<PathBuf>,
}

impl Inbox {
    pub fn new(dir: PathBuf) -> Self {
        // After a restart the newest download may still be on the clipboard.
        let newest = subfolders(&dir)
            .into_iter()
            .max_by_key(|path| fs::metadata(path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH));
        let mut inbox = Self { dir, keep: newest };
        inbox.clean();
        inbox
    }

    /// Creates an empty folder for a new download.
    pub fn prepare(&mut self, item: Uuid) -> Result<PathBuf> {
        self.clean();
        let dest = self.dir.join(item.simple().to_string());
        fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
        Ok(dest)
    }

    /// Records a finished download; returns the paths to put on the clipboard.
    pub fn completed(&mut self, dest: &Path, manifest: &FileManifest) -> Vec<PathBuf> {
        self.keep = Some(dest.to_owned());
        manifest
            .roots()
            .filter_map(|entry| safe_relative_path(&entry.path))
            .map(|relative| dest.join(relative))
            .filter(|path| path.exists())
            .collect()
    }

    pub fn discard(&self, dest: &Path) {
        if let Err(e) = fs::remove_dir_all(dest) {
            debug!("could not remove {}: {e}", dest.display());
        }
    }

    /// Removes every download except the one being kept. Failures (a file
    /// that is open in another app, say) are retried next time.
    fn clean(&mut self) {
        for path in subfolders(&self.dir) {
            if Some(&path) != self.keep.as_ref() {
                self.discard(&path);
            }
        }
    }
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lists_folders_before_their_contents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("README"), "hi").unwrap();
        fs::write(dir.path().join("notes.txt"), "abc").unwrap();

        let scanned = scan(&[root.clone(), dir.path().join("notes.txt")], u64::MAX).unwrap();
        let paths: Vec<&str> = scanned.manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["project", "project/README", "project/src", "project/src/main.rs", "notes.txt"]);
        assert_eq!(
            scanned.sources,
            [root.join("README"), root.join("src").join("main.rs"), dir.path().join("notes.txt")]
        );
        assert_eq!(scanned.manifest.total_bytes(), 2 + 12 + 3);
    }

    #[test]
    fn scan_enforces_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big"), vec![0u8; 100]).unwrap();
        let err = scan(&[dir.path().join("big")], 99).unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
        assert!(scan(&[dir.path().join("missing")], 99).is_err());
    }

    #[test]
    fn inbox_keeps_only_the_latest_download() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = Inbox::new(dir.path().to_owned());
        let manifest = FileManifest {
            entries: vec![FileEntry { path: "a.txt".into(), kind: EntryKind::File { size: 1, executable: false }, modified_ms: None }],
        };

        let first = inbox.prepare(Uuid::new_v4()).unwrap();
        fs::write(first.join("a.txt"), "1").unwrap();
        assert_eq!(inbox.completed(&first, &manifest), [first.join("a.txt")]);

        let abandoned = inbox.prepare(Uuid::new_v4()).unwrap();
        let third = inbox.prepare(Uuid::new_v4()).unwrap();
        assert!(first.exists(), "the download on the clipboard is kept");
        assert!(!abandoned.exists());
        fs::write(third.join("a.txt"), "3").unwrap();
        inbox.completed(&third, &manifest);

        inbox.prepare(Uuid::new_v4()).unwrap();
        assert!(!first.exists());
        assert!(third.exists());

        // A restart keeps the newest folder.
        drop(inbox);
        Inbox::new(dir.path().to_owned());
        assert_eq!(subfolders(dir.path()).len(), 1);
    }
}
