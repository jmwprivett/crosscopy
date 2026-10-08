//! Platform-independent types shared by every crosscopy component.
//!
//! Clipboard content is represented as a list of MIME-typed payloads so the
//! wire format never depends on any one OS's clipboard format identifiers.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Well-known MIME types used for clipboard payloads.
pub mod mime {
    pub const TEXT_PLAIN: &str = "text/plain;charset=utf-8";
}

/// QUIC ALPN identifier; bump the suffix on incompatible protocol changes.
pub const ALPN: &[u8] = b"crosscopy/1";

/// Upper bound on a single encoded message.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// A device's identity: 160 bits of the BLAKE3 hash of its TLS certificate.
///
/// Peers pin each other by this value, so it doubles as the "pairing code"
/// users exchange. Displayed as RFC 4648 base32 in dash-separated groups.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 20]);

impl DeviceId {
    pub fn from_cert(der: &[u8]) -> Self {
        let mut id = [0u8; 20];
        id.copy_from_slice(&blake3::hash(der).as_bytes()[..20]);
        Self(id)
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = data_encoding::BASE32_NOPAD.encode(&self.0);
        let groups: Vec<&str> = encoded
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).expect("base32 is ascii"))
            .collect();
        f.write_str(&groups.join("-"))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid device code (expected 32 base32 characters, dashes optional)")]
pub struct InvalidDeviceId;

impl FromStr for DeviceId {
    type Err = InvalidDeviceId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cleaned: String = s
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let bytes = data_encoding::BASE32_NOPAD
            .decode(cleaned.as_bytes())
            .map_err(|_| InvalidDeviceId)?;
        Ok(Self(bytes.try_into().map_err(|_| InvalidDeviceId)?))
    }
}

/// Everything that travels between peers. Each message is sent on its own
/// QUIC unidirectional stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// First message from the accepting side, sent only after it verified
    /// the dialer. In TLS 1.3 the client's handshake completes before the
    /// server checks the client certificate, so this is how the dialer
    /// learns it was actually accepted.
    Hello,
    Clip(ClipItem),
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("serializing to a Vec cannot fail")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
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
    fn device_id_round_trips_through_code() {
        let id = DeviceId::from_cert(b"some certificate");
        let code = id.to_string();
        assert_eq!(code.len(), 32 + 7);
        assert_eq!(code.parse::<DeviceId>().unwrap(), id);
        assert_eq!(code.replace('-', "").to_lowercase().parse::<DeviceId>().unwrap(), id);
        assert!("ABCD-EFGH".parse::<DeviceId>().is_err());
    }

    #[test]
    fn message_round_trips() {
        let msg = Message::Clip(ClipItem::from_text(DeviceId([7; 20]), "hi"));
        assert_eq!(Message::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn crlf_and_lf_hash_equal() {
        let dev = DeviceId([1; 20]);
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
