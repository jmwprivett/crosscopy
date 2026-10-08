use image::imageops::FilterType;
use tray_icon::Icon;

/// White glyph, for dark taskbars.
const WHITE: &[u8] = include_bytes!("../../../assets/crosscopy.png");
/// Black glyph, for light taskbars and as the macOS template image.
const BLACK: &[u8] = include_bytes!("../../../assets/crosscopy-light.png");

/// Windows scales tray icons down from this; macOS draws the menu bar icon
/// at about 18pt, so give it enough pixels for Retina.
#[cfg(windows)]
const SIZE: u32 = 32;
#[cfg(not(windows))]
const SIZE: u32 = 64;

/// Fraction of the icon the glyph fills. macOS scales the image to the full
/// menu bar icon height, and an edge-to-edge glyph looks oversized next to
/// the system icons, so pad it there.
#[cfg(target_os = "macos")]
const GLYPH_SCALE: f32 = 0.75;
#[cfg(not(target_os = "macos"))]
const GLYPH_SCALE: f32 = 1.0;

pub fn tray(black: bool, paused: bool) -> Icon {
    let bytes = if black { BLACK } else { WHITE };
    let glyph_size = (SIZE as f32 * GLYPH_SCALE).round() as u32;
    let glyph = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("bundled icon is a valid PNG")
        .resize_exact(glyph_size, glyph_size, FilterType::Lanczos3)
        .to_rgba8();
    let mut image = image::RgbaImage::new(SIZE, SIZE);
    let offset = i64::from((SIZE - glyph_size) / 2);
    image::imageops::overlay(&mut image, &glyph, offset, offset);
    if paused {
        // Fade the icon so pausing is visible at a glance.
        for pixel in image.pixels_mut() {
            pixel.0[3] = (u16::from(pixel.0[3]) * 2 / 5) as u8;
        }
    }
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("icon has valid RGBA data")
}
