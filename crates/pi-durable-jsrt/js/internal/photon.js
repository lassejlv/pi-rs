// pi-durable:internal/photon — the embedded runner's native substitute for the
// `@silvia-odwyer/photon` package (photon_rs.js, 0.3.3): the same photon-rs 0.3.3 Rust source as
// the package's WebAssembly, compiled natively (src/dispatch/images.rs `photon.*`). Only the
// exports the durable tests use: `initSync`, the default async init and the `PhotonImage`
// members below. Initialization is a no-op: nothing needs loading.
import { call } from "pi-durable:bridge";

export function initSync(_module) {
  return {};
}

export default async function init(_input) {
  return {};
}

const NATIVE = Symbol("photon");

/** The bytes as wasm-bindgen's `passArray8ToWasm0` reads them (copied on the native side). */
function bytes(input) {
  return input instanceof Uint8Array ? input : Uint8Array.from(input);
}

export class PhotonImage {
  constructor(rawPixels, width, height) {
    Object.defineProperty(this, NATIVE, { value: call("photon.new", bytes(rawPixels), width >>> 0, height >>> 0), writable: true });
  }

  static new_from_byteslice(vec) {
    const image = Object.create(PhotonImage.prototype);
    Object.defineProperty(image, NATIVE, { value: call("photon.fromByteslice", bytes(vec)), writable: true });
    return image;
  }

  get_width() {
    return call("photon.width", this[NATIVE]);
  }

  get_height() {
    return call("photon.height", this[NATIVE]);
  }

  get_raw_pixels() {
    return call("photon.rawPixels", this[NATIVE]);
  }

  get_bytes() {
    return call("photon.png", this[NATIVE]);
  }

  get_bytes_jpeg(quality) {
    return call("photon.jpeg", this[NATIVE], quality & 0xff);
  }

  free() {
    call("photon.free", this[NATIVE]);
  }
}
