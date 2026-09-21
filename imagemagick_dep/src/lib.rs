use std::ffi::{c_char, c_uint, c_void, CStr};
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::time::Duration;

use image::{Frame, RgbaImage};

const MNG_SIGNATURE: &[u8; 8] = b"\x8aMNG\r\n\x1a\n";
const MAX_SIDE: u64 = 32768;

extern "C" {
    fn MagickWandGenesis();
    fn NewMagickWand() -> *mut c_void;
    fn DestroyMagickWand(wand: *mut c_void) -> *mut c_void;
    fn MagickReadImageBlob(wand: *mut c_void, blob: *const c_void, length: usize) -> c_uint;
    fn MagickCoalesceImages(wand: *mut c_void) -> *mut c_void;
    fn MagickGetNumberImages(wand: *const c_void) -> usize;
    fn MagickResetIterator(wand: *mut c_void);
    fn MagickNextImage(wand: *mut c_void) -> c_uint;
    fn MagickGetImageWidth(wand: *const c_void) -> usize;
    fn MagickGetImageHeight(wand: *const c_void) -> usize;
    fn MagickGetImageDelay(wand: *const c_void) -> usize;
    fn MagickGetImageTicksPerSecond(wand: *const c_void) -> isize;
    fn MagickGetImageIterations(wand: *const c_void) -> usize;
    fn MagickExportImagePixels(
        wand: *const c_void,
        x: isize,
        y: isize,
        width: usize,
        height: usize,
        map: *const c_char,
        storage: c_uint,
        pixels: *mut c_void,
    ) -> c_uint;
    fn MagickGetException(wand: *const c_void, severity: *mut c_uint) -> *mut c_char;
    fn MagickRelinquishMemory(resource: *mut c_void) -> *mut c_void;
    fn MagickSetResourceLimit(resource: c_uint, limit: u64) -> c_uint;
}

struct Wand {
    ptr: NonNull<c_void>,
}

impl Wand {
    fn new() -> Result<Self, String> {
        let ptr = NonNull::new(unsafe { NewMagickWand() })
            .ok_or_else(|| "ImageMagick failed to create a wand".to_owned())?;
        Ok(Self { ptr })
    }

    fn from_raw(ptr: *mut c_void) -> Result<Self, String> {
        let ptr = NonNull::new(ptr)
            .ok_or_else(|| "ImageMagick failed to coalesce MNG frames".to_owned())?;
        Ok(Self { ptr })
    }

    fn error(&self, fallback: &str) -> String {
        let mut severity = 0;
        let message = unsafe { MagickGetException(self.ptr.as_ptr(), &mut severity) };
        if message.is_null() {
            return fallback.to_owned();
        }
        let result = unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .trim()
            .to_owned();
        unsafe { MagickRelinquishMemory(message.cast()) };
        if result.is_empty() {
            fallback.to_owned()
        } else {
            result
        }
    }
}

impl Drop for Wand {
    fn drop(&mut self) {
        unsafe { DestroyMagickWand(self.ptr.as_ptr()) };
    }
}

#[derive(Debug, Clone, Copy)]
struct MngHeader {
    width: u32,
    height: u32,
    nominal_frames: u32,
}

pub fn is_mng(bytes: &[u8]) -> bool {
    bytes.starts_with(MNG_SIGNATURE)
}

pub fn decode(
    bytes: &[u8],
    max_decode_pixels: u64,
    max_frames: u64,
) -> Result<(Vec<Frame>, u32), String> {
    let header = parse_header(bytes)?;
    validate_dimensions(header, max_decode_pixels, max_frames)?;

    initialize_magick()?;
    let source = Wand::new()?;
    let loaded =
        unsafe { MagickReadImageBlob(source.ptr.as_ptr(), bytes.as_ptr().cast(), bytes.len()) };
    if loaded == 0 {
        return Err(source.error("ImageMagick failed to decode MNG"));
    }

    let frame_count = unsafe { MagickGetNumberImages(source.ptr.as_ptr()) };
    if frame_count == 0 {
        return Err("MNG contains no frames".to_owned());
    }
    if frame_count as u64 > max_frames {
        return Err(format!("FramesLimit {frame_count}>{max_frames}"));
    }
    let decoded_pixels = (header.width as u64)
        .checked_mul(header.height as u64)
        .and_then(|pixels| pixels.checked_mul(frame_count as u64))
        .ok_or_else(|| "MNG decoded pixel count overflow".to_owned())?;
    if decoded_pixels > max_decode_pixels {
        return Err(format!("DecodePixels {decoded_pixels}>{max_decode_pixels}"));
    }

    let (frame_durations, loop_count) = animation_metadata(&source, frame_count)?;

    let coalesced_ptr = unsafe { MagickCoalesceImages(source.ptr.as_ptr()) };
    if coalesced_ptr.is_null() {
        return Err(source.error("ImageMagick failed to coalesce MNG frames"));
    }
    let coalesced = Wand::from_raw(coalesced_ptr)?;
    frames_from_wand(&coalesced, &frame_durations, max_decode_pixels, max_frames)
        .map(|frames| (frames, loop_count))
}

