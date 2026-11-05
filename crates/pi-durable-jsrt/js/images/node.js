// @earendil-works/pi-durable/images/node (packages/durable/src/images/node.ts): native Photon
// preparing on a worker thread (retired after `idleMs` idle), or inline with `worker: false`.
import { photonImages } from "pi-durable:internal/images/native";

export async function createNodePhotonImages(options = {}) {
  return photonImages(options.worker !== false, options.idleMs ?? 30000);
}
