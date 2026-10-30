//! Photon image preparation runs natively; no JavaScript or WebAssembly host.
pub mod cloudflare;
pub mod exif;
pub mod native;
use crate::tools::image_processor::*;
pub use cloudflare::create_cloudflare_photon_images;
pub use native::{NativePhotonImages, NodePhotonImagesOptions, create_native_photon_images};
use photon_rs::{
    PhotonImage,
    transform::{SamplingFilter, fliph, flipv, resize},
};
use std::sync::Arc;

pub fn create_photon_images() -> Arc<NativePhotonImages> {
    create_native_photon_images(NodePhotonImagesOptions {
        worker: false,
        ..Default::default()
    })
}
pub(crate) fn prepare(bytes: Vec<u8>, mime: String, limits: ImageLimits) -> Option<PreparedImage> {
    std::panic::catch_unwind(|| prepare_image(bytes, mime, limits))
        .ok()
        .flatten()
}
fn prepare_image(bytes: Vec<u8>, mime: String, limits: ImageLimits) -> Option<PreparedImage> {
    let orientation = exif::exif_orientation(&bytes);
    let mut image = photon_rs::native::open_image_from_bytes(&bytes).ok()?;
    if inline_image_type(&mime)
        && orientation == 1
        && f64::from(image.get_width()) <= limits.max_width
        && f64::from(image.get_height()) <= limits.max_height
        && base64_length(bytes.len() as f64) <= limits.max_bytes
    {
        return Some(PreparedImage {
            data: to_base64(&bytes),
            mime_type: mime,
            resized: None,
            converted_from: None,
        });
    }
    if orientation >= 5 {
        let (width, height) = (image.get_width(), image.get_height());
        let source = image.get_raw_pixels();
        let mut pixels = vec![0; source.len()];
        for y in 0..height {
            for x in 0..width {
                let out_x = if matches!(orientation, 5 | 8) {
                    y
                } else {
                    height - 1 - y
                };
                let out_y = if matches!(orientation, 5 | 6) {
                    x
                } else {
                    width - 1 - x
                };
                let from = (y as usize * width as usize + x as usize) * 4;
                let to = (out_y as usize * height as usize + out_x as usize) * 4;
                pixels[to..to + 4].copy_from_slice(&source[from..from + 4]);
            }
        }
        drop(image);
        image = PhotonImage::new(pixels, height, width);
    } else if orientation != 1 {
        if orientation != 4 {
            fliph(&mut image);
        }
        if orientation != 2 {
            flipv(&mut image);
        }
    }
    let from = ImageDimensions {
        width: image.get_width(),
        height: image.get_height(),
    };
    let sx = limits.max_width / f64::from(from.width);
    let sy = limits.max_height / f64::from(from.height);
    if sx.is_nan() || sy.is_nan() {
        return None;
    }
    let scale = 1.0f64.min(sx).min(sy);
    let width = (f64::from(from.width) * scale).round();
    let height = (f64::from(from.height) * scale).round();
    if width.is_nan() || height.is_nan() {
        return None;
    }
    let mut target = ImageDimensions {
        width: width.max(1.0) as u32,
        height: height.max(1.0) as u32,
    };
    loop {
        if let Some((data, mime_type)) =
            encode_within(&image, target, limits.max_bytes, mime == "image/jpeg")
        {
            let resized = if target.width == from.width && target.height == from.height {
                None
            } else {
                Some(ImageResize { from, to: target })
            };
            let converted_from = (mime_type != mime).then_some(mime);
            return Some(PreparedImage {
                data,
                mime_type,
                resized,
                converted_from,
            });
        }
        if target.width == 1 && target.height == 1 {
            return None;
        }
        target.width = ((f64::from(target.width) * 0.75).floor() as u32).max(1);
        target.height = ((f64::from(target.height) * 0.75).floor() as u32).max(1);
    }
}
fn encode_within(
    image: &PhotonImage,
    size: ImageDimensions,
    max_bytes: f64,
    jpeg_first: bool,
) -> Option<(String, String)> {
    let scaled = if size.width == image.get_width() && size.height == image.get_height() {
        None
    } else {
        Some(resize(
            image,
            size.width,
            size.height,
            SamplingFilter::Lanczos3,
        ))
    };
    let image = scaled.as_ref().unwrap_or(image);
    let qualities = if jpeg_first {
        vec![Some(80), Some(70), Some(55), Some(40), None]
    } else {
        vec![None, Some(80), Some(70), Some(55), Some(40)]
    };
    for quality in qualities {
        let bytes = quality.map_or_else(|| image.get_bytes(), |q| image.get_bytes_jpeg(q));
        if base64_length(bytes.len() as f64) <= max_bytes {
            return Some((
                to_base64(&bytes),
                if quality.is_some() {
                    "image/jpeg"
                } else {
                    "image/png"
                }
                .to_owned(),
            ));
        }
    }
    None
}
