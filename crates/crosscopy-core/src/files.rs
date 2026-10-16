//! The description of a file copy that travels inside a clip. Contents are
//! streamed separately, in manifest order, when the receiver asks for them.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifest {
    /// Every copied file and folder, parents before their children.
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative path with `/` separators. Entries with a single component
    /// are the items that were copied; the rest are inside copied folders.
    pub path: String,
    pub kind: EntryKind,
    /// Last modification time, milliseconds since the Unix epoch.
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    Dir,
    File { size: u64, executable: bool },
}

impl FileManifest {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("serializing to a Vec cannot fail")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
    }

    pub fn files(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(|e| matches!(e.kind, EntryKind::File { .. }))
    }

    pub fn total_bytes(&self) -> u64 {
        self.files()
            .map(|e| match e.kind {
                EntryKind::File { size, .. } => size,
                EntryKind::Dir => 0,
            })
            .sum()
    }

    /// The copied items themselves, which are what goes on the clipboard.
    pub fn roots(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries.iter().filter(|e| !e.path.contains('/'))
    }

    /// Short human description such as "report.pdf" or "3 items (1.2 MB)".
    pub fn describe(&self) -> String {
        let roots: Vec<&FileEntry> = self.roots().collect();
        let size = format_bytes(self.total_bytes());
        match roots.as_slice() {
            [one] if one.kind == EntryKind::Dir => format!("folder {} ({size})", one.path),
            [one] => format!("{} ({size})", one.path),
            many => format!("{} items ({size})", many.len()),
        }
    }
}

/// Formats a byte count for humans, e.g. "1.2 MB".
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// Turns a manifest path from a peer into a relative path that is safe to
/// create under a download folder, or `None` if it tries to escape it.
///
/// Characters this OS can't store in a file name are replaced with `_`.
pub fn safe_relative_path(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        out.push(safe_component(component)?);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn safe_component(name: &str) -> Option<String> {
    let invalid = |c: char| c.is_control() || c == '\\' || (cfg!(windows) && "<>:\"|?*".contains(c));
    let mut cleaned: String = name.chars().map(|c| if invalid(c) { '_' } else { c }).collect();
    if cfg!(windows) {
        // Windows silently drops trailing dots and spaces, and reserves
        // device names like CON or NUL.txt.
        let trimmed = cleaned.trim_end_matches(['.', ' ']).len();
        cleaned.truncate(trimmed);
        let stem = cleaned.split('.').next().unwrap_or_default().to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            cleaned.insert(0, '_');
        }
    }
    (!cleaned.is_empty() && cleaned != "." && cleaned != "..").then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> FileEntry {
        FileEntry { path: path.into(), kind: EntryKind::File { size, executable: false }, modified_ms: None }
    }

    fn dir(path: &str) -> FileEntry {
        FileEntry { path: path.into(), kind: EntryKind::Dir, modified_ms: None }
    }

    #[test]
    fn manifest_round_trips_and_summarizes() {
        let manifest = FileManifest { entries: vec![dir("photos"), file("photos/a.jpg", 1500), file("notes.txt", 10)] };
        assert_eq!(FileManifest::decode(&manifest.encode()).unwrap(), manifest);
        assert_eq!(manifest.total_bytes(), 1510);
        assert_eq!(manifest.roots().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["photos", "notes.txt"]);
        assert_eq!(manifest.describe(), "2 items (1.5 KB)");
        assert_eq!(FileManifest { entries: vec![file("a.txt", 5)] }.describe(), "a.txt (5 B)");
    }

    #[test]
    fn traversal_is_rejected() {
        for bad in ["", "..", "a/../b", "/etc/passwd", "a//b", "./a", "a/"] {
            assert_eq!(safe_relative_path(bad), None, "{bad:?}");
        }
        assert_eq!(safe_relative_path("a/b.txt"), Some(PathBuf::from("a").join("b.txt")));
    }

    #[test]
    fn backslashes_never_act_as_separators() {
        let path = safe_relative_path("..\\..\\evil").unwrap();
        assert_eq!(path.components().count(), 1);
        assert_eq!(path, PathBuf::from(".._.._evil"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_are_made_valid() {
        assert_eq!(safe_relative_path("what?.txt"), Some(PathBuf::from("what_.txt")));
        assert_eq!(safe_relative_path("con.txt"), Some(PathBuf::from("_con.txt")));
        assert_eq!(safe_relative_path("COM1"), Some(PathBuf::from("_COM1")));
        assert_eq!(safe_relative_path("trailing. "), Some(PathBuf::from("trailing")));
        assert_eq!(safe_relative_path("..."), None);
    }

    #[test]
    fn bytes_format_for_humans() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_234_567), "1.2 MB");
    }
}
