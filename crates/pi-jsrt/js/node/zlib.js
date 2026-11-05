// node:zlib — deflate/inflate/gzip/brotli/zstd streams, one-shot APIs and crc32 over
// ops/zlib.rs. Node's internal handle (`stream._handle`, constructor `zlib.Zlib`-style with
// init/writeSync/close and the shared writeState array) is emulated for packages that drive it
// directly (ssh2).

const Buffer = internal.Buffer;
const { Transform } = internal.requireBuiltin("stream");

const constants = {
  Z_NO_FLUSH: 0, Z_PARTIAL_FLUSH: 1, Z_SYNC_FLUSH: 2, Z_FULL_FLUSH: 3, Z_FINISH: 4, Z_BLOCK: 5, Z_OK: 0,
  Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2, Z_DATA_ERROR: -3, Z_MEM_ERROR: -4,
  Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6, Z_NO_COMPRESSION: 0, Z_BEST_SPEED: 1, Z_BEST_COMPRESSION: 9,
  Z_DEFAULT_COMPRESSION: -1, Z_FILTERED: 1, Z_HUFFMAN_ONLY: 2, Z_RLE: 3, Z_FIXED: 4, Z_DEFAULT_STRATEGY: 0,
  ZLIB_VERNUM: 4880, DEFLATE: 1, INFLATE: 2, GZIP: 3, GUNZIP: 4, DEFLATERAW: 5, INFLATERAW: 6, UNZIP: 7,
  BROTLI_DECODE: 8, BROTLI_ENCODE: 9, ZSTD_DECOMPRESS: 11, ZSTD_COMPRESS: 10, Z_MIN_WINDOWBITS: 8,
  Z_MAX_WINDOWBITS: 15, Z_DEFAULT_WINDOWBITS: 15, Z_MIN_CHUNK: 64, Z_MAX_CHUNK: Infinity, Z_DEFAULT_CHUNK: 16384,
  Z_MIN_MEMLEVEL: 1, Z_MAX_MEMLEVEL: 9, Z_DEFAULT_MEMLEVEL: 8, Z_MIN_LEVEL: -1, Z_MAX_LEVEL: 9,
  Z_DEFAULT_LEVEL: -1, BROTLI_OPERATION_PROCESS: 0, BROTLI_OPERATION_FLUSH: 1, BROTLI_OPERATION_FINISH: 2,
  BROTLI_OPERATION_EMIT_METADATA: 3, BROTLI_PARAM_MODE: 0, BROTLI_MODE_GENERIC: 0, BROTLI_MODE_TEXT: 1,
  BROTLI_MODE_FONT: 2, BROTLI_DEFAULT_MODE: 0, BROTLI_PARAM_QUALITY: 1, BROTLI_MIN_QUALITY: 0,
  BROTLI_MAX_QUALITY: 11, BROTLI_DEFAULT_QUALITY: 11, BROTLI_PARAM_LGWIN: 2, BROTLI_MIN_WINDOW_BITS: 10,
  BROTLI_MAX_WINDOW_BITS: 24, BROTLI_LARGE_MAX_WINDOW_BITS: 30, BROTLI_DEFAULT_WINDOW: 22,
  BROTLI_PARAM_LGBLOCK: 3, BROTLI_MIN_INPUT_BLOCK_BITS: 16, BROTLI_MAX_INPUT_BLOCK_BITS: 24,
  BROTLI_PARAM_DISABLE_LITERAL_CONTEXT_MODELING: 4, BROTLI_PARAM_SIZE_HINT: 5, BROTLI_PARAM_LARGE_WINDOW: 6,
  BROTLI_PARAM_NPOSTFIX: 7, BROTLI_PARAM_NDIRECT: 8, BROTLI_DECODER_RESULT_ERROR: 0,
  BROTLI_DECODER_RESULT_SUCCESS: 1, BROTLI_DECODER_RESULT_NEEDS_MORE_INPUT: 2,
  BROTLI_DECODER_RESULT_NEEDS_MORE_OUTPUT: 3, BROTLI_DECODER_PARAM_DISABLE_RING_BUFFER_REALLOCATION: 0,
  BROTLI_DECODER_PARAM_LARGE_WINDOW: 1, BROTLI_DECODER_NO_ERROR: 0, BROTLI_DECODER_SUCCESS: 1,
  BROTLI_DECODER_NEEDS_MORE_INPUT: 2, BROTLI_DECODER_NEEDS_MORE_OUTPUT: 3,
  BROTLI_DECODER_ERROR_FORMAT_EXUBERANT_NIBBLE: -1, BROTLI_DECODER_ERROR_FORMAT_RESERVED: -2,
  BROTLI_DECODER_ERROR_FORMAT_EXUBERANT_META_NIBBLE: -3, BROTLI_DECODER_ERROR_FORMAT_SIMPLE_HUFFMAN_ALPHABET: -4,
  BROTLI_DECODER_ERROR_FORMAT_SIMPLE_HUFFMAN_SAME: -5, BROTLI_DECODER_ERROR_FORMAT_CL_SPACE: -6,
  BROTLI_DECODER_ERROR_FORMAT_HUFFMAN_SPACE: -7, BROTLI_DECODER_ERROR_FORMAT_CONTEXT_MAP_REPEAT: -8,
  BROTLI_DECODER_ERROR_FORMAT_BLOCK_LENGTH_1: -9, BROTLI_DECODER_ERROR_FORMAT_BLOCK_LENGTH_2: -10,
  BROTLI_DECODER_ERROR_FORMAT_TRANSFORM: -11, BROTLI_DECODER_ERROR_FORMAT_DICTIONARY: -12,
  BROTLI_DECODER_ERROR_FORMAT_WINDOW_BITS: -13, BROTLI_DECODER_ERROR_FORMAT_PADDING_1: -14,
  BROTLI_DECODER_ERROR_FORMAT_PADDING_2: -15, BROTLI_DECODER_ERROR_FORMAT_DISTANCE: -16,
  BROTLI_DECODER_ERROR_DICTIONARY_NOT_SET: -19, BROTLI_DECODER_ERROR_INVALID_ARGUMENTS: -20,
  BROTLI_DECODER_ERROR_ALLOC_CONTEXT_MODES: -21, BROTLI_DECODER_ERROR_ALLOC_TREE_GROUPS: -22,
  BROTLI_DECODER_ERROR_ALLOC_CONTEXT_MAP: -25, BROTLI_DECODER_ERROR_ALLOC_RING_BUFFER_1: -26,
  BROTLI_DECODER_ERROR_ALLOC_RING_BUFFER_2: -27, BROTLI_DECODER_ERROR_ALLOC_BLOCK_TYPE_TREES: -30,
  BROTLI_DECODER_ERROR_UNREACHABLE: -31, ZSTD_e_continue: 0, ZSTD_e_flush: 1, ZSTD_e_end: 2, ZSTD_fast: 1,
  ZSTD_dfast: 2, ZSTD_greedy: 3, ZSTD_lazy: 4, ZSTD_lazy2: 5, ZSTD_btlazy2: 6, ZSTD_btopt: 7, ZSTD_btultra: 8,
  ZSTD_btultra2: 9, ZSTD_c_compressionLevel: 100, ZSTD_c_windowLog: 101, ZSTD_c_hashLog: 102,
  ZSTD_c_chainLog: 103, ZSTD_c_searchLog: 104, ZSTD_c_minMatch: 105, ZSTD_c_targetLength: 106,
  ZSTD_c_strategy: 107, ZSTD_c_enableLongDistanceMatching: 160, ZSTD_c_ldmHashLog: 161, ZSTD_c_ldmMinMatch: 162,
  ZSTD_c_ldmBucketSizeLog: 163, ZSTD_c_ldmHashRateLog: 164, ZSTD_c_contentSizeFlag: 200,
  ZSTD_c_checksumFlag: 201, ZSTD_c_dictIDFlag: 202, ZSTD_c_nbWorkers: 400, ZSTD_c_jobSize: 401,
  ZSTD_c_overlapLog: 402, ZSTD_d_windowLogMax: 100, ZSTD_CLEVEL_DEFAULT: 3,
};
Object.freeze(constants);

