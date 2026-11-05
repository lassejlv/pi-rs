// CompressionStream / DecompressionStream over node:zlib (gzip, deflate, deflate-raw).

const zlib = internal.requireBuiltin("zlib");
const formats = {
  compress: { gzip: zlib.createGzip, deflate: zlib.createDeflate, "deflate-raw": zlib.createDeflateRaw },
  decompress: { gzip: zlib.createGunzip, deflate: zlib.createInflate, "deflate-raw": zlib.createInflateRaw },
};

function makeStream(kind, format) {
  const create = formats[kind][format];
  if (!create) {
    throw new TypeError(`The provided value '${format}' is not a valid enum value of type CompressionFormat.`);
  }
  const engine = create();
  const run = (chunk, flush, controller) => {
    const out = engine._processChunk(chunk, flush);
    if (out.length) controller.enqueue(new Uint8Array(out.buffer, out.byteOffset, out.byteLength));
  };
  return new TransformStream({
    transform(chunk, controller) {
      if (!ArrayBuffer.isView(chunk) && !(chunk instanceof ArrayBuffer)) {
        throw new TypeError("The \"chunk\" argument must be of type ArrayBuffer or ArrayBufferView");
      }
      const bytes = chunk instanceof ArrayBuffer ? new Uint8Array(chunk) : new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
      run(bytes, zlib.constants.Z_NO_FLUSH, controller);
    },
    flush(controller) {
      run(new Uint8Array(0), zlib.constants.Z_FINISH, controller);
    },
  });
}

class CompressionStream {
  #stream;
  constructor(format) {
    this.#stream = makeStream("compress", format);
  }
  get readable() {
    return this.#stream.readable;
  }
  get writable() {
    return this.#stream.writable;
  }
}

class DecompressionStream {
  #stream;
  constructor(format) {
    this.#stream = makeStream("decompress", format);
  }
  get readable() {
    return this.#stream.readable;
  }
  get writable() {
    return this.#stream.writable;
  }
}

globalThis.CompressionStream = CompressionStream;
globalThis.DecompressionStream = DecompressionStream;