fn animation_metadata(wand: &Wand, frame_count: usize) -> Result<(Vec<Duration>, u32), String> {
    unsafe { MagickResetIterator(wand.ptr.as_ptr()) };
    if unsafe { MagickNextImage(wand.ptr.as_ptr()) } == 0 {
        return Err("MNG contains no readable frames".to_owned());
    }
    let loop_count =
        u32::try_from(unsafe { MagickGetImageIterations(wand.ptr.as_ptr()) }).unwrap_or(u32::MAX);
    let mut durations = Vec::with_capacity(frame_count);
    loop {
        durations.push(frame_duration(wand));
        if unsafe { MagickNextImage(wand.ptr.as_ptr()) } == 0 {
            break;
        }
    }
    if durations.len() != frame_count {
        return Err(format!(
            "MNG frame metadata count mismatch: {}!={frame_count}",
            durations.len()
        ));
    }
    Ok((durations, loop_count))
}

fn frames_from_wand(
    wand: &Wand,
    frame_durations: &[Duration],
    max_decode_pixels: u64,
    max_frames: u64,
) -> Result<Vec<Frame>, String> {
    let mut frames = Vec::new();
    let mut total_pixels = 0u64;
    unsafe { MagickResetIterator(wand.ptr.as_ptr()) };

    while unsafe { MagickNextImage(wand.ptr.as_ptr()) } != 0 {
        if frames.len() as u64 >= max_frames {
            return Err(format!("FramesLimit {}>{max_frames}", frames.len() + 1));
        }
        let width = unsafe { MagickGetImageWidth(wand.ptr.as_ptr()) };
        let height = unsafe { MagickGetImageHeight(wand.ptr.as_ptr()) };
        let frame_pixels = (width as u64)
            .checked_mul(height as u64)
            .ok_or_else(|| "MNG frame dimensions overflow".to_owned())?;
        total_pixels = total_pixels
            .checked_add(frame_pixels)
            .ok_or_else(|| "MNG decoded pixel count overflow".to_owned())?;
        if width == 0
            || height == 0
            || width as u64 > MAX_SIDE
            || height as u64 > MAX_SIDE
            || total_pixels > max_decode_pixels
        {
            return Err(format!(
                "DecodeDimensions {width}x{height}, pixels {total_pixels}>{max_decode_pixels}"
            ));
        }

        let pixel_bytes = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "MNG RGBA buffer size overflow".to_owned())?;
        let mut rgba = vec![0u8; pixel_bytes];
        let exported = unsafe {
            MagickExportImagePixels(
                wand.ptr.as_ptr(),
                0,
                0,
                width,
                height,
                c"RGBA".as_ptr(),
                1,
                rgba.as_mut_ptr().cast(),
            )
        };
        if exported == 0 {
            return Err(wand.error("ImageMagick failed to export MNG pixels"));
        }

        let width = u32::try_from(width).map_err(|_| "MNG width exceeds u32".to_owned())?;
        let height = u32::try_from(height).map_err(|_| "MNG height exceeds u32".to_owned())?;
        let image = RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| "invalid MNG RGBA buffer".to_owned())?;
        let delay = frame_durations
            .get(frames.len())
            .copied()
            .map(image::Delay::from_saturating_duration)
            .ok_or_else(|| "MNG frame duration is missing".to_owned())?;
        frames.push(Frame::from_parts(image, 0, 0, delay));
    }

    if frames.is_empty() {
        Err("MNG contains no decoded frames".to_owned())
    } else {
        Ok(frames)
    }
}

fn frame_duration(wand: &Wand) -> Duration {
    let ticks = unsafe { MagickGetImageDelay(wand.ptr.as_ptr()) } as u64;
    let ticks_per_second = unsafe { MagickGetImageTicksPerSecond(wand.ptr.as_ptr()) };
    if ticks_per_second > 0 {
        Duration::from_secs_f64(ticks as f64 / ticks_per_second as f64)
    } else {
        Duration::ZERO
    }
}

fn parse_header(bytes: &[u8]) -> Result<MngHeader, String> {
    if !is_mng(bytes) {
        return Err("not an MNG image".to_owned());
    }
    if bytes.len() < 48 || &bytes[12..16] != b"MHDR" {
        return Err("MNG is missing its MHDR chunk".to_owned());
    }
    let chunk_length = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    if chunk_length != 28 {
        return Err(format!("invalid MNG MHDR length: {chunk_length}"));
    }
    Ok(MngHeader {
        width: u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        height: u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        nominal_frames: u32::from_be_bytes(bytes[32..36].try_into().unwrap()),
    })
}