const codes = {
  Z_OK: 0, Z_STREAM_END: 1, Z_NEED_DICT: 2, Z_ERRNO: -1, Z_STREAM_ERROR: -2, Z_DATA_ERROR: -3,
  Z_MEM_ERROR: -4, Z_BUF_ERROR: -5, Z_VERSION_ERROR: -6,
};
for (const key of Object.keys(codes)) codes[codes[key]] = key;

const kMaxLength = 2 ** 32;
const isBrotli = (mode) => mode === constants.BROTLI_DECODE || mode === constants.BROTLI_ENCODE;
const isZstd = (mode) => mode === constants.ZSTD_COMPRESS || mode === constants.ZSTD_DECOMPRESS;
const finishFlagFor = (mode) => (isBrotli(mode) || isZstd(mode) ? 2 : constants.Z_FINISH);

function invalidArgType(name, expected, actual) {
  const kind = name.includes(".") ? "property" : "argument";
  return Object.assign(new TypeError(`The "${name}" ${kind} must be ${expected}. Received ${internal.describeReceived(actual)}`), { code: "ERR_INVALID_ARG_TYPE" });
}

function toInput(buffer, name = "buffer") {
  if (typeof buffer === "string") return Buffer.from(buffer);
  if (ArrayBuffer.isView(buffer)) return buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength);
  if (buffer instanceof ArrayBuffer) return new Uint8Array(buffer);
  throw invalidArgType(name, "of type string or an instance of Buffer, TypedArray, DataView, or ArrayBuffer", buffer);
}

