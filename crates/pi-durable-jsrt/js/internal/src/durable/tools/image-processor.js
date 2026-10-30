// packages/durable/src/tools/image-processor.ts (not an export entry): image limits and base64
// over pi_durable::tools::image_processor (src/dispatch/images.rs `image.*`).
import { call } from "pi-durable:bridge";

export const DEFAULT_IMAGE_LIMITS = { maxWidth: 2000, maxHeight: 2000, maxBytes: 4.5 * 1024 * 1024 };

export const INLINE_IMAGE_TYPES = new Set(["image/png", "image/jpeg", "image/gif", "image/webp"]);

export function toBase64(bytes) {
  return call("image.base64", bytes);
}

export function base64Length(byteLength) {
  return call("image.base64Length", byteLength);
}
