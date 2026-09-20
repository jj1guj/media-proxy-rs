use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io::Write;
use std::ptr;
use std::sync::OnceLock;

use image::DynamicImage;

const VIPS_MAGIC_LE: [u8; 4] = [0xb6, 0xa6, 0xf2, 0x08];
const VIPS_MAGIC_BE: [u8; 4] = [0x08, 0xf2, 0xa6, 0xb6];
const MAX_SIDE: u64 = 32768;

extern "C" {
    fn vips_init(argv0: *const c_char) -> c_int;
    fn vips_concurrency_set(concurrency: c_int);
    fn vips_error_buffer() -> *const c_char;
    fn vips_error_clear();
    fn vips_image_new_from_file(name: *const c_char, ...) -> *mut c_void;
    fn vips_image_get_width(image: *const c_void) -> c_int;
    fn vips_image_get_height(image: *const c_void) -> c_int;
    fn vips_image_get_bands(image: *const c_void) -> c_int;
    fn vips_image_write_to_memory(image: *mut c_void, size: *mut usize) -> *mut c_void;
    fn g_object_unref(object: *mut c_void);
    fn g_free(memory: *mut c_void);
}

struct Image(*mut c_void);

impl Drop for Image {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { g_object_unref(self.0) };
        }
    }
}

pub fn is_vips(bytes: &[u8]) -> bool {
    bytes.starts_with(&VIPS_MAGIC_LE) || bytes.starts_with(&VIPS_MAGIC_BE)
}

pub fn decode(bytes: &[u8], max_decode_pixels: u64) -> Result<DynamicImage, String> {
    init_vips()?;

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
    let path = CString::new(path).map_err(|_| "temporary path contains NUL".to_owned())?;
    let image = unsafe {
        Image(vips_image_new_from_file(
            path.as_ptr(),
            ptr::null::<c_char>(),
        ))
    };
    if image.0.is_null() {
        return Err(last_vips_error("failed to load VIPS image"));
    }

    let width = u32::try_from(unsafe { vips_image_get_width(image.0) })
        .map_err(|_| "invalid width".to_owned())?;
    let height = u32::try_from(unsafe { vips_image_get_height(image.0) })
        .map_err(|_| "invalid height".to_owned())?;
    let bands = usize::try_from(unsafe { vips_image_get_bands(image.0) })
        .map_err(|_| "invalid bands".to_owned())?;

    if !dimensions_allowed(max_decode_pixels, width as u64, height as u64) {
        return Err(format!("DecodeDimensions {width}x{height} over limit"));
    }

    let mut pixel_bytes = 0usize;
    let pixel_data = unsafe { vips_image_write_to_memory(image.0, &mut pixel_bytes) };
    if pixel_data.is_null() {
        return Err(last_vips_error("failed to export VIPS pixels"));
    }
    let pixels =
        unsafe { std::slice::from_raw_parts(pixel_data.cast::<u8>(), pixel_bytes) }.to_vec();
    unsafe { g_free(pixel_data) };
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(bands))
        .ok_or_else(|| "pixel buffer size overflow".to_owned())?;
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

fn init_vips() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        let name = CString::new("media-proxy-rs").unwrap();
        if unsafe { vips_init(name.as_ptr()) } != 0 {
            return Err(last_vips_error("failed to initialize libvips"));
        }
        unsafe { vips_concurrency_set(1) };
        Ok(())
    })
    .clone()
}

fn last_vips_error(fallback: &str) -> String {
    unsafe {
        let error = vips_error_buffer();
        let message = if error.is_null() {
            fallback.to_owned()
        } else {
            CStr::from_ptr(error).to_string_lossy().trim().to_owned()
        };
        vips_error_clear();
        if message.is_empty() {
            fallback.to_owned()
        } else {
            message
        }
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
