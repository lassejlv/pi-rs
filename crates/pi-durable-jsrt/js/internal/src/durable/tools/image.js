// packages/durable/src/tools/image.ts (not an export entry): image signatures and dimensions over
// pi_durable::tools::image (src/dispatch/images.rs `image.*`). The reader-based
// `detectSupportedImageMimeTypeOf` runs inside the native `read` tool and is not exposed.
import { call } from "pi-durable:bridge";

export function detectSupportedImageMimeType(buffer) {
  return call("image.detect", buffer);
}

export function imageDimensions(buffer, mimeType) {
  return call("image.dimensions", buffer, mimeType);
}
