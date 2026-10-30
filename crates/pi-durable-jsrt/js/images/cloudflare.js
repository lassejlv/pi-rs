// @earendil-works/pi-durable/images/cloudflare (packages/durable/src/images/cloudflare.ts): the same
// native Photon processor (no Workers runtime here; structural parity only).
import { photonImages } from "pi-durable:internal/images/native";

export function createCloudflarePhotonImages() {
  return photonImages(false, 30000);
}
