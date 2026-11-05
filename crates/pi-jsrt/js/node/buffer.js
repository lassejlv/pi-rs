// node:buffer — Buffer as a Uint8Array subclass.

const normEnc = (enc) => {
  if (enc === undefined || enc === null) return "utf8";
  switch (String(enc).toLowerCase()) {
    case "utf8":
    case "utf-8":
      return "utf8";
    case "hex":
      return "hex";
    case "base64":
      return "base64";
    case "base64url":
      return "base64url";
    case "latin1":
    case "binary":
      return "latin1";
    case "ascii":
      return "ascii";
    case "ucs2":
    case "ucs-2":
    case "utf16le":
    case "utf-16le":
      return "utf16le";
    default: {
      const err = new TypeError(`Unknown encoding: ${enc}`);
      err.code = "ERR_UNKNOWN_ENCODING";
      throw err;
    }
  }
};

function encodeString(str, enc) {
  switch (normEnc(enc)) {
    case "utf8":
      return ops.utf8Encode(str);
    case "hex": {
      const len = str.length >>> 1;
      const out = new Uint8Array(len);
      let i = 0;
      for (; i < len; i++) {
        const byte = parseInt(str.substr(i * 2, 2), 16);
        if (Number.isNaN(byte)) break;
        out[i] = byte;
      }
      return i === len ? out : out.subarray(0, i);
    }
    case "base64":
    case "base64url":
      return ops.base64Decode(str, false);
    case "latin1":
    case "ascii": {
      const out = new Uint8Array(str.length);
      for (let i = 0; i < str.length; i++) out[i] = str.charCodeAt(i) & 0xff;
      return out;
    }
    case "utf16le": {
      const out = new Uint8Array(str.length * 2);
      for (let i = 0; i < str.length; i++) {
        const c = str.charCodeAt(i);
        out[i * 2] = c & 0xff;
        out[i * 2 + 1] = c >> 8;
      }
      return out;
    }
  }
}

function decodeBytes(bytes, enc) {
  switch (normEnc(enc)) {
    case "utf8":
      return ops.utf8Decode(bytes, false);
    case "hex": {
      let s = "";
      for (let i = 0; i < bytes.length; i++) s += (bytes[i] < 16 ? "0" : "") + bytes[i].toString(16);
      return s;
    }
    case "base64":
      return ops.base64Encode(bytes, false);
    case "base64url":
      return ops.base64Encode(bytes, true);
    case "latin1": {
      let s = "";
      for (let i = 0; i < bytes.length; i += 4096) s += String.fromCharCode.apply(null, Array.from(bytes.subarray(i, i + 4096)));
      return s;
    }
    case "ascii": {
      let s = "";
      for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i] & 0x7f);
      return s;
    }
    case "utf16le": {
      let s = "";
      for (let i = 0; i + 1 < bytes.length; i += 2) s += String.fromCharCode(bytes[i] | (bytes[i + 1] << 8));
      return s;
    }
  }
}

const kMaxLength = 2 ** 32;