function zlibError(err) {
  if (err && typeof err === "object" && err.code !== undefined) return err;
  return err;
}

function checkRange(name, value, min, max, def, prefix = "options.") {
  name = prefix + name;
  if (value === undefined || (typeof value === "number" && Number.isNaN(value))) return def;
  if (typeof value !== "number") throw invalidArgType(name, "of type number", value);
  if (!Number.isFinite(value)) {
    throw Object.assign(new RangeError(`The value of "${name}" is out of range. It must be a finite number. Received ${value}`), { code: "ERR_OUT_OF_RANGE" });
  }
  if (value < min || value > max) {
    throw Object.assign(new RangeError(`The value of "${name}" is out of range. It must be >= ${min} and <= ${max}. Received ${value}`), { code: "ERR_OUT_OF_RANGE" });
  }
  return value;
}

/** Node's internal zlib/brotli handle (`stream._handle`). */
class ZlibHandle {
  constructor(mode) {
    this.mode = mode;
    this._native = null;
    this._pending = null;
    this._writeState = null;
    this._processCallback = null;
    this._level = -1;
    this._strategy = 0;
    this._params = undefined;
    this._finished = false;
    this.onerror = null;
  }

  // zlib: init(windowBits, level, memLevel, strategy, writeState, processCallback, dictionary)
  // brotli/zstd: init(params, writeState, processCallback)
  init(...args) {
    if (isBrotli(this.mode) || isZstd(this.mode)) {
      const [params, writeState, processCallback] = args;
      const flat = [];
      if (params) for (let i = 0; i < params.length; i++) if (params[i] !== 0xffffffff && params[i] !== -1) flat.push(i, params[i]);
      this._params = flat;
      this._writeState = writeState;
      this._processCallback = processCallback;
    } else {
      const [windowBits, level, memLevel, strategy, writeState, processCallback, dictionary] = args;
      this._level = level ?? -1;
      this._strategy = strategy ?? 0;
      this._windowBits = windowBits ?? 15;
      this._memLevel = memLevel ?? 8;
      this._dictionary = dictionary && dictionary.byteLength ? new Uint8Array(dictionary.buffer, dictionary.byteOffset, dictionary.byteLength) : undefined;
      this._writeState = writeState;
      this._processCallback = processCallback;
    }
    this._native = ops.zlibNew(this.mode, this._level, this._strategy, this._params, this._windowBits, this._memLevel, this._dictionary);
    return true;
  }

  params(level, strategy) {
    this._level = level;
    this._strategy = strategy;
    if (this._native) ops.zlibParams(this._native, level, strategy);
  }

  reset() {
    if (this._native) ops.zlibReset(this._native);
    this._pending = null;
    this._finished = false;
  }

  close() {
    this._native = null;
    this._pending = null;
  }

  /** Run the engine over a whole chunk and return all output (throws coded errors). */
  _run(input, flush) {
    if (!this._native) throw new Error("zlib binding closed");
    const out = ops.zlibWrite(this._native, input, flush);
    return Buffer.from(out.buffer, out.byteOffset, out.byteLength);
  }

  writeSync(flush, input, inOff, inLen, out, outOff, outLen) {
    this._write(flush, input, inOff, inLen, out, outOff, outLen);
  }

