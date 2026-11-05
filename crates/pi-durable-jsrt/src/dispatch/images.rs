//! `@earendil-works/pi-durable/images`, `/images/node`, `/images/cloudflare` (`js/images/*.js`)
//! and the image helpers of `tools/image.ts`, `tools/image-processor.ts` and `images/exif.ts`:
//! Photon decoding, orientation, resizing and encoding are `pi_durable::images`'s native Photon
//! (`image.*`); `worker` processors prepare on their own thread, leaving the event loop free.

use std::sync::Arc;

use pi_durable::images::{
    NativePhotonImages, NodePhotonImagesOptions, create_native_photon_images, exif,
};
use pi_durable::storage::StorageFuture;
use pi_durable::tools::image::{detect_supported_image_mime_type, image_dimensions};
use pi_durable::tools::image_processor::{
    ImageLimits, ImageProcessor, Images, PreparedImage, base64_length, to_base64,
};
use pi_jsrt::JsHandle;

use crate::errors::{HostError, HostResult};
use crate::host::{Args, Dispatch, Methods, call_async};
use crate::value::HostValue;

/// The images method group.
pub struct ImagesDispatch;

/// A native Photon processor behind a JS `ImageProcessor`.
pub struct NativeImages(pub Arc<NativePhotonImages>);

/// A JS `ImageProcessor` (`prepare(bytes, mimeType, limits)`) as a native one.
struct JsImages(JsHandle);

impl ImageProcessor for JsImages {
    fn prepare(
        &self,
        bytes: Vec<u8>,
        mime: String,
        limits: ImageLimits,
    ) -> StorageFuture<Option<PreparedImage>> {
        let processor = self.0.clone();
        Box::pin(async move {
            let prepared = call_async(
                &processor,
                Some("prepare"),
                vec![
                    HostValue::Bytes(bytes),
                    HostValue::string(&mime),
                    limits_value(&limits),
                ],
            )
            .await
            .map_err(|e| pi_durable::Error::host(e.name().to_string(), e.message().clone()))?;
            if matches!(prepared, HostValue::Undefined | HostValue::Null) {
                return Ok(None);
            }
            let prepared = prepared
                .to_strict_omitting_undefined()
                .map_err(|e| pi_durable::Error::message(e.message().to_string_lossy()))?;
            crate::dispatch::env::decode(&prepared)
                .map(Some)
                .map_err(pi_durable::Error::message)
        })
    }
}

/// The processor for a JS `images` option: the native one behind the glue's processors,
/// anything else called through JS; `undefined` is none.
pub fn images_from(value: &HostValue) -> HostResult<Option<Images>> {
    match value {
        HostValue::Undefined | HostValue::Null => Ok(None),
        HostValue::Native(_) => value
            .as_native::<NativeImages>()
            .map(|n| Some(n.0.clone() as Images))
            .ok_or_else(|| HostError::type_error("not an image processor")),
        HostValue::Handle(h) => Ok(Some(Arc::new(JsImages(h.clone())) as Images)),
        _ => Err(HostError::type_error("images must be an ImageProcessor")),
    }
}

fn limits_value(limits: &ImageLimits) -> HostValue {
    HostValue::object([
        ("maxWidth", HostValue::from(limits.max_width)),
        ("maxHeight", HostValue::from(limits.max_height)),
        ("maxBytes", HostValue::from(limits.max_bytes)),
    ])
}

fn limits(value: &HostValue) -> ImageLimits {
    let n = |key: &str| {
        value
            .get(key)
            .and_then(HostValue::as_f64)
            .unwrap_or(f64::NAN)
    };
    ImageLimits {
        max_width: n("maxWidth"),
        max_height: n("maxHeight"),
        max_bytes: n("maxBytes"),
    }
}

fn bytes(args: &Args, i: usize) -> HostResult<&[u8]> {
    args.get(i)
        .as_bytes()
        .ok_or_else(|| HostError::type_error(format!("argument {i} must be a Uint8Array")))
}

fn prepared_value(prepared: Option<PreparedImage>) -> HostValue {
    prepared
        .map(|p| HostValue::from_strict(&crate::dispatch::env::encode(&p)))
        .unwrap_or_default()
}

fn processor(args: &Args) -> HostResult<Arc<NativeImages>> {
    args.native::<NativeImages>(0)
}

