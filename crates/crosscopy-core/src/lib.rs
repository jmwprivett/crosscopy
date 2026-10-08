//! Platform-independent types shared by every crosscopy component.
//!
//! Clipboard content is represented as a list of MIME-typed payloads so the
//! wire format never depends on any one OS's clipboard format identifiers.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Well-known MIME types used for clipboard payloads.
pub mod mime {
    pub const TEXT_PLAIN: &str = "text/plain;charset=utf-8";
}

/// Stable identifier for a device participating in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub Uuid);

impl DeviceId {
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }
}

/// BLAKE3 hash of a clip's payloads, used for dedup and echo suppression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub fn short(&self) -> String {
        self.0[..4].iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// One representation of the clipboard contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipFormat {
    pub mime: String,
    pub data: Vec<u8>,
}

/// A single clipboard snapshot, possibly carrying several representations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipItem {
    pub id: Uuid,
    pub origin: DeviceId,
    /// Milliseconds since the Unix epoch on the origin device.
    pub timestamp_ms: u64,
    pub hash: ContentHash,
    pub formats: Vec<ClipFormat>,
}

impl ClipItem {
    pub fn new(origin: DeviceId, formats: Vec<ClipFormat>) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            id: Uuid::new_v4(),
            origin,
            timestamp_ms,
            hash: hash_formats(&formats),
            formats,
        }
    }

    pub fn from_text(origin: DeviceId, text: &str) -> Self {
        Self::new(
            origin,
            vec![ClipFormat {
                mime: mime::TEXT_PLAIN.to_owned(),
                data: normalize_newlines(text).into_bytes(),
            }],
        )
    }

    pub fn text(&self) -> Option<&str> {
        self.formats
            .iter()
            .find(|f| f.mime == mime::TEXT_PLAIN)
            .and_then(|f| std::str::from_utf8(&f.data).ok())
    }
}

/// Hash payloads order-independently of how the OS happened to list them.
pub fn hash_formats(formats: &[ClipFormat]) -> ContentHash {
    let mut sorted: Vec<&ClipFormat> = formats.iter().collect();
    sorted.sort_by(|a, b| a.mime.cmp(&b.mime));
    let mut hasher = blake3::Hasher::new();
    for f in sorted {
        // Length-prefix each field so ("ab","c") and ("a","bc") differ.
        hasher.update(&(f.mime.len() as u64).to_le_bytes());
        hasher.update(f.mime.as_bytes());
        hasher.update(&(f.data.len() as u64).to_le_bytes());
        hasher.update(&f.data);
    }
    ContentHash(*hasher.finalize().as_bytes())
}

/// Canonical wire form for text is LF; each platform converts on write.
pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_and_lf_hash_equal() {
        let dev = DeviceId::random();
        let a = ClipItem::from_text(dev, "a\r\nb");
        let b = ClipItem::from_text(dev, "a\nb");
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.text(), Some("a\nb"));
    }

    #[test]
    fn hash_ignores_format_order() {
        let x = ClipFormat { mime: "a".into(), data: vec![1] };
        let y = ClipFormat { mime: "b".into(), data: vec![2] };
        assert_eq!(
            hash_formats(&[x.clone(), y.clone()]),
            hash_formats(&[y, x])
        );
    }

    #[test]
    fn hash_fields_are_length_prefixed() {
        let a = ClipFormat { mime: "ab".into(), data: b"c".to_vec() };
        let b = ClipFormat { mime: "a".into(), data: b"bc".to_vec() };
        assert_ne!(hash_formats(&[a]), hash_formats(&[b]));
    }
}
