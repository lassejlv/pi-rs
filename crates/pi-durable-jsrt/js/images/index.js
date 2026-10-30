// @earendil-works/pi-durable/images (packages/durable/src/images/index.ts): Photon image
// preparation, here native (src/dispatch/images.rs). The WebAssembly module argument is accepted
// and not needed: native Photon is always initialized.
import { photonImages } from "pi-durable:internal/images/native";

export function createPhotonImages(_wasm) {
  return photonImages(false, 30000);
}
