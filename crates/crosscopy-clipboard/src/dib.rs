//! Conversions between PNG, the wire format for images, and the Windows
//! device-independent bitmaps most Windows apps put on the clipboard.

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, ImageFormat, RgbaImage};

const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;
const V5_HEADER_LEN: u32 = 124;

/// Converts `CF_DIB` / `CF_DIBV5` data (a bitmap without its file header).
pub fn to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |at: usize| dib.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let header_len = u32_at(0)? as usize;
    let bit_count = dib.get(14..16).map(|b| u16::from_le_bytes([b[0], b[1]]))?;
    let compression = u32_at(16)?;
    let colors_used = u32_at(32)? as usize;

    // Color masks follow a plain BITMAPINFOHEADER; later versions embed them.
    let masks = match (header_len, compression) {
        (40, BI_BITFIELDS) => 12,
        (40, BI_ALPHABITFIELDS) => 16,
        _ => 0,
    };
    let palette = match (bit_count, colors_used) {
        (0..=8, 0) => 1usize << bit_count,
        _ => colors_used,
    };
    let pixels_at = 14 + header_len + masks + palette * 4;

    // Prepend a BITMAPFILEHEADER so it can be decoded as a .bmp file.
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend(b"BM");
    bmp.extend(u32::try_from(14 + dib.len()).ok()?.to_le_bytes());
    bmp.extend([0u8; 4]);
    bmp.extend(u32::try_from(pixels_at).ok()?.to_le_bytes());
    bmp.extend(dib);

    let mut rgba = image::load_from_memory_with_format(&bmp, ImageFormat::Bmp).ok()?.to_rgba8();
    // Most apps leave the alpha byte of 32-bit bitmaps at zero. That means
    // "no alpha", not "fully transparent".
    if rgba.pixels().all(|p| p[3] == 0) {
        rgba.pixels_mut().for_each(|p| p[3] = 255);
    }
    encode_png(&rgba)
}

/// Converts PNG to a 32-bit bottom-up `CF_DIBV5` with straight alpha.
pub fn from_png(png: &[u8]) -> Option<Vec<u8>> {
    let rgba = image::load_from_memory_with_format(png, ImageFormat::Png).ok()?.to_rgba8();
    let (width, height) = rgba.dimensions();
    let size = width.checked_mul(height)?.checked_mul(4)?;

    let mut out = Vec::with_capacity(V5_HEADER_LEN as usize + size as usize);
    out.extend(V5_HEADER_LEN.to_le_bytes());
    out.extend(i32::try_from(width).ok()?.to_le_bytes());
    out.extend(i32::try_from(height).ok()?.to_le_bytes()); // positive: bottom-up
    out.extend(1u16.to_le_bytes()); // planes
    out.extend(32u16.to_le_bytes()); // bits per pixel
    out.extend(BI_BITFIELDS.to_le_bytes());
    out.extend(size.to_le_bytes());
    out.extend([0u8; 16]); // resolution, palette size, important colors
    out.extend(0x00FF_0000u32.to_le_bytes()); // red mask
    out.extend(0x0000_FF00u32.to_le_bytes()); // green mask
    out.extend(0x0000_00FFu32.to_le_bytes()); // blue mask
    out.extend(0xFF00_0000u32.to_le_bytes()); // alpha mask
    out.extend(0x7352_4742u32.to_le_bytes()); // LCS_sRGB
    out.extend([0u8; 48]); // endpoints, gamma
    out.extend(4u32.to_le_bytes()); // LCS_GM_IMAGES
    out.extend([0u8; 12]); // profile offset, profile size, reserved
    debug_assert_eq!(out.len(), V5_HEADER_LEN as usize);

    for row in rgba.rows().rev() {
        for p in row {
            out.extend([p[2], p[1], p[0], p[3]]);
        }
    }
    Some(out)
}

fn encode_png(image: &RgbaImage) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(image.as_raw(), image.width(), image.height(), image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RgbaImage {
        RgbaImage::from_fn(3, 2, |x, y| image::Rgba([x as u8 * 80, y as u8 * 120, 7, if x == 0 { 128 } else { 255 }]))
    }

    fn decode(png: &[u8]) -> RgbaImage {
        image::load_from_memory_with_format(png, ImageFormat::Png).unwrap().to_rgba8()
    }

    #[test]
    fn png_survives_a_trip_through_dibv5() {
        let png = encode_png(&sample()).unwrap();
        let dib = from_png(&png).unwrap();
        assert_eq!(dib.len(), 124 + 3 * 2 * 4);
        assert_eq!(decode(&to_png(&dib).unwrap()), sample());
    }

    #[test]
    fn zero_alpha_bitmaps_become_opaque() {
        // 2x1, 32-bit BI_RGB BITMAPINFOHEADER with alpha bytes left at 0.
        let mut dib = Vec::new();
        dib.extend(40u32.to_le_bytes());
        dib.extend(2i32.to_le_bytes());
        dib.extend(1i32.to_le_bytes());
        dib.extend(1u16.to_le_bytes());
        dib.extend(32u16.to_le_bytes());
        dib.extend(0u32.to_le_bytes());
        dib.extend([0u8; 20]);
        dib.extend([10, 20, 30, 0, 40, 50, 60, 0]);
        let image = decode(&to_png(&dib).unwrap());
        assert_eq!(image.get_pixel(0, 0).0, [30, 20, 10, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [60, 50, 40, 255]);
    }

    #[test]
    fn garbage_is_rejected() {
        assert_eq!(to_png(&[1, 2, 3]), None);
        assert_eq!(from_png(b"not a png"), None);
    }
}
