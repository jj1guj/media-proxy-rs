pub(crate) fn decode_animation(
    data: &[u8],
    max_decode_pixels: u64,
    max_frames: u64,
) -> Result<Option<(Vec<image::Frame>, u32)>, String> {
    let mut image = jxl_oxide::JxlImage::read_with_defaults(std::io::Cursor::new(data))
        .map_err(|error| format!("{error:?}"))?;
    let Some(animation) = image.image_header().metadata.animation.as_ref() else {
        return Ok(None);
    };
    let (tps_numerator, tps_denominator, loop_count) = (
        animation.tps_numerator,
        animation.tps_denominator,
        animation.num_loops,
    );
    if tps_numerator == 0 {
        return Err("Invalid animation tick rate".to_owned());
    }

    let frame_count = image.num_loaded_keyframes() as u64;
    if frame_count == 0 || frame_count > max_frames {
        return Err(format!("FramesLimit {}>{}", frame_count, max_frames));
    }
    let frame_pixels = u64::from(image.width())
        .checked_mul(u64::from(image.height()))
        .ok_or("JPEG XL dimensions overflow")?;
    let total_pixels = frame_pixels
        .checked_mul(frame_count)
        .ok_or("JPEG XL animation pixels overflow")?;
    if total_pixels > max_decode_pixels {
        return Err(format!(
            "DecodePixels {}>{}",
            total_pixels, max_decode_pixels
        ));
    }

    if image.pixel_format().has_black() {
        image.request_color_encoding(jxl_oxide::color::EnumColourEncoding::srgb(
            jxl_oxide::color::RenderingIntent::Relative,
        ));
    }

    let mut frames = Vec::with_capacity(frame_count as usize);
    for frame_index in 0..frame_count as usize {
        let render = image
            .render_frame(frame_index)
            .map_err(|error| format!("{error:?}"))?;
        let duration_ms = (u128::from(render.duration()) * u128::from(tps_denominator) * 1000)
            .checked_div(u128::from(tps_numerator))
            .ok_or("Invalid animation tick rate")?
            .min(u128::from(u64::MAX));
        let mut stream = render.stream();
        let width = stream.width();
        let height = stream.height();
        let channels = stream.channels() as usize;
        let sample_count = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(channels))
            .ok_or("JPEG XL frame dimensions overflow")?;
        let mut samples = vec![0u8; sample_count];
        stream.write_to_buffer(&mut samples);

        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        match channels {
            1 => samples
                .iter()
                .for_each(|&gray| rgba.extend_from_slice(&[gray, gray, gray, 255])),
            2 => samples.chunks_exact(2).for_each(|pixel| {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]])
            }),
            3 => samples
                .chunks_exact(3)
                .for_each(|pixel| rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255])),
            4 => rgba.extend_from_slice(&samples),
            _ => return Err(format!("Unsupported JPEG XL channel count {channels}")),
        }
        let buffer = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or("Invalid JPEG XL pixel buffer")?;
        let delay = image::Delay::from_saturating_duration(std::time::Duration::from_millis(
            duration_ms as u64,
        ));
        frames.push(image::Frame::from_parts(buffer, 0, 0, delay));
    }

    Ok(Some((frames, loop_count)))
}
