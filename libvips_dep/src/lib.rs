use std::io::Write;
use std::sync::Once;

use image::DynamicImage;
use libvips_rs::{VipsApp, VipsImage};

const VIPS_MAGIC_LE: [u8; 4] = [0xb6, 0xa6, 0xf2, 0x08];
const VIPS_MAGIC_BE: [u8; 4] = [0x08, 0xf2, 0xa6, 0xb6];
const MAX_SIDE: u64 = 32768;

pub fn is_vips(bytes: &[u8]) -> bool {
    bytes.starts_with(&VIPS_MAGIC_LE) || bytes.starts_with(&VIPS_MAGIC_BE)
}

pub fn decode(bytes: &[u8], max_decode_pixels: u64) -> Result<DynamicImage, String> {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let app = VipsApp::new("media-proxy-rs", false).expect("failed to initialize libvips");
        app.concurrency_set(1);
        Box::leak(Box::new(app));
    });

    let mut tmp = tempfile::Builder::new()
        .suffix(".v")
        .tempfile()
        .map_err(|error| format!("temporary file: {error}"))?;
    tmp.write_all(bytes)
        .map_err(|error| format!("temporary file write: {error}"))?;
    tmp.flush()
        .map_err(|error| format!("temporary file flush: {error}"))?;

    let path = tmp
        .path()
        .to_str()
        .ok_or_else(|| "temporary path is not UTF-8".to_owned())?;
    let image = VipsImage::new_from_file(path).map_err(|error| format!("{error:?}"))?;
    let width = u32::try_from(image.get_width()).map_err(|_| "invalid width".to_owned())?;
    let height = u32::try_from(image.get_height()).map_err(|_| "invalid height".to_owned())?;
    let bands = usize::try_from(image.get_bands()).map_err(|_| "invalid bands".to_owned())?;

    if !dimensions_allowed(max_decode_pixels, width as u64, height as u64) {
        return Err(format!("DecodeDimensions {width}x{height} over limit"));
    }

    let pixels = image.image_write_to_memory();
    let expected = width as usize * height as usize * bands;
    if pixels.len() != expected {
        return Err(format!(
            "unsupported pixel buffer: {} bytes for {width}x{height}x{bands}",
            pixels.len()
        ));
    }

    match bands {
        1 => image::GrayImage::from_raw(width, height, pixels)
            .map(DynamicImage::ImageLuma8)
            .ok_or_else(|| "invalid grayscale pixel buffer".to_owned()),
        2 => image::GrayAlphaImage::from_raw(width, height, pixels)
            .map(DynamicImage::ImageLumaA8)
            .ok_or_else(|| "invalid grayscale-alpha pixel buffer".to_owned()),
        3 => image::RgbImage::from_raw(width, height, pixels)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| "invalid RGB pixel buffer".to_owned()),
        4 => image::RgbaImage::from_raw(width, height, pixels)
            .map(DynamicImage::ImageRgba8)
            .ok_or_else(|| "invalid RGBA pixel buffer".to_owned()),
        _ => Err(format!("unsupported band count: {bands}")),
    }
}

fn dimensions_allowed(max_decode_pixels: u64, width: u64, height: u64) -> bool {
    width > 0
        && height > 0
        && width <= MAX_SIDE
        && height <= MAX_SIDE
        && width
            .checked_mul(height)
            .is_some_and(|pixels| pixels <= max_decode_pixels)
}

#[cfg(test)]
mod tests {
    use super::is_vips;

    #[test]
    fn detects_vips_magic() {
        assert!(is_vips(&[0xb6, 0xa6, 0xf2, 0x08, 0x00]));
        assert!(is_vips(&[0x08, 0xf2, 0xa6, 0xb6, 0x00]));
        assert!(!is_vips(b"not vips"));
    }
}