  write(flush, input, inOff, inLen, out, outOff, outLen) {
    this._write(flush, input, inOff, inLen, out, outOff, outLen);
    process.nextTick(() => this._processCallback?.call(this));
  }

  _write(flush, input, inOff, inLen, out, outOff, outLen) {
    try {
      const hasPending = this._pending && this._pending.length > 0;
      if (inLen > 0 || (!hasPending && flush !== constants.Z_NO_FLUSH && !this._finished)) {
        const chunk = inLen > 0 ? input.subarray(inOff, inOff + inLen) : new Uint8Array(0);
        const produced = this._run(chunk, flush);
        if (flush === finishFlagFor(this.mode)) this._finished = true;
        this._pending = hasPending ? Buffer.concat([this._pending, produced]) : produced;
      }
    } catch (err) {
      this._writeState[0] = outLen;
      this._writeState[1] = 0;
      if (this.onerror) this.onerror(err.message, err.errno, err.code);
      else throw err;
      return;
    }
    const pending = this._pending ?? Buffer.alloc(0);
    const n = Math.min(outLen, pending.length);
    if (n > 0) out.set(pending.subarray(0, n), outOff);
    this._pending = n < pending.length ? pending.subarray(n) : null;
    this._writeState[0] = outLen - n;
    this._writeState[1] = 0;
  }
}

class ZlibBase extends Transform {
  constructor(opts, mode) {
    opts = opts ?? {};
    super({ ...opts, decodeStrings: true });
    this._mode = mode;
    this.bytesWritten = 0;
    this._closed = false;
    this._chunkSize = checkRange("chunkSize", opts.chunkSize, constants.Z_MIN_CHUNK, Infinity, constants.Z_DEFAULT_CHUNK);
    this._maxOutputLength = checkRange("maxOutputLength", opts.maxOutputLength, 1, kMaxLength, kMaxLength);
    const brotliLike = isBrotli(mode) || isZstd(mode);
    this._defaultFlushFlag = checkRange("flush", opts.flush, 0, brotliLike ? 3 : 5, 0);
    this._finishFlushFlag = checkRange("finishFlush", opts.finishFlush, 0, brotliLike ? 3 : 5, finishFlagFor(mode));
    this._defaultFullFlushFlag = brotliLike ? 1 : constants.Z_FULL_FLUSH;
    this._outputLength = 0;
    this._handle = new ZlibHandle(mode);
    this._handle._owner = this;
    this._writeState = new Uint32Array(2);
    if (brotliLike) {
      const params = [];
      for (const [key, value] of Object.entries(opts.params ?? {})) params[Number(key)] = Number(value);
      const arr = new Uint32Array(Math.max(10, params.length)).fill(0xffffffff);
      params.forEach((v, i) => {
        if (v !== undefined) arr[i] = v;
      });
      this._handle.init(arr, this._writeState, () => {});
    } else {
      const level = checkRange("level", opts.level, -1, 9, -1);
      const strategy = checkRange("strategy", opts.strategy, 0, 4, 0);
      let windowBits;
      if ((opts.windowBits === undefined || opts.windowBits === null || opts.windowBits === 0) && (mode === constants.INFLATE || mode === constants.GUNZIP || mode === constants.UNZIP)) {
        windowBits = 0;
      } else {
        windowBits = checkRange("windowBits", opts.windowBits, constants.Z_MIN_WINDOWBITS + (mode === constants.GZIP ? 1 : 0), constants.Z_MAX_WINDOWBITS, constants.Z_DEFAULT_WINDOWBITS);
      }
      const memLevel = checkRange("memLevel", opts.memLevel, constants.Z_MIN_MEMLEVEL, constants.Z_MAX_MEMLEVEL, constants.Z_DEFAULT_MEMLEVEL);
      let dictionary = opts.dictionary;
      if (dictionary !== undefined && !ArrayBuffer.isView(dictionary)) {
        if (dictionary instanceof ArrayBuffer || (typeof SharedArrayBuffer === "function" && dictionary instanceof SharedArrayBuffer)) dictionary = Buffer.from(dictionary);
        else throw invalidArgType("options.dictionary", "an instance of Buffer, TypedArray, DataView, or ArrayBuffer", dictionary);
      }
      this._level = level;
      this._strategy = strategy;
      this._handle.init(windowBits, level, memLevel, strategy, this._writeState, () => {}, dictionary);
    }
  }

