use std::sync::Arc;

use image::{DynamicImage, ImageBuffer};
use resvg::usvg;

fn image_href_resolver(max_decode_pixels: u64) -> usvg::ImageHrefResolver<'static> {
    let resolve_data = usvg::ImageHrefResolver::default_data_resolver();
    usvg::ImageHrefResolver {
        resolve_data: Box::new(move |mime, data, options| {
            if let Some((width, height)) = crate::img::probe_dimensions(&data[..]) {
                if !crate::img::dimensions_allowed_for(
                    max_decode_pixels,
                    width as u64,
                    height as u64,
                ) {
                    return None;
                }
            }
            resolve_data(mime, data, options)
        }),
        resolve_string: Box::new(|_, _| None),
    }
}

pub(crate) fn render_svg(
    src_bytes: &[u8],
    fontdb: Arc<usvg::fontdb::Database>,
    size_hint: (u32, u32),
    max_decode_pixels: u64,
) -> Result<DynamicImage, ()> {
    let mut options = usvg::Options {
        fontdb: fontdb.clone(),
        image_href_resolver: image_href_resolver(max_decode_pixels),
        ..Default::default()
    };
    for f in fontdb.faces() {
        if let Some((name, _)) = f.families.first() {
            options.font_family = name.to_owned();
            break;
        }
    }
    let tree = usvg::Tree::from_data(src_bytes, &options).map_err(|_| ())?;
    let size = size(&tree);
    let (width, height, scale) =
        if size.width() > size_hint.0 as f32 || size.height() > size_hint.1 as f32 {
            let scale = f32::min(
                size_hint.0 as f32 / size.width(),
                size_hint.1 as f32 / size.height(),
            );
            let width = std::cmp::max((size.width() * scale).round() as u32, 1);
            let height = std::cmp::max((size.height() * scale).round() as u32, 1);
            (width, height, scale)
        } else {
            (size.width() as u32, size.height() as u32, 1f32)
        };
    let pixels = (width as u64).checked_mul(height as u64).ok_or(())?;
    if pixels > max_decode_pixels {
        return Err(());
    }
    let len = pixels
        .checked_mul(4)
        .and_then(|len| usize::try_from(len).ok())
        .ok_or(())?;
    let mut rgba = vec![0; len];
    let mut pxmap = resvg::tiny_skia::PixmapMut::from_bytes(&mut rgba, width, height).ok_or(())?;
    let transform = usvg::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pxmap);
    ImageBuffer::from_vec(width, height, rgba)
        .map(DynamicImage::ImageRgba8)
        .ok_or(())
}

pub(crate) async fn render_svg_blocking(
    src_bytes: Vec<u8>,
    fontdb: Arc<usvg::fontdb::Database>,
    size_hint: (u32, u32),
    max_decode_pixels: u64,
    timeout_ms: u64,
) -> Result<DynamicImage, ()> {
    let task = tokio::task::spawn_blocking(move || {
        render_svg(&src_bytes, fontdb, size_hint, max_decode_pixels)
    });
    let abort_handle = task.abort_handle();
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms.max(1)), task).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(()),
        Err(_) => {
            abort_handle.abort();
            Err(())
        }
    }
}

fn size(tree: &usvg::Tree) -> usvg::Size {
    let bb = tree.root().bounding_box();
    if bb.width() > tree.size().width() || bb.height() > tree.size().height() {
        if let Some(size) = usvg::Size::from_wh(bb.width(), bb.height()) {
            return size;
        }
    }
    tree.size()
}
