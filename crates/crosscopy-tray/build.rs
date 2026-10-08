//! Embeds the app icon into the Windows executable, generated from
//! `assets/crosscopy.png` so the PNG stays the single source of truth.

use image::{Rgba, RgbaImage, imageops};
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};

fn main() {
    let assets = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../assets");
    let glyph = assets.join("crosscopy.png");
    println!("cargo:rerun-if-changed={}", glyph.display());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon(&glyph);
    }
}

fn embed_windows_icon(glyph_path: &Path) {
    let glyph = image::open(glyph_path).expect("reading app icon").to_rgba8();
    let tile = tile(&glyph, 256);

    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
        let scaled = imageops::resize(&tile, size, size, imageops::FilterType::Lanczos3);
        let image = ico::IconImage::from_rgba_data(size, size, scaled.into_raw());
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encoding icon"));
    }
    let ico_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("crosscopy.ico");
    dir.write(File::create(&ico_path).expect("creating icon file"))
        .expect("writing icon file");

    let mut resources = winresource::WindowsResource::new();
    resources
        .set_icon(ico_path.to_str().unwrap())
        .set("ProductName", "CrossCopy")
        .set("FileDescription", "CrossCopy clipboard sync");
    resources.compile().expect("embedding Windows resources");
}

/// The white glyph on a dark rounded tile, so the icon reads on both light
/// and dark Explorer backgrounds.
fn tile(glyph: &RgbaImage, size: u32) -> RgbaImage {
    const BACKGROUND: [u8; 3] = [0x1f, 0x29, 0x37];
    let s = size as f32;
    let radius = s * 0.22;
    let mut tile = RgbaImage::from_fn(size, size, |x, y| {
        // Distance outside the rounded rectangle, for an anti-aliased edge.
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let dx = (radius - fx).max(fx - (s - radius)).max(0.0);
        let dy = (radius - fy).max(fy - (s - radius)).max(0.0);
        let outside = (dx * dx + dy * dy).sqrt() - radius;
        let coverage = (0.5 - outside).clamp(0.0, 1.0);
        let [r, g, b] = BACKGROUND;
        Rgba([r, g, b, (coverage * 255.0).round() as u8])
    });
    let inner = (s * 0.72) as u32;
    let scaled = imageops::resize(glyph, inner, inner, imageops::FilterType::Lanczos3);
    let offset = i64::from((size - inner) / 2);
    imageops::overlay(&mut tile, &scaled, offset, offset);
    tile
}
