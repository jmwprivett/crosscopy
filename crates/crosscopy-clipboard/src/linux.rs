//! Linux: every format on wlroots Wayland compositors (Hyprland, Sway, ...)
//! through `wlr-data-control`, and text-only polling everywhere else.

use crate::{ClipboardChanged, Contents, Error, Result, normalize_newlines, poll, wayland};
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use wl_clipboard_rs::{copy, paste};

/// How long to wait for the source app to hand over one format. A hung app
/// must not stall the clipboard thread forever.
const READ_TIMEOUT: Duration = Duration::from_secs(3);

const TEXT_TYPES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];
const URI_LIST: &str = "text/uri-list";
/// What GNOME Files, Nemo and Thunar paste from.
const GNOME_FILES: &str = "x-special/gnome-copied-files";
const HTML: &str = "text/html";
const RTF_TYPES: &[&str] = &["text/rtf", "application/rtf"];
const PNG: &str = "image/png";

pub fn watch() -> Result<Receiver<ClipboardChanged>> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match wayland::watch() {
            Ok(rx) => return Ok(rx),
            Err(e) => tracing::warn!("{e}; falling back to polling the clipboard"),
        }
    }
    poll::watch()
}

pub fn is_excluded() -> bool {
    wayland::is_excluded()
}

pub enum Backend {
    Wayland,
    TextOnly(poll::Backend),
}

impl Backend {
    pub fn new() -> Result<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && data_control_available() {
            return Ok(Self::Wayland);
        }
        tracing::warn!("no wlr-data-control clipboard; only text will sync");
        Ok(Self::TextOnly(poll::Backend::new()?))
    }

    pub fn read(&mut self) -> Result<Contents> {
        match self {
            Self::Wayland => read_wayland(),
            Self::TextOnly(backend) => backend.read(),
        }
    }

    pub fn write(&mut self, contents: &Contents) -> Result<Contents> {
        match self {
            Self::Wayland => write_wayland(contents),
            Self::TextOnly(backend) => backend.write(contents),
        }
    }
}

fn data_control_available() -> bool {
    !matches!(
        paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified),
        Err(paste::Error::MissingProtocol { .. } | paste::Error::WaylandConnection(_))
    )
}

fn read_wayland() -> Result<Contents> {
    let types = match paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified) {
        Ok(types) => types,
        Err(paste::Error::NoSeats | paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => {
            return Ok(Contents::default());
        }
        Err(e) => return Err(Error::Clipboard(e.to_string())),
    };
    let has = |mime: &str| types.contains(mime);
    let get = |mime: &str| has(mime).then(|| fetch(Some(mime))).flatten();

    // A file copy also offers its URIs as text; like the other backends,
    // report just the files.
    let files = get(URI_LIST).map(|list| parse_uri_list(&list)).unwrap_or_default();
    if !files.is_empty() {
        return Ok(Contents { files, ..Contents::default() });
    }
    let text = TEXT_TYPES
        .iter()
        .any(|t| has(t))
        .then(|| fetch(None))
        .flatten()
        .map(|bytes| normalize_newlines(String::from_utf8_lossy(&bytes).into_owned()));
    Ok(Contents {
        text,
        html: get(HTML).map(decode_html),
        rtf: RTF_TYPES.iter().find_map(|t| get(t)),
        png: get(PNG).filter(|data| data.starts_with(b"\x89PNG")),
        files,
    })
}

