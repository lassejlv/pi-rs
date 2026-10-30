// pi-durable:internal/images/native — the native Photon processor behind every processor the
// images modules create (src/dispatch/images.rs `image.*`).
import { call, callAsync, ref } from "pi-durable:bridge";

const natives = new WeakMap();

/** A processor over native Photon; `worker`: prepare off the calling thread. */
export function photonImages(worker, idleMs) {
  const native = call("image.photon", worker, idleMs);
  const prepare = (bytes, mimeType, limits) => {
    const wire = { maxWidth: limits.maxWidth, maxHeight: limits.maxHeight, maxBytes: limits.maxBytes };
    return worker
      ? callAsync("image.prepare", native, bytes, mimeType, wire)
      : Promise.resolve(call("image.prepareInline", native, bytes, mimeType, wire));
  };
  const processor = { prepare };
  natives.set(processor, { native, prepare });
  return processor;
}

/** What native tools take for an `images` option: the native processor, else the object itself. */
export function imagesOption(images) {
  if (images === undefined) return undefined;
  const entry = natives.get(images);
  return entry !== undefined && images.prepare === entry.prepare ? entry.native : ref(images);
}