  get bytesRead() {
    return this.bytesWritten;
  }

  _process(chunk, flush) {
    const out = this._handle._run(chunk, flush);
    this.bytesWritten += chunk.length;
    this._outputLength += out.length;
    if (this._outputLength > this._maxOutputLength) {
      throw Object.assign(new RangeError(`Cannot create a Buffer larger than ${this._maxOutputLength} bytes`), { code: "ERR_BUFFER_TOO_LARGE" });
    }
    for (let i = 0; i < out.length; i += this._chunkSize) {
      this.push(i === 0 && out.length <= this._chunkSize ? out : out.subarray(i, i + this._chunkSize));
    }
  }

  /** Node's internal one-chunk processor (used by minizlib): sync without a callback. */
  _processChunk(chunk, flushFlag, callback) {
    if (typeof callback === "function") {
      try {
        this._process(chunk, flushFlag);
        process.nextTick(callback);
      } catch (err) {
        process.nextTick(callback, err);
      }
      return;
    }
    if (!this._handle._native) throw Object.assign(new Error("zlib binding closed"), { code: "ERR_ZLIB_BINDING_CLOSED" });
    const out = this._handle._run(chunk, flushFlag);
    this.bytesWritten += chunk.length;
    return Buffer.concat(out.length ? [out] : [], out.length);
  }

  _transform(chunk, encoding, callback) {
    if (this._closed) return callback(Object.assign(new Error("zlib binding closed"), { code: "ERR_ZLIB_BINDING_CLOSED" }));
    try {
      const buf = typeof chunk === "string" ? Buffer.from(chunk, encoding) : chunk;
      this._process(buf, this._defaultFlushFlag);
      callback();
    } catch (err) {
      this._handleError(err, callback);
    }
  }

  _flush(callback) {
    if (this._closed) return callback();
    try {
      this._process(new Uint8Array(0), this._finishFlushFlag);
      callback();
    } catch (err) {
      this._handleError(err, callback);
    }
  }

  _handleError(err, callback) {
    this._closed = true;
    this._handle.close();
    callback(err);
  }

  flush(kind, callback) {
    if (typeof kind === "function" || kind === undefined) {
      callback = kind;
      kind = this._defaultFullFlushFlag;
    }
    if (this.writableFinished || this._closed) {
      if (callback) process.nextTick(callback);
      return;
    }
    if (this.writableEnded) {
      if (callback) this.once("end", callback);
      return;
    }
    try {
      this._process(new Uint8Array(0), kind);
      if (callback) process.nextTick(callback);
    } catch (err) {
      this.destroy(err);
    }
  }

  params(level, strategy, callback) {
    checkRange("level", level, constants.Z_MIN_LEVEL, constants.Z_MAX_LEVEL, undefined, "");
    checkRange("strategy", strategy, constants.Z_DEFAULT_STRATEGY, constants.Z_FIXED, undefined, "");
    if (this._level !== level || this._strategy !== strategy) {
      this.flush(constants.Z_SYNC_FLUSH, () => {
        if (this._handle._native) this._handle.params(level, strategy);
        if (!this.destroyed) {
          this._level = level;
          this._strategy = strategy;
          if (callback) callback();
        }
      });
    } else if (callback) {
      process.nextTick(callback);
    }
  }

  reset() {
    this._handle.reset();
  }

  close(callback) {
    if (callback) process.nextTick(callback);
    this._closed = true;
    this._handle.close();
    this.destroy();
  }

  _destroy(err, callback) {
    this._handle.close();
    callback(err);
  }
}

function makeClass(name, mode) {
  const cls = {
    [name]: class extends ZlibBase {
      constructor(opts) {
        super(opts, mode);
      }
    },
  }[name];
  // Node's constructors also work without `new`.
  const callable = new Proxy(cls, {
    apply(target, thisArg, args) {
      return new target(...args);
    },
  });
  return callable;
}

