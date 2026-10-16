//! Exercises the real system clipboard, so it replaces whatever is on it.
//! Not run by default: `cargo test -p crosscopy-clipboard -- --ignored --test-threads=1`

use crosscopy_clipboard::{Clipboard, Contents};
use std::path::PathBuf;

/// A 2x2 PNG with partial transparency.
fn png() -> Vec<u8> {
    let mut png = Vec::new();
    let pixels = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128];
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels, 2, 2, image::ExtendedColorType::Rgba8)
        .unwrap();
    png
}

use image::ImageEncoder;

#[test]
#[ignore = "replaces the system clipboard"]
fn rich_contents_round_trip() {
    let mut clipboard = Clipboard::new().unwrap();
    let written = Contents {
        text: Some("line one\nline two ✓".into()),
        html: Some("<meta charset='utf-8'><b>line one</b><br>line two ✓".into()),
        rtf: Some(b"{\\rtf1\\ansi {\\b line one}\\par line two}".to_vec()),
        png: Some(png()),
        files: Vec::new(),
    };
    let reported = clipboard.write(&written).unwrap();
    let read = clipboard.read().unwrap();
    assert_eq!(read, reported, "write() must report what read() sees");
    assert_eq!(read.text, written.text);
    assert_eq!(read.html, written.html);
    assert_eq!(read.rtf, written.rtf);
    assert_eq!(read.png, written.png);
}

#[test]
#[ignore = "replaces the system clipboard"]
fn image_only_round_trip() {
    let mut clipboard = Clipboard::new().unwrap();
    let written = Contents { png: Some(png()), ..Contents::default() };
    let read = clipboard.write(&written).unwrap();
    assert_eq!(read.png, written.png);
    assert_eq!(read.text, None);
}

#[test]
#[ignore = "replaces the system clipboard"]
fn files_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<PathBuf> = ["a.txt", "b c.txt"].iter().map(|n| dir.path().join(n)).collect();
    for f in &files {
        std::fs::write(f, "x").unwrap();
    }
    let mut clipboard = Clipboard::new().unwrap();
    let read = clipboard.write(&Contents { files: files.clone(), ..Contents::default() }).unwrap();
    assert_eq!(read.files, files);
    assert_eq!(clipboard.read().unwrap().files, files);
}