impl Dispatch for ImagesDispatch {
    fn register(&self, m: &mut Methods) {
        photon(m);
        // (worker, idleMs) -> processor
        m.sync("image.photon", |_, args: Args| {
            let options = NodePhotonImagesOptions {
                worker: args.get(0).as_bool() != Some(false),
                idle_ms: args.get(1).as_f64().unwrap_or(30000.0),
            };
            Ok(HostValue::native(NativeImages(
                create_native_photon_images(options),
            )))
        });
        // An inline processor prepares on the calling (JS) thread, as upstream's does.
        m.sync("image.prepareInline", |_, args: Args| {
            let processor = processor(&args)?;
            let prepare = processor.0.prepare(
                bytes(&args, 1)?.to_vec(),
                args.text(2)?,
                limits(args.get(3)),
            );
            let prepared = futures::executor::block_on(prepare)
                .map_err(|e| HostError::new(crate::errors::ErrorClass::Error, e.to_string()))?;
            Ok(prepared_value(prepared))
        });
        m.asynchronous("image.prepare", |_, args: Args| async move {
            let processor = processor(&args)?;
            let prepare = processor.0.prepare(
                bytes(&args, 1)?.to_vec(),
                args.text(2)?,
                limits(args.get(3)),
            );
            let prepared = prepare
                .await
                .map_err(|e| HostError::new(crate::errors::ErrorClass::Error, e.to_string()))?;
            Ok(prepared_value(prepared))
        });
        m.sync("image.detect", |_, args: Args| {
            Ok(detect_supported_image_mime_type(bytes(&args, 0)?)
                .map(HostValue::from)
                .unwrap_or_default())
        });
        m.sync("image.dimensions", |_, args: Args| {
            Ok(image_dimensions(bytes(&args, 0)?, &args.text(1)?)
                .map(|d| {
                    HostValue::object([
                        ("width", HostValue::from(f64::from(d.width))),
                        ("height", HostValue::from(f64::from(d.height))),
                    ])
                })
                .unwrap_or_default())
        });
        m.sync("image.base64", |_, args: Args| {
            Ok(HostValue::string(&to_base64(bytes(&args, 0)?)))
        });
        m.sync("image.base64Length", |_, args: Args| {
            Ok(HostValue::from(base64_length(
                args.get(0).as_f64().unwrap_or(f64::NAN),
            )))
        });
        m.sync("image.exif", |_, args: Args| {
            Ok(HostValue::from(f64::from(exif::exif_orientation(bytes(
                &args, 0,
            )?))))
        });
    }
}

/// One `@silvia-odwyer/photon` `PhotonImage` (`js/internal/photon.js`); `free()` empties it.
struct Photon(parking_lot::Mutex<Option<photon_rs::PhotonImage>>);

/// Run photon-rs code that panics where its wasm build traps (`unwrap` on bad input); the wasm
/// trap surfaces in JS as `RuntimeError: unreachable`, so the panic does too.
fn photon_call<T>(f: impl FnOnce() -> T) -> HostResult<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| HostError::new(crate::errors::ErrorClass::Error, "unreachable"))
}

fn with_photon<T>(args: &Args, f: impl FnOnce(&photon_rs::PhotonImage) -> T) -> HostResult<T> {
    let photon = args.native::<Photon>(0)?;
    let image = photon.0.lock();
    let image = image.as_ref().ok_or_else(|| {
        HostError::new(
            crate::errors::ErrorClass::Error,
            "null pointer passed to rust",
        )
    })?;
    photon_call(|| f(image))
}

/// The `photon.*` methods behind the runner's native substitute for `@silvia-odwyer/photon`
/// (same photon-rs 0.3.3 source as the package's WebAssembly, compiled natively).
fn photon(m: &mut Methods) {
    m.sync("photon.new", |_, args: Args| {
        let raw = bytes(&args, 0)?.to_vec();
        let (width, height) = (args.f64(1)? as u32, args.f64(2)? as u32);
        Ok(HostValue::native(Photon(parking_lot::Mutex::new(Some(
            photon_rs::PhotonImage::new(raw, width, height),
        )))))
    });
    m.sync("photon.fromByteslice", |_, args: Args| {
        let data = bytes(&args, 0)?.to_vec();
        let image = photon_call(|| photon_rs::PhotonImage::new_from_byteslice(data))?;
        Ok(HostValue::native(Photon(parking_lot::Mutex::new(Some(
            image,
        )))))
    });
    m.sync("photon.width", |_, args: Args| {
        with_photon(&args, |i| HostValue::from(f64::from(i.get_width())))
    });
    m.sync("photon.height", |_, args: Args| {
        with_photon(&args, |i| HostValue::from(f64::from(i.get_height())))
    });
    m.sync("photon.rawPixels", |_, args: Args| {
        with_photon(&args, |i| HostValue::Bytes(i.get_raw_pixels()))
    });
    m.sync("photon.png", |_, args: Args| {
        with_photon(&args, |i| HostValue::Bytes(i.get_bytes()))
    });
    m.sync("photon.jpeg", |_, args: Args| {
        let quality = args.f64(1)? as u8;
        with_photon(&args, |i| HostValue::Bytes(i.get_bytes_jpeg(quality)))
    });
    m.sync("photon.free", |_, args: Args| {
        args.native::<Photon>(0)?.0.lock().take();
        Ok(HostValue::Undefined)
    });
}