const Deflate = makeClass("Deflate", constants.DEFLATE);
const Inflate = makeClass("Inflate", constants.INFLATE);
const Gzip = makeClass("Gzip", constants.GZIP);
const Gunzip = makeClass("Gunzip", constants.GUNZIP);
const DeflateRaw = makeClass("DeflateRaw", constants.DEFLATERAW);
const InflateRaw = makeClass("InflateRaw", constants.INFLATERAW);
const Unzip = makeClass("Unzip", constants.UNZIP);
const BrotliCompress = makeClass("BrotliCompress", constants.BROTLI_ENCODE);
const BrotliDecompress = makeClass("BrotliDecompress", constants.BROTLI_DECODE);
const ZstdCompress = makeClass("ZstdCompress", constants.ZSTD_COMPRESS);
const ZstdDecompress = makeClass("ZstdDecompress", constants.ZSTD_DECOMPRESS);

function syncProcess(Cls, buffer, opts) {
  const input = toInput(buffer);
  const engine = new Cls(opts);
  const out = engine._handle._run(input, engine._finishFlushFlag);
  if (out.length > engine._maxOutputLength) {
    throw Object.assign(new RangeError(`Cannot create a Buffer larger than ${engine._maxOutputLength} bytes`), { code: "ERR_BUFFER_TOO_LARGE" });
  }
  engine._handle.close();
  return opts?.info ? { buffer: out, engine } : out;
}

function makeSync(Cls) {
  return (buffer, opts) => syncProcess(Cls, buffer, opts);
}

function makeAsync(Cls) {
  return function (buffer, opts, callback) {
    if (typeof opts === "function") {
      callback = opts;
      opts = {};
    }
    if (typeof callback !== "function") throw invalidArgType("callback", "of type function", callback);
    let result;
    try {
      result = syncProcess(Cls, buffer, opts);
    } catch (err) {
      process.nextTick(callback, zlibError(err));
      return;
    }
    process.nextTick(callback, null, result);
  };
}

function crc32(data, value = 0) {
  if (typeof data !== "string" && !ArrayBuffer.isView(data)) {
    throw invalidArgType("data", "of type string or an instance of Buffer, TypedArray, or DataView", data);
  }
  if (typeof value !== "number") throw invalidArgType("value", "of type number", value);
  return ops.crc32(typeof data === "string" ? Buffer.from(data) : toInput(data), value >>> 0);
}

const zlib = {
  crc32,
  Deflate, Inflate, Gzip, Gunzip, DeflateRaw, InflateRaw, Unzip, BrotliCompress, BrotliDecompress, ZstdCompress, ZstdDecompress,
  deflate: makeAsync(Deflate), deflateSync: makeSync(Deflate),
  gzip: makeAsync(Gzip), gzipSync: makeSync(Gzip),
  deflateRaw: makeAsync(DeflateRaw), deflateRawSync: makeSync(DeflateRaw),
  unzip: makeAsync(Unzip), unzipSync: makeSync(Unzip),
  inflate: makeAsync(Inflate), inflateSync: makeSync(Inflate),
  gunzip: makeAsync(Gunzip), gunzipSync: makeSync(Gunzip),
  inflateRaw: makeAsync(InflateRaw), inflateRawSync: makeSync(InflateRaw),
  brotliCompress: makeAsync(BrotliCompress), brotliCompressSync: makeSync(BrotliCompress),
  brotliDecompress: makeAsync(BrotliDecompress), brotliDecompressSync: makeSync(BrotliDecompress),
  zstdCompress: makeAsync(ZstdCompress), zstdCompressSync: makeSync(ZstdCompress),
  zstdDecompress: makeAsync(ZstdDecompress), zstdDecompressSync: makeSync(ZstdDecompress),
  createDeflate: (o) => new Deflate(o),
  createInflate: (o) => new Inflate(o),
  createDeflateRaw: (o) => new DeflateRaw(o),
  createInflateRaw: (o) => new InflateRaw(o),
  createGzip: (o) => new Gzip(o),
  createGunzip: (o) => new Gunzip(o),
  createUnzip: (o) => new Unzip(o),
  createBrotliCompress: (o) => new BrotliCompress(o),
  createBrotliDecompress: (o) => new BrotliDecompress(o),
  createZstdCompress: (o) => new ZstdCompress(o),
  createZstdDecompress: (o) => new ZstdDecompress(o),
  constants,
  codes,
};
// Legacy top-level constants (zlib.Z_NO_FLUSH, ...).
for (const [key, value] of Object.entries(constants)) {
  if (key.startsWith("Z_") && !(key in zlib)) Object.defineProperty(zlib, key, { value, enumerable: false });
}
internal.ZlibHandle = ZlibHandle;
internal.defineBuiltin("zlib", () => zlib);
