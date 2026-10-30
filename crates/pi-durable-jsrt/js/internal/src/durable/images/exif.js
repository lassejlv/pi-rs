// packages/durable/src/images/exif.ts (not an export entry): EXIF orientation over
// pi_durable::images::exif (src/dispatch/images.rs `image.exif`).
import { call } from "pi-durable:bridge";

export function exifOrientation(bytes) {
  return call("image.exif", bytes);
}