fn validate_dimensions(
    header: MngHeader,
    max_decode_pixels: u64,
    max_frames: u64,
) -> Result<(), String> {
    if header.width == 0
        || header.height == 0
        || header.width as u64 > MAX_SIDE
        || header.height as u64 > MAX_SIDE
    {
        return Err(format!(
            "DecodeDimensions {}x{} over limit",
            header.width, header.height
        ));
    }
    if header.nominal_frames as u64 > max_frames {
        return Err(format!(
            "FramesLimit {}>{max_frames}",
            header.nominal_frames
        ));
    }
    if header.nominal_frames > 0 {
        let pixels = (header.width as u64)
            .checked_mul(header.height as u64)
            .and_then(|pixels| pixels.checked_mul(header.nominal_frames as u64))
            .ok_or_else(|| "MNG nominal pixel count overflow".to_owned())?;
        if pixels > max_decode_pixels {
            return Err(format!("DecodePixels {pixels}>{max_decode_pixels}"));
        }
    }
    Ok(())
}

fn initialize_magick() -> Result<(), String> {
    static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
    INITIALIZED
        .get_or_init(|| unsafe {
            MagickWandGenesis();
            set_resource_limits()
        })
        .clone()
}

unsafe fn set_resource_limits() -> Result<(), String> {
    for (resource, limit) in [
        (1, 256 * 1024 * 1024),
        (2, 512 * 1024 * 1024),
        (5, 512 * 1024 * 1024),
        (6, 256 * 1024 * 1024),
        (7, 1),
        (10, MAX_SIDE),
        (11, 1000),
    ] {
        if MagickSetResourceLimit(resource, limit) == 0 {
            return Err(format!(
                "ImageMagick rejected resource limit {resource}={limit}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{decode, is_mng, parse_header, validate_dimensions};

    const TWO_FRAME_MNG: &[u8] = &[
        0x8a, 0x4d, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x1c, 0x4d, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x45,
        0x7e, 0xc6, 0x3f, 0x00, 0x00, 0x00, 0x0a, 0x54, 0x45, 0x52, 0x4d, 0x03, 0x00, 0x00, 0x00,
        0x00, 0x0a, 0x00, 0x00, 0x00, 0x03, 0xba, 0xac, 0x7d, 0x9a, 0x00, 0x00, 0x00, 0x01, 0x73,
        0x52, 0x47, 0x42, 0x00, 0xae, 0xce, 0x1c, 0xe9, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x01, 0x03, 0x00, 0x00, 0x00, 0x48,
        0x78, 0x9f, 0x67, 0x00, 0x00, 0x00, 0x03, 0x50, 0x4c, 0x54, 0x45, 0xff, 0x00, 0x00, 0x19,
        0xe2, 0x09, 0x37, 0x00, 0x00, 0x00, 0x07, 0x74, 0x49, 0x4d, 0x45, 0x07, 0xea, 0x09, 0x15,
        0x06, 0x1f, 0x06, 0x31, 0x68, 0x80, 0x81, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54,
        0x08, 0xd7, 0x63, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34, 0x27,
        0x0a, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82, 0x00, 0x00,
        0x00, 0x0a, 0x46, 0x52, 0x41, 0x4d, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x0a, 0xfa, 0x3e, 0x6d, 0x90, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x01, 0x03, 0x00, 0x00, 0x00, 0x48, 0x78, 0x9f, 0x67,
        0x00, 0x00, 0x00, 0x03, 0x50, 0x4c, 0x54, 0x45, 0x00, 0x00, 0xff, 0x8a, 0x78, 0xd2, 0x57,
        0x00, 0x00, 0x00, 0x07, 0x74, 0x49, 0x4d, 0x45, 0x07, 0xea, 0x09, 0x15, 0x06, 0x1f, 0x06,
        0x31, 0x68, 0x80, 0x81, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63,
        0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34, 0x27, 0x0a, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82, 0x00, 0x00, 0x00, 0x00, 0x4d,
        0x45, 0x4e, 0x44, 0x21, 0x20, 0xf7, 0xd5,
    ];

    fn header(width: u32, height: u32, frames: u32) -> Vec<u8> {
        let mut bytes = Vec::from(*b"\x8aMNG\r\n\x1a\n");
        bytes.extend_from_slice(&28u32.to_be_bytes());
        bytes.extend_from_slice(b"MHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&1000u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&frames.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes
    }

    #[test]
    fn detects_and_parses_mng_header() {
        let bytes = header(320, 240, 3);
        assert!(is_mng(&bytes));
        let parsed = parse_header(&bytes).unwrap();
        assert_eq!(parsed.width, 320);
        assert_eq!(parsed.height, 240);
        assert_eq!(parsed.nominal_frames, 3);
    }

    #[test]
    fn rejects_nominal_animation_over_pixel_budget() {
        let parsed = parse_header(&header(100, 100, 11)).unwrap();
        assert!(validate_dimensions(parsed, 100_000, 1000).is_err());
    }

    #[test]
    fn decodes_all_rendered_frames() {
        let (frames, loop_count) = decode(TWO_FRAME_MNG, 8, 2).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(loop_count, 3);
        assert_eq!(Duration::from(frames[0].delay()), Duration::from_millis(10));
        assert_eq!(
            Duration::from(frames[1].delay()),
            Duration::from_millis(100)
        );
        assert_eq!(frames[0].buffer().get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(frames[1].buffer().get_pixel(0, 0).0, [0, 0, 255, 255]);
    }
}
