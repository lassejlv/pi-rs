use crate::{Result, storage::StorageFuture};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageLimits {
    pub max_width: f64,
    pub max_height: f64,
    pub max_bytes: f64,
}
pub const DEFAULT_IMAGE_LIMITS: ImageLimits = ImageLimits {
    max_width: 2000.0,
    max_height: 2000.0,
    max_bytes: 4.5 * 1024.0 * 1024.0,
};
impl Default for ImageLimits {
    fn default() -> Self {
        DEFAULT_IMAGE_LIMITS
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageResize {
    pub from: ImageDimensions,
    pub to: ImageDimensions,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedImage {
    pub data: String,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resized: Option<ImageResize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub converted_from: Option<String>,
}
pub trait ImageProcessor: Send + Sync {
    fn prepare(
        &self,
        bytes: Vec<u8>,
        mime_type: String,
        limits: ImageLimits,
    ) -> StorageFuture<Option<PreparedImage>>;
}
pub type Images = Arc<dyn ImageProcessor>;
pub fn inline_image_type(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}
pub fn base64_length(bytes: f64) -> f64 {
    (bytes / 3.0).ceil() * 4.0
}
pub fn to_base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ImageModel {
    pub vision: bool,
    pub limits: ImageLimits,
}
impl Default for ImageModel {
    fn default() -> Self {
        Self {
            vision: true,
            limits: DEFAULT_IMAGE_LIMITS,
        }
    }
}
pub(crate) fn image_block(image: PreparedImage) -> Result<Arc<crate::JsonObject>> {
    let value = crate::wire::encode(
        &serde_json::json!({"type":"image","data":image.data,"mimeType":image.mime_type}),
    );
    crate::JsonObject::try_from_value(value)
        .map(Arc::new)
        .map_err(crate::Error::message)
}