class Buffer extends Uint8Array {
  constructor(arg, encodingOrOffset, length) {
    if (typeof arg === "number") {
      super(arg);
    } else if (arg instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && arg instanceof SharedArrayBuffer)) {
      super(arg, encodingOrOffset ?? 0, length ?? arg.byteLength - (encodingOrOffset ?? 0));
    } else {
      super(arg);
    }
  }

  static get [Symbol.species]() {
    return Buffer;
  }

  static from(value, encodingOrOffset, length) {
    if (typeof value === "string") {
      const bytes = encodeString(value, encodingOrOffset);
      return new Buffer(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    }
    if (value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== "undefined" && value instanceof SharedArrayBuffer)) {
      const offset = encodingOrOffset === undefined ? 0 : Number(encodingOrOffset);
      const len = length === undefined ? value.byteLength - offset : Number(length);
      return new Buffer(value, offset, len);
    }
    if (ArrayBuffer.isView(value)) {
      const copy = new Buffer(value.byteLength);
      copy.set(new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
      if (!(value instanceof Uint8Array) && value.BYTES_PER_ELEMENT > 1) {
        // Typed arrays of wider elements copy element values (truncated), like Node.
        const out = new Buffer(value.length);
        for (let i = 0; i < value.length; i++) out[i] = value[i] & 0xff;
        return out;
      }
      return copy;
    }
    if (value === null || value === undefined) {
      throw new TypeError(
        `The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object. Received ${value}`,
      );
    }
    if (typeof value === "object") {
      if (typeof value.valueOf === "function") {
        const prim = value.valueOf();
        if (prim !== value && (typeof prim === "string" || typeof prim === "object")) return Buffer.from(prim, encodingOrOffset, length);
      }
      if (value.type === "Buffer" && Array.isArray(value.data)) return Buffer.from(value.data);
      if (typeof value[Symbol.toPrimitive] === "function") return Buffer.from(value[Symbol.toPrimitive]("string"), encodingOrOffset);
      const len = value.length >>> 0;
      const out = new Buffer(len);
      for (let i = 0; i < len; i++) out[i] = Number(value[i]) & 0xff;
      return out;
    }
    throw new TypeError(`The first argument must be of type string or an instance of Buffer, ArrayBuffer, or Array or an Array-like Object. Received type ${typeof value}`);
  }

  static alloc(size, fill, encoding) {
    const buf = new Buffer(size);
    if (fill !== undefined && fill !== 0) buf.fill(fill, 0, buf.length, encoding);
    return buf;
  }
  static allocUnsafe(size) {
    return new Buffer(size);
  }
  static allocUnsafeSlow(size) {
    return new Buffer(size);
  }
  static isBuffer(obj) {
    return obj instanceof Buffer;
  }
  static isEncoding(enc) {
    try {
      normEnc(enc);
      return typeof enc === "string";
    } catch {
      return false;
    }
  }
  static byteLength(value, encoding) {
    if (typeof value !== "string") {
      if (ArrayBuffer.isView(value) || value instanceof ArrayBuffer) return value.byteLength;
      throw new TypeError('The "string" argument must be of type string or an instance of Buffer or ArrayBuffer.');
    }
    const enc = normEnc(encoding);
    if (enc === "utf8") return ops.utf8Length(value);
    return encodeString(value, enc).length;
  }
  static concat(list, totalLength) {
    if (!Array.isArray(list)) throw new TypeError('The "list" argument must be an instance of Array.');
    if (totalLength === undefined) totalLength = list.reduce((n, b) => n + b.length, 0);
    const out = Buffer.alloc(totalLength);
    let pos = 0;
    for (const b of list) {
      if (pos >= totalLength) break;
      const src = b instanceof Uint8Array ? b : new Uint8Array(b);
      const n = Math.min(src.length, totalLength - pos);
      out.set(src.subarray(0, n), pos);
      pos += n;
    }
    return out;
  }
  static compare(a, b) {
    const len = Math.min(a.length, b.length);
    for (let i = 0; i < len; i++) {
      if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
    }
    return a.length < b.length ? -1 : a.length > b.length ? 1 : 0;
  }
  static of(...items) {
    return Buffer.from(items);
  }

  get parent() {
    return this.buffer;
  }
  get offset() {
    return this.byteOffset;
  }

  toString(encoding, start = 0, end = this.length) {
    start = Math.max(0, start | 0);
    end = Math.min(this.length, end === undefined ? this.length : end | 0);
    if (end <= start) return "";
    return decodeBytes(this.subarray(start, end), encoding);
  }
  toLocaleString(encoding, start, end) {
    return this.toString(encoding, start, end);
  }
  toJSON() {
    return { type: "Buffer", data: Array.from(this) };
  }
  equals(other) {
    if (!(other instanceof Uint8Array)) throw new TypeError('The "otherBuffer" argument must be an instance of Buffer or Uint8Array.');
    return Buffer.compare(this, other) === 0;
  }
  compare(target, targetStart = 0, targetEnd = target.length, sourceStart = 0, sourceEnd = this.length) {
    return Buffer.compare(this.subarray(sourceStart, sourceEnd), target.subarray(targetStart, targetEnd));
  }
  write(string, offset, length, encoding) {
    if (typeof offset === "string") {
      encoding = offset;
      offset = 0;
      length = undefined;
    } else if (typeof length === "string") {
      encoding = length;
      length = undefined;
    }
    offset = offset === undefined ? 0 : offset >>> 0;
    const bytes = encodeString(String(string), encoding);
    const n = Math.min(bytes.length, this.length - offset, length === undefined ? Infinity : length);
    this.set(bytes.subarray(0, n), offset);
    return n;
  }
  slice(start, end) {
    return this.subarray(start, end);
  }
  subarray(start, end) {
    const u = Uint8Array.prototype.subarray.call(this, start, end);
    return new Buffer(u.buffer, u.byteOffset, u.length);
  }
  copy(target, targetStart = 0, sourceStart = 0, sourceEnd = this.length) {
    sourceEnd = Math.min(sourceEnd, this.length);
    const n = Math.max(0, Math.min(sourceEnd - sourceStart, target.length - targetStart));
    if (n > 0) target.set(Uint8Array.prototype.subarray.call(this, sourceStart, sourceStart + n), targetStart);
    return n;
  }
  fill(value, offset = 0, end = this.length, encoding) {
    if (typeof offset === "string") {
      encoding = offset;
      offset = 0;
      end = this.length;
    } else if (typeof end === "string") {
      encoding = end;
      end = this.length;
    }
    if (typeof value === "string") {
      const bytes = encodeString(value, encoding);
      if (bytes.length === 0) return Uint8Array.prototype.fill.call(this, 0, offset, end);
      if (bytes.length === 1) return Uint8Array.prototype.fill.call(this, bytes[0], offset, end);
      for (let i = offset, j = 0; i < end; i++, j = (j + 1) % bytes.length) this[i] = bytes[j];
      return this;
    }
    if (value instanceof Uint8Array) {
      for (let i = offset, j = 0; i < end; i++, j = (j + 1) % value.length) this[i] = value[j];
      return this;
    }
    return Uint8Array.prototype.fill.call(this, Number(value) & 0xff, offset, end);
  }
  indexOf(value, byteOffset = 0, encoding) {
    if (typeof byteOffset === "string") {
      encoding = byteOffset;
      byteOffset = 0;
    }
    if (byteOffset < 0) byteOffset = Math.max(0, this.length + byteOffset);
    if (typeof value === "number") return Uint8Array.prototype.indexOf.call(this, value & 0xff, byteOffset);
    const needle = typeof value === "string" ? encodeString(value, encoding) : value;
    if (needle.length === 0) return byteOffset <= this.length ? byteOffset : this.length;
    outer: for (let i = byteOffset; i <= this.length - needle.length; i++) {
      for (let j = 0; j < needle.length; j++) if (this[i + j] !== needle[j]) continue outer;
      return i;
    }
    return -1;
  }
  lastIndexOf(value, byteOffset = this.length, encoding) {
    if (typeof value === "number") return Uint8Array.prototype.lastIndexOf.call(this, value & 0xff, byteOffset);
    const needle = typeof value === "string" ? encodeString(value, encoding) : value;
    outer: for (let i = Math.min(byteOffset, this.length - needle.length); i >= 0; i--) {
      for (let j = 0; j < needle.length; j++) if (this[i + j] !== needle[j]) continue outer;
      return i;
    }
    return -1;
  }
  includes(value, byteOffset, encoding) {
    return this.indexOf(value, byteOffset, encoding) !== -1;
  }
  #view() {
    return new DataView(this.buffer, this.byteOffset, this.byteLength);
  }
  readUInt8(o = 0) {
    return this.#view().getUint8(o);
  }
  readInt8(o = 0) {
    return this.#view().getInt8(o);
  }
  readUInt16LE(o = 0) {
    return this.#view().getUint16(o, true);
  }
  readUInt16BE(o = 0) {
    return this.#view().getUint16(o, false);
  }
  readInt16LE(o = 0) {
    return this.#view().getInt16(o, true);
  }
  readInt16BE(o = 0) {
    return this.#view().getInt16(o, false);
  }
  readUInt32LE(o = 0) {
    return this.#view().getUint32(o, true);
  }
  readUInt32BE(o = 0) {
    return this.#view().getUint32(o, false);
  }
  readInt32LE(o = 0) {
    return this.#view().getInt32(o, true);
  }
  readInt32BE(o = 0) {
    return this.#view().getInt32(o, false);
  }
  readFloatLE(o = 0) {
    return this.#view().getFloat32(o, true);
  }
  readFloatBE(o = 0) {
    return this.#view().getFloat32(o, false);
  }
  readDoubleLE(o = 0) {
    return this.#view().getFloat64(o, true);
  }
  readDoubleBE(o = 0) {
    return this.#view().getFloat64(o, false);
  }
  readBigUInt64LE(o = 0) {
    return this.#view().getBigUint64(o, true);
  }
  readBigUInt64BE(o = 0) {
    return this.#view().getBigUint64(o, false);
  }
  readBigInt64LE(o = 0) {
    return this.#view().getBigInt64(o, true);
  }
  readBigInt64BE(o = 0) {
    return this.#view().getBigInt64(o, false);
  }
  readUIntBE(o, n) {
    let v = 0;
    for (let i = 0; i < n; i++) v = v * 256 + this[o + i];
    return v;
  }
  readUIntLE(o, n) {
    let v = 0;
    for (let i = n - 1; i >= 0; i--) v = v * 256 + this[o + i];
    return v;
  }
  writeUInt8(v, o = 0) {
    this.#view().setUint8(o, v);
    return o + 1;
  }
  writeInt8(v, o = 0) {
    this.#view().setInt8(o, v);
    return o + 1;
  }
  writeUInt16LE(v, o = 0) {
    this.#view().setUint16(o, v, true);
    return o + 2;
  }
  writeUInt16BE(v, o = 0) {
    this.#view().setUint16(o, v, false);
    return o + 2;
  }
  writeInt16LE(v, o = 0) {
    this.#view().setInt16(o, v, true);
    return o + 2;
  }
  writeInt16BE(v, o = 0) {
    this.#view().setInt16(o, v, false);
    return o + 2;
  }
  writeUInt32LE(v, o = 0) {
    this.#view().setUint32(o, v, true);
    return o + 4;
  }
  writeUInt32BE(v, o = 0) {
    this.#view().setUint32(o, v, false);
    return o + 4;
  }
  writeInt32LE(v, o = 0) {
    this.#view().setInt32(o, v, true);
    return o + 4;
  }
  writeInt32BE(v, o = 0) {
    this.#view().setInt32(o, v, false);
    return o + 4;
  }
  writeFloatLE(v, o = 0) {
    this.#view().setFloat32(o, v, true);
    return o + 4;
  }
  writeFloatBE(v, o = 0) {
    this.#view().setFloat32(o, v, false);
    return o + 4;
  }
  writeDoubleLE(v, o = 0) {
    this.#view().setFloat64(o, v, true);
    return o + 8;
  }
  writeDoubleBE(v, o = 0) {
    this.#view().setFloat64(o, v, false);
    return o + 8;
  }
  writeBigUInt64LE(v, o = 0) {
    this.#view().setBigUint64(o, v, true);
    return o + 8;
  }
  writeBigUInt64BE(v, o = 0) {
    this.#view().setBigUint64(o, v, false);
    return o + 8;
  }
  swap16() {
    for (let i = 0; i + 1 < this.length; i += 2) [this[i], this[i + 1]] = [this[i + 1], this[i]];
    return this;
  }
  [Symbol.for("nodejs.util.inspect.custom")]() {
    const max = 50;
    let hex = "";
    for (let i = 0; i < Math.min(this.length, max); i++) hex += (i ? " " : "") + this[i].toString(16).padStart(2, "0");
    if (this.length > max) hex += ` ... ${this.length - max} more bytes`;
    return `<Buffer${hex ? " " + hex : ""}>`;
  }
}
for (const name of Object.getOwnPropertyNames(Buffer.prototype)) {
  const m = /^(read|write)(UInt|Uint)(.*)$/.exec(name);
  if (m && name.includes("UInt")) {
    const alias = name.replace("UInt", "Uint");
    if (!(alias in Buffer.prototype)) Buffer.prototype[alias] = Buffer.prototype[name];
  }
}
Buffer.poolSize = 8192;
// Node's internal per-encoding slice/write methods (used directly by some packages, e.g. ssh2).
for (const [prefix, enc] of [["utf8", "utf8"], ["latin1", "latin1"], ["ascii", "ascii"], ["hex", "hex"], ["base64", "base64"], ["base64url", "base64url"], ["ucs2", "utf16le"]]) {
  if (!Buffer.prototype[`${prefix}Slice`]) {
    Buffer.prototype[`${prefix}Slice`] = function (start = 0, end = this.length) {
      return this.toString(enc, start, end);
    };
  }
  if (!Buffer.prototype[`${prefix}Write`]) {
    Buffer.prototype[`${prefix}Write`] = function (string, offset = 0, length = this.length - offset) {
      return this.write(string, offset, length, enc);
    };
  }
}
if (!Buffer.copyBytesFrom) {
  Buffer.copyBytesFrom = (view, offset = 0, length) => {
    const bytes = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
    const size = view.BYTES_PER_ELEMENT ?? 1;
    const end = length === undefined ? view.length : offset + length;
    return Buffer.from(bytes.slice(offset * size, Math.min(end, view.length) * size));
  };
}
// Node's Buffer statics are enumerable own properties; packages copy them with for-in
// (safer-buffer).
for (const name of ["from", "copyBytesFrom", "of", "alloc", "allocUnsafe", "allocUnsafeSlow", "isBuffer", "compare", "isEncoding", "concat", "byteLength"]) {
  const desc = Object.getOwnPropertyDescriptor(Buffer, name);
  if (desc) Object.defineProperty(Buffer, name, { ...desc, enumerable: true });
}

