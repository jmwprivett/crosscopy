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

pub fn tray(black: bool, paused: bool) -> Icon {
    let bytes = if black { BLACK } else { WHITE };
    let mut image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("bundled icon is a valid PNG")
        .resize_exact(SIZE, SIZE, FilterType::Lanczos3)
        .to_rgba8();
    if paused {
        // Fade the icon so pausing is visible at a glance.
        for pixel in image.pixels_mut() {
            pixel.0[3] = (u16::from(pixel.0[3]) * 2 / 5) as u8;
        }
    }
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("icon has valid RGBA data")
}
