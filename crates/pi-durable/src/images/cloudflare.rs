//! Photon for Cloudflare Workers. The reference imports Photon's WebAssembly as a
//! compiled `WebAssembly.Module` and hands it to `createPhotonImages`, because
//! Workers cannot compile WebAssembly from bytes at runtime. Here the codec is Rust
//! compiled into the Worker itself: there is no module to import or initialize, and
//! no thread to spawn, so the processor runs on the calling task.

use super::native::{NativePhotonImages, NodePhotonImagesOptions, create_native_photon_images};
use std::sync::Arc;

/// `create_photon_images()` for Cloudflare Workers and Durable Objects.
pub fn create_cloudflare_photon_images() -> Arc<NativePhotonImages> {
    create_native_photon_images(NodePhotonImagesOptions {
        worker: false,
        ..Default::default()
    })
}