class Blob {
  #bytes;
  #type;
  constructor(parts = [], options = {}) {
    const chunks = [];
    for (const p of parts) {
      if (typeof p === "string") chunks.push(ops.utf8Encode(p));
      else if (p instanceof Blob) chunks.push(p.#bytes);
      else if (p instanceof ArrayBuffer) chunks.push(new Uint8Array(p));
      else if (ArrayBuffer.isView(p)) chunks.push(new Uint8Array(p.buffer, p.byteOffset, p.byteLength));
      else chunks.push(ops.utf8Encode(String(p)));
    }
    this.#bytes = Buffer.concat(chunks);
    this.#type = options.type ? String(options.type).toLowerCase() : "";
  }
  get size() {
    return this.#bytes.length;
  }
  get type() {
    return this.#type;
  }
  async text() {
    return ops.utf8Decode(this.#bytes, false);
  }
  async arrayBuffer() {
    return this.#bytes.buffer.slice(this.#bytes.byteOffset, this.#bytes.byteOffset + this.#bytes.byteLength);
  }
  async bytes() {
    return new Uint8Array(this.#bytes);
  }
  slice(start, end, type) {
    const b = new Blob([this.#bytes.subarray(start, end)], { type });
    return b;
  }
  stream() {
    const bytes = this.#bytes;
    return new globalThis.ReadableStream({
      start(controller) {
        controller.enqueue(new Uint8Array(bytes));
        controller.close();
      },
    });
  }
  get [Symbol.toStringTag]() {
    return "Blob";
  }
}

class File extends Blob {
  #name;
  #lastModified;
  constructor(parts, name, options = {}) {
    super(parts, options);
    this.#name = String(name);
    this.#lastModified = options.lastModified ?? Date.now();
  }
  get name() {
    return this.#name;
  }
  get lastModified() {
    return this.#lastModified;
  }
}

internal.Buffer = Buffer;
internal.Blob = Blob;
internal.hidden(globalThis, "Buffer", Buffer);
internal.hidden(globalThis, "Blob", Blob);
internal.hidden(globalThis, "File", File);
internal.defineBuiltin("buffer", () => ({
  Buffer,
  Blob,
  File,
  kMaxLength,
  constants: { MAX_LENGTH: kMaxLength, MAX_STRING_LENGTH: 2 ** 29 - 24 },
  atob: globalThis.atob,
  btoa: globalThis.btoa,
  isUtf8: (b) => {
    try {
      ops.utf8Decode(b, true);
      return true;
    } catch {
      return false;
    }
  },
  SlowBuffer: Buffer,
}));