/// Reads one format (`None` meaning the best plain-text type), giving up
/// after [`READ_TIMEOUT`].
fn fetch(mime: Option<&str>) -> Option<Vec<u8>> {
    let mime = mime.map(str::to_owned);
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("clipboard-read".into())
        .spawn(move || {
            let requested = match &mime {
                Some(mime) => paste::MimeType::Specific(mime),
                None => paste::MimeType::Text,
            };
            let result = paste::get_contents(paste::ClipboardType::Regular, paste::Seat::Unspecified, requested)
                .map_err(|e| e.to_string())
                .and_then(|(mut pipe, _)| {
                    let mut data = Vec::new();
                    pipe.read_to_end(&mut data).map_err(|e| e.to_string())?;
                    Ok(data)
                });
            let _ = tx.send(result);
        })
        .ok()?;
    match rx.recv_timeout(READ_TIMEOUT) {
        Ok(Ok(data)) => Some(data),
        Ok(Err(e)) => {
            tracing::debug!("clipboard read failed: {e}");
            None
        }
        Err(_) => {
            tracing::warn!("the app that owns the clipboard did not respond; skipping a format");
            None
        }
    }
}

fn write_wayland(contents: &Contents) -> Result<Contents> {
    use copy::{MimeSource, MimeType, Options, Source};
    let source = |data: &[u8], mime_type: MimeType| MimeSource {
        source: Source::Bytes(data.to_vec().into_boxed_slice()),
        mime_type,
    };
    let specific = |mime: &str| MimeType::Specific(mime.to_owned());

    let mut sources = Vec::new();
    let written = if contents.files.is_empty() {
        // Plain text goes first: the extra text/plain variants that
        // wl-clipboard-rs adds are served from the first text source.
        if let Some(text) = &contents.text {
            sources.push(source(text.as_bytes(), MimeType::Text));
        }
        if let Some(html) = &contents.html {
            sources.push(source(html.as_bytes(), specific(HTML)));
        }
        if let Some(rtf) = &contents.rtf {
            sources.push(source(rtf, specific(RTF_TYPES[0])));
        }
        if let Some(png) = &contents.png {
            sources.push(source(png, specific(PNG)));
        }
        Contents { files: Vec::new(), ..contents.clone() }
    } else {
        let uris: Vec<String> = contents.files.iter().map(|p| file_uri(p)).collect();
        sources.push(source(format!("{}\r\n", uris.join("\r\n")).as_bytes(), specific(URI_LIST)));
        sources.push(source(format!("copy\n{}", uris.join("\n")).as_bytes(), specific(GNOME_FILES)));
        Contents { files: contents.files.clone(), ..Contents::default() }
    };
    if sources.is_empty() {
        return Err(Error::Clipboard("nothing to write".into()));
    }

    let mut options = Options::new();
    // Without real plain text, don't let HTML pose as text/plain.
    options.omit_additional_text_mime_types(contents.files.is_empty() && contents.text.is_none());
    // Serves paste requests from a background thread until replaced.
    options.copy_multi(sources).map_err(|e| Error::Clipboard(e.to_string()))?;
    Ok(written)
}

/// Firefox has been known to offer UTF-16 HTML with a byte order mark.
fn decode_html(data: Vec<u8>) -> String {
    match data.as_slice() {
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8(data).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
    }
}

fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|uri| {
            let rest = uri.strip_prefix("file://")?;
            // Skip an optional host ("file://localhost/...").
            let path = &rest[rest.find('/')?..];
            Some(PathBuf::from(OsString::from_vec(percent_decode(path))))
        })
        .collect()
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_round_trip() {
        let paths = vec![PathBuf::from("/home/me/a b#1%.txt"), PathBuf::from("/tmp/ünï")];
        let list: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
        assert_eq!(list[0], "file:///home/me/a%20b%231%25.txt");
        assert_eq!(parse_uri_list(format!("# comment\r\n{}\r\n", list.join("\r\n")).as_bytes()), paths);
        assert_eq!(parse_uri_list(b"file://localhost/etc/hosts\nhttps://example.com"), [PathBuf::from("/etc/hosts")]);
    }

    #[test]
    fn utf16_html_is_decoded() {
        let mut data = vec![0xFF, 0xFE];
        data.extend("<b>x</b>".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_html(data), "<b>x</b>");
        assert_eq!(decode_html(b"<i>y</i>".to_vec()), "<i>y</i>");
    }
}
