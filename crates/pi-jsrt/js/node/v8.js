// node:v8 — V8's value serialization format (wire format version 15, byte-compatible with
// Node 22's v8.serialize/deserialize and Serializer/Deserializer), used by structured clone
// across threads (worker_threads), `serialization: 'advanced'` IPC and `v8.serialize`; plus
// placeholder heap statistics.

const Buffer = internal.Buffer;
const kLatestVersion = 15;

const Tag = {
  kVersion: 0xff,
  kPadding: 0x00,
  kVerifyObjectCount: 0x3f, // ?
  kTheHole: 0x2d, // -
  kUndefined: 0x5f, // _
  kNull: 0x30, // 0
  kTrue: 0x54, // T
  kFalse: 0x46, // F
  kInt32: 0x49, // I
  kUint32: 0x55, // U
  kDouble: 0x4e, // N
  kBigInt: 0x5a, // Z
  kUtf8String: 0x53, // S
  kOneByteString: 0x22, // "
  kTwoByteString: 0x63, // c
  kObjectReference: 0x5e, // ^
  kBeginJSObject: 0x6f, // o
  kEndJSObject: 0x7b, // {
  kBeginSparseJSArray: 0x61, // a
  kEndSparseJSArray: 0x40, // @
  kBeginDenseJSArray: 0x41, // A
  kEndDenseJSArray: 0x24, // $
  kDate: 0x44, // D
  kTrueObject: 0x79, // y
  kFalseObject: 0x78, // x
  kNumberObject: 0x6e, // n
  kBigIntObject: 0x7a, // z
  kStringObject: 0x73, // s
  kRegExp: 0x52, // R
  kBeginJSMap: 0x3b, // ;
  kEndJSMap: 0x3a, // :
  kBeginJSSet: 0x27, // '
  kEndJSSet: 0x2c, // ,
  kArrayBuffer: 0x42, // B
  kResizableArrayBuffer: 0x7e, // ~
  kArrayBufferTransfer: 0x74, // t
  kArrayBufferView: 0x56, // V
  kSharedArrayBuffer: 0x75, // u
  kSharedObject: 0x70, // p
  kWasmModuleTransfer: 0x77, // w
  kHostObject: 0x5c, // backslash
  kWasmMemoryTransfer: 0x6d, // m
  kError: 0x72, // r
  kLegacyReservedMessagePort: 0x4d, // M
  kLegacyReservedBlob: 0x62, // b
  kLegacyReservedBlobIndex: 0x69, // i
  kLegacyReservedFile: 0x66, // f
  kLegacyReservedFileIndex: 0x65, // e
  kLegacyReservedDOMFileSystem: 0x64, // d
  kLegacyReservedFileList: 0x6c, // l
  kLegacyReservedFileListIndex: 0x4c, // L
  kLegacyReservedImageData: 0x23, // #
  kLegacyReservedImageBitmap: 0x67, // g
  kLegacyReservedImageBitmapTransfer: 0x47, // G
  kLegacyReservedOffscreenCanvas: 0x48, // H
  kLegacyReservedCryptoKey: 0x4b, // K
  kLegacyReservedRTCCertificate: 0x6b, // k
};

const ErrorTag = {
  kEvalErrorPrototype: 0x45, // E
  kRangeErrorPrototype: 0x52, // R
  kReferenceErrorPrototype: 0x46, // F
  kSyntaxErrorPrototype: 0x53, // S
  kTypeErrorPrototype: 0x54, // T
  kUriErrorPrototype: 0x55, // U
  kMessage: 0x6d, // m
  kCause: 0x63, // c
  kStack: 0x73, // s
  kEnd: 0x2e, // .
};

// ArrayBufferView subtags.
const viewTags = new Map([
  [Int8Array, 0x62], // b
  [Uint8Array, 0x42], // B
  [Uint8ClampedArray, 0x43], // C
  [Int16Array, 0x77], // w
  [Uint16Array, 0x57], // W
  [Int32Array, 0x64], // d
  [Uint32Array, 0x44], // D
  [Float32Array, 0x66], // f
  [Float64Array, 0x46], // F
  [BigInt64Array, 0x71], // q
  [BigUint64Array, 0x51], // Q
  [DataView, 0x3f], // ?
]);
if (typeof Float16Array === "function") viewTags.set(Float16Array, 0x68); // h
const viewCtorByTag = new Map([...viewTags].map(([c, t]) => [t, c]));

function viewKind(view) {
  if (view instanceof DataView) return DataView;
  for (const ctor of viewTags.keys()) if (ctor !== DataView && view instanceof ctor) return ctor;
  return undefined;
}

// Arrays V8 creates with holey elements: deserialized dense arrays (`new Array(length)` filled
// in) are serialized again in the sparse format, as V8 does.
const holeyArrays = new WeakSet();

const objectToString = Object.prototype.toString;
const typeTag = (v) => objectToString.call(v).slice(8, -1);
const isNativeError = (v) => typeTag(v) === "Error" || v instanceof Error;

function cloneErrorMessage(value) {
  if (typeof value === "function") return `${Function.prototype.toString.call(value)} could not be cloned.`;
  if (typeof value === "symbol") return `${value.toString()} could not be cloned.`;
  let name = "Object";
  try {
    name = Object.getPrototypeOf(value)?.constructor?.name || typeTag(value);
  } catch {}
  return `#<${name}> could not be cloned.`;
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

class ValueSerializer {
  constructor(delegate) {
    this._delegate = delegate ?? null;
    this._buf = new Uint8Array(64);
    this._len = 0;
    this._ids = new Map();
    this._nextId = 0;
    this._transfers = new Map();
    this._viewsAsHost = false;
  }
  _ensure(n) {
    if (this._len + n <= this._buf.length) return;
    let size = this._buf.length * 2;
    while (size < this._len + n) size *= 2;
    const next = new Uint8Array(size);
    next.set(this._buf.subarray(0, this._len));
    this._buf = next;
  }
  _byte(b) {
    this._ensure(1);
    this._buf[this._len++] = b;
  }
  _varint(n) {
    // Unsigned LEB128 for values up to 2^53.
    n = Number(n);
    do {
      let b = n % 128;
      n = Math.floor(n / 128);
      if (n > 0) b |= 0x80;
      this._byte(b);
    } while (n > 0);
  }
  _varintBig(n) {
    do {
      let b = Number(n & 0x7fn);
      n >>= 7n;
      if (n > 0n) b |= 0x80;
      this._byte(b);
    } while (n > 0n);
  }
  _zigzag(n) {
    this._varint(n >= 0 ? n * 2 : -n * 2 - 1);
  }
  _double(d) {
    this._ensure(8);
    new DataView(this._buf.buffer, this._len, 8).setFloat64(0, d, true);
    this._len += 8;
  }
  _raw(bytes) {
    this._ensure(bytes.length);
    this._buf.set(bytes, this._len);
    this._len += bytes.length;
  }
  release() {
    const out = this._buf.subarray(0, this._len);
    this._buf = new Uint8Array(64);
    this._len = 0;
    this._ids = new Map();
    this._nextId = 0;
    return out;
  }
  writeHeader() {
    this._byte(Tag.kVersion);
    this._varint(kLatestVersion);
  }
  _writeString(s) {
    let oneByte = true;
    for (let i = 0; i < s.length; i++) {
      if (s.charCodeAt(i) > 0xff) {
        oneByte = false;
        break;
      }
    }
    if (oneByte) {
      this._byte(Tag.kOneByteString);
      this._varint(s.length);
      this._ensure(s.length);
      for (let i = 0; i < s.length; i++) this._buf[this._len++] = s.charCodeAt(i);
      return;
    }
    const byteLength = s.length * 2;
    let varintLen = 1;
    for (let n = byteLength; n >= 128; n = Math.floor(n / 128)) varintLen++;
    if ((this._len + 1 + varintLen) & 1) this._byte(Tag.kPadding);
    this._byte(Tag.kTwoByteString);
    this._varint(byteLength);
    this._ensure(byteLength);
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      this._buf[this._len++] = c & 0xff;
      this._buf[this._len++] = c >> 8;
    }
  }
  _writeBigIntContents(v) {
    const negative = v < 0n;
    let mag = negative ? -v : v;
    const bytes = [];
    while (mag > 0n) {
      bytes.push(Number(mag & 0xffn));
      mag >>= 8n;
    }
    while (bytes.length % 8) bytes.push(0);
    this._varint(bytes.length * 2 + (negative ? 1 : 0));
    this._raw(bytes);
  }
  _writeNumber(v) {
    if (Number.isInteger(v) && v >= -2147483648 && v <= 2147483647 && !Object.is(v, -0)) {
      this._byte(Tag.kInt32);
      this._zigzag(v);
    } else {
      this._byte(Tag.kDouble);
      this._double(v);
    }
  }
  writeValue(v) {
    switch (typeof v) {
      case "undefined":
        return this._byte(Tag.kUndefined);
      case "boolean":
        return this._byte(v ? Tag.kTrue : Tag.kFalse);
      case "number":
        return this._writeNumber(v);
      case "bigint":
        this._byte(Tag.kBigInt);
        return this._writeBigIntContents(v);
      case "string":
        return this._writeString(v);
      case "symbol":
        throw this._cloneError(cloneErrorMessage(v));
      case "function":
        throw this._cloneError(cloneErrorMessage(v));
    }
    if (v === null) return this._byte(Tag.kNull);
    return this._writeReceiver(v);
  }
  _cloneError(message) {
    if (this._delegate?._getDataCloneError) return new this._delegate._getDataCloneError(message);
    return new internal.DOMException(message, "DataCloneError");
  }
  _writeReceiver(v) {
    const existing = this._ids.get(v);
    if (existing !== undefined) {
      this._byte(Tag.kObjectReference);
      this._varint(existing);
      return;
    }
    // A view's buffer is written first (V8 assigns the buffer's id before the view's).
    if (ArrayBuffer.isView(v) && !this._viewsAsHost && !this._isHostObject(v)) {
      const buffer = v.buffer;
      if (!this._ids.has(buffer)) this._writeReceiver(buffer);
    }
    const id = this._nextId++;
    this._ids.set(v, id);
    if (Array.isArray(v)) return this._writeArray(v);
    if (this._isHostObject(v)) return this._writeHost(v);
    if (ArrayBuffer.isView(v)) {
      if (this._viewsAsHost) return this._writeHost(v);
      return this._writeView(v);
    }
    if (v instanceof ArrayBuffer) return this._writeArrayBuffer(v);
    if (typeof SharedArrayBuffer === "function" && v instanceof SharedArrayBuffer) {
      const sabId = this._delegate?._getSharedArrayBufferId?.(v);
      if (sabId === undefined) throw this._cloneError(cloneErrorMessage(v));
      this._byte(Tag.kSharedArrayBuffer);
      this._varint(sabId);
      return;
    }
    const tag = typeTag(v);
    switch (tag) {
      case "Date":
        this._byte(Tag.kDate);
        return this._double(v.getTime());
      case "RegExp": {
        this._byte(Tag.kRegExp);
        this._writeString(v.source);
        let flags = 0;
        const f = v.flags;
        if (f.includes("g")) flags |= 1;
        if (f.includes("i")) flags |= 2;
        if (f.includes("m")) flags |= 4;
        if (f.includes("y")) flags |= 8;
        if (f.includes("u")) flags |= 16;
        if (f.includes("s")) flags |= 32;
        if (f.includes("l")) flags |= 64;
        if (f.includes("d")) flags |= 128;
        if (f.includes("v")) flags |= 256;
        return this._varint(flags);
      }
      case "Boolean":
        return this._byte(Boolean.prototype.valueOf.call(v) ? Tag.kTrueObject : Tag.kFalseObject);
      case "Number":
        this._byte(Tag.kNumberObject);
        return this._double(Number.prototype.valueOf.call(v));
      case "String":
        this._byte(Tag.kStringObject);
        return this._writeString(String.prototype.valueOf.call(v));
      case "BigInt":
        this._byte(Tag.kBigIntObject);
        return this._writeBigIntContents(BigInt.prototype.valueOf.call(v));
      case "Map": {
        this._byte(Tag.kBeginJSMap);
        const entries = [...Map.prototype.entries.call(v)];
        for (const [k, val] of entries) {
          this.writeValue(k);
          this.writeValue(val);
        }
        this._byte(Tag.kEndJSMap);
        return this._varint(entries.length * 2);
      }
      case "Set": {
        this._byte(Tag.kBeginJSSet);
        const values = [...Set.prototype.values.call(v)];
        for (const val of values) this.writeValue(val);
        this._byte(Tag.kEndJSSet);
        return this._varint(values.length);
      }
    }
    if (isNativeError(v)) return this._writeError(v);
    if (v instanceof Promise || tag === "WeakMap" || tag === "WeakSet" || tag === "WeakRef" || tag === "Proxy" || tag === "Generator") {
      throw this._cloneError(cloneErrorMessage(v));
    }
    this._byte(Tag.kBeginJSObject);
    const n = this._writeProperties(v, Object.keys(v));
    this._byte(Tag.kEndJSObject);
    this._varint(n);
  }
  _isHostObject(v) {
    return this._delegate?._isHostObject?.(v) ?? false;
  }
  _writeHost(v) {
    this._byte(Tag.kHostObject);
    if (!this._delegate || typeof this._delegate._writeHostObject !== "function") {
      throw this._cloneError(`Unserializable host object: ${internal.inspect(v)}`);
    }
    this._delegate._writeHostObject(v);
  }
  _writeProperties(v, keys) {
    let n = 0;
    for (const key of keys) {
      if (!Object.prototype.propertyIsEnumerable.call(v, key)) continue;
      const value = v[key];
      const index = Number(key);
      if (String(index >>> 0) === key && index !== 4294967295) this._writeNumber(index);
      else this._writeString(key);
      this.writeValue(value);
      n++;
    }
    return n;
  }
  _writeArray(v) {
    const length = v.length;
    let dense = !holeyArrays.has(v);
    for (let i = 0; i < length; i++) {
      if (!Object.prototype.hasOwnProperty.call(v, i)) {
        dense = false;
        break;
      }
    }
    if (dense) {
      this._byte(Tag.kBeginDenseJSArray);
      this._varint(length);
      for (let i = 0; i < length; i++) this.writeValue(v[i]);
      const extra = Object.keys(v).filter((k) => !(String(Number(k) >>> 0) === k && Number(k) < length));
      const n = this._writeProperties(v, extra);
      this._byte(Tag.kEndDenseJSArray);
      this._varint(n);
      this._varint(length);
      return;
    }
    this._byte(Tag.kBeginSparseJSArray);
    this._varint(length);
    const n = this._writeProperties(v, Object.keys(v));
    this._byte(Tag.kEndSparseJSArray);
    this._varint(n);
    this._varint(length);
  }
  _writeArrayBuffer(v) {
    const transferId = this._transfers.get(v);
    if (transferId !== undefined) {
      this._byte(Tag.kArrayBufferTransfer);
      return this._varint(transferId);
    }
    if (v.resizable) {
      this._byte(Tag.kResizableArrayBuffer);
      this._varint(v.byteLength);
      this._varint(v.maxByteLength);
    } else {
      this._byte(Tag.kArrayBuffer);
      this._varint(v.byteLength);
    }
    this._raw(new Uint8Array(v));
  }
  _writeView(v) {
    const kind = viewKind(v);
    this._byte(Tag.kArrayBufferView);
    this._byte(viewTags.get(kind));
    this._varint(v.byteOffset);
    this._varint(v.byteLength);
    // Version 14+: flags (bit 0 length-tracking, bit 1 backed by a resizable buffer).
    this._varint(v.buffer.resizable ? 2 : 0);
  }
  _writeError(v) {
    this._byte(Tag.kError);
    let name;
    try {
      name = v.name;
    } catch {}
    const protoTag = {
      EvalError: ErrorTag.kEvalErrorPrototype,
      RangeError: ErrorTag.kRangeErrorPrototype,
      ReferenceError: ErrorTag.kReferenceErrorPrototype,
      SyntaxError: ErrorTag.kSyntaxErrorPrototype,
      TypeError: ErrorTag.kTypeErrorPrototype,
      URIError: ErrorTag.kUriErrorPrototype,
    }[name];
    if (protoTag) this._byte(protoTag);
    const messageDesc = Object.getOwnPropertyDescriptor(v, "message");
    if (messageDesc && "value" in messageDesc) {
      this._byte(ErrorTag.kMessage);
      this._writeString(String(messageDesc.value));
    }
    let stack;
    try {
      stack = v.stack;
    } catch {}
    if (typeof stack === "string") {
      this._byte(ErrorTag.kStack);
      this._writeString(stack);
    }
    const causeDesc = Object.getOwnPropertyDescriptor(v, "cause");
    if (causeDesc && "value" in causeDesc) {
      this._byte(ErrorTag.kCause);
      this.writeValue(causeDesc.value);
    }
    this._byte(ErrorTag.kEnd);
  }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

class ValueDeserializer {
  constructor(bytes, delegate) {
    this._buf = bytes;
    this._pos = 0;
    this._delegate = delegate ?? null;
    this._version = 0;
    this._objects = new Map();
    this._nextId = 0;
    this._transfers = new Map();
  }
  _error() {
    return new Error("Unable to deserialize cloned data.");
  }
  _byte() {
    if (this._pos >= this._buf.length) throw this._error();
    return this._buf[this._pos++];
  }
  _peek() {
    return this._pos < this._buf.length ? this._buf[this._pos] : undefined;
  }
  _varint() {
    let result = 0;
    let shift = 1;
    for (;;) {
      const b = this._byte();
      result += (b & 0x7f) * shift;
      shift *= 128;
      if (!(b & 0x80)) return result;
    }
  }
  _zigzag() {
    const n = this._varint();
    return n % 2 === 0 ? n / 2 : -(n + 1) / 2;
  }
  _double() {
    if (this._pos + 8 > this._buf.length) throw this._error();
    const d = new DataView(this._buf.buffer, this._buf.byteOffset + this._pos, 8).getFloat64(0, true);
    this._pos += 8;
    return d;
  }
  _raw(n) {
    if (this._pos + n > this._buf.length) throw this._error();
    const out = this._buf.subarray(this._pos, this._pos + n);
    this._pos += n;
    return out;
  }
  readHeader() {
    if (this._peek() === Tag.kVersion) {
      this._pos++;
      this._version = this._varint();
      if (this._version > kLatestVersion) throw new Error("Unable to deserialize cloned data due to invalid or unsupported version.");
    }
    return true;
  }
  _readOneByte(n) {
    const bytes = this._raw(n);
    let s = "";
    for (let i = 0; i < bytes.length; i += 8192) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
    return s;
  }
  _readTwoByte(n) {
    const bytes = this._raw(n);
    let s = "";
    for (let i = 0; i + 1 < bytes.length; i += 2) s += String.fromCharCode(bytes[i] | (bytes[i + 1] << 8));
    return s;
  }
  _readStringTag(tag) {
    switch (tag) {
      case Tag.kOneByteString:
        return this._readOneByte(this._varint());
      case Tag.kTwoByteString:
        return this._readTwoByte(this._varint());
      case Tag.kUtf8String:
        return new internal.TextDecoder().decode(this._raw(this._varint()));
    }
    throw this._error();
  }
  _readString() {
    let tag;
    do tag = this._byte();
    while (tag === Tag.kPadding);
    return this._readStringTag(tag);
  }
  _readBigIntContents() {
    const bitfield = this._varint();
    const bytes = this._raw(Math.floor(bitfield / 2));
    let v = 0n;
    for (let i = bytes.length - 1; i >= 0; i--) v = (v << 8n) | BigInt(bytes[i]);
    return bitfield & 1 ? -v : v;
  }
  _register(value) {
    this._objects.set(this._nextId++, value);
    return value;
  }
  readValue() {
    let tag;
    do tag = this._byte();
    while (tag === Tag.kPadding || tag === Tag.kVerifyObjectCount && (this._varint(), true));
    const v = this._readTagged(tag);
    // A buffer may be followed by a view on it.
    if ((tag === Tag.kArrayBuffer || tag === Tag.kResizableArrayBuffer || tag === Tag.kArrayBufferTransfer || tag === Tag.kSharedArrayBuffer) && this._peek() === Tag.kArrayBufferView) {
      this._pos++;
      return this._readView(v);
    }
    return v;
  }
  _readTagged(tag) {
    switch (tag) {
      case Tag.kUndefined:
        return undefined;
      case Tag.kNull:
        return null;
      case Tag.kTrue:
        return true;
      case Tag.kFalse:
        return false;
      case Tag.kInt32:
        return this._zigzag();
      case Tag.kUint32:
        return this._varint();
      case Tag.kDouble:
        return this._double();
      case Tag.kBigInt:
        return this._readBigIntContents();
      case Tag.kOneByteString:
      case Tag.kTwoByteString:
      case Tag.kUtf8String:
        return this._readStringTag(tag);
      case Tag.kObjectReference: {
        const id = this._varint();
        if (!this._objects.has(id)) throw this._error();
        return this._objects.get(id);
      }
      case Tag.kBeginJSObject: {
        const obj = this._register({});
        const n = this._readProperties(obj, Tag.kEndJSObject);
        if (this._varint() !== n) throw this._error();
        return obj;
      }
      case Tag.kBeginDenseJSArray: {
        const length = this._varint();
        const arr = this._register(new Array(length));
        holeyArrays.add(arr);
        for (let i = 0; i < length; i++) {
          if (this._peek() === Tag.kTheHole) {
            this._pos++;
            continue;
          }
          arr[i] = this.readValue();
        }
        const n = this._readProperties(arr, Tag.kEndDenseJSArray);
        if (this._varint() !== n) throw this._error();
        if (this._varint() !== length) throw this._error();
        return arr;
      }
      case Tag.kBeginSparseJSArray: {
        const length = this._varint();
        const arr = this._register(new Array(length));
        const n = this._readProperties(arr, Tag.kEndSparseJSArray);
        if (this._varint() !== n) throw this._error();
        if (this._varint() !== length) throw this._error();
        return arr;
      }
      case Tag.kDate:
        return this._register(new Date(this._double()));
      case Tag.kTrueObject:
        return this._register(Object(true));
      case Tag.kFalseObject:
        return this._register(Object(false));
      case Tag.kNumberObject:
        return this._register(Object(this._double()));
      case Tag.kBigIntObject:
        return this._register(Object(this._readBigIntContents()));
      case Tag.kStringObject:
        return this._register(Object(this._readString()));
      case Tag.kRegExp: {
        const source = this._readString();
        const bits = this._varint();
        let flags = "";
        if (bits & 128) flags += "d";
        if (bits & 1) flags += "g";
        if (bits & 2) flags += "i";
        if (bits & 64) flags += "l";
        if (bits & 4) flags += "m";
        if (bits & 32) flags += "s";
        if (bits & 16) flags += "u";
        if (bits & 256) flags += "v";
        if (bits & 8) flags += "y";
        return this._register(new RegExp(source, flags.replace("l", "")));
      }
      case Tag.kBeginJSMap: {
        const map = this._register(new Map());
        let n = 0;
        while (this._peek() !== Tag.kEndJSMap) {
          const k = this.readValue();
          const v = this.readValue();
          map.set(k, v);
          n += 2;
        }
        this._pos++;
        if (this._varint() !== n) throw this._error();
        return map;
      }
      case Tag.kBeginJSSet: {
        const set = this._register(new Set());
        let n = 0;
        while (this._peek() !== Tag.kEndJSSet) {
          set.add(this.readValue());
          n++;
        }
        this._pos++;
        if (this._varint() !== n) throw this._error();
        return set;
      }
      case Tag.kArrayBuffer: {
        const n = this._varint();
        const ab = new ArrayBuffer(n);
        new Uint8Array(ab).set(this._raw(n));
        return this._register(ab);
      }
      case Tag.kResizableArrayBuffer: {
        const n = this._varint();
        const max = this._varint();
        const ab = new ArrayBuffer(n, { maxByteLength: max });
        new Uint8Array(ab).set(this._raw(n));
        return this._register(ab);
      }
      case Tag.kArrayBufferTransfer: {
        const id = this._varint();
        if (!this._transfers.has(id)) throw this._error();
        return this._register(this._transfers.get(id));
      }
      case Tag.kSharedArrayBuffer: {
        const id = this._varint();
        const sab = this._delegate?._getSharedArrayBufferFromId?.(id);
        if (!sab) throw this._error();
        return this._register(sab);
      }
      case Tag.kArrayBufferView:
        // A view without a preceding buffer is invalid.
        throw this._error();
      case Tag.kError:
        return this._readError();
      case Tag.kHostObject: {
        const id = this._nextId++;
        if (!this._delegate || typeof this._delegate._readHostObject !== "function") throw this._error();
        const obj = this._delegate._readHostObject();
        this._objects.set(id, obj);
        return obj;
      }
    }
    throw this._error();
  }
  _readView(buffer) {
    const subtag = this._byte();
    const byteOffset = this._varint();
    const byteLength = this._varint();
    if (this._version >= 14) this._varint();
    const Ctor = viewCtorByTag.get(subtag);
    if (!Ctor) throw this._error();
    const view = Ctor === DataView ? new DataView(buffer, byteOffset, byteLength) : new Ctor(buffer, byteOffset, byteLength / Ctor.BYTES_PER_ELEMENT);
    return this._register(view);
  }
  _readProperties(obj, endTag) {
    let n = 0;
    while (this._peek() !== endTag) {
      if (this._peek() === undefined) throw this._error();
      const key = this.readValue();
      const value = this.readValue();
      obj[key] = value;
      n++;
    }
    this._pos++;
    return n;
  }
  _readError() {
    const id = this._nextId++;
    let Ctor = Error;
    let message;
    let stack;
    let cause;
    let hasCause = false;
    for (;;) {
      const t = this._byte();
      if (t === ErrorTag.kEnd) break;
      switch (t) {
        case ErrorTag.kEvalErrorPrototype:
          Ctor = EvalError;
          break;
        case ErrorTag.kRangeErrorPrototype:
          Ctor = RangeError;
          break;
        case ErrorTag.kReferenceErrorPrototype:
          Ctor = ReferenceError;
          break;
        case ErrorTag.kSyntaxErrorPrototype:
          Ctor = SyntaxError;
          break;
        case ErrorTag.kTypeErrorPrototype:
          Ctor = TypeError;
          break;
        case ErrorTag.kUriErrorPrototype:
          Ctor = URIError;
          break;
        case ErrorTag.kMessage:
          message = this._readString();
          break;
        case ErrorTag.kStack:
          stack = this._readString();
          break;
        case ErrorTag.kCause:
          hasCause = true;
          cause = this.readValue();
          break;
        default:
          throw this._error();
      }
    }
    const err = message === undefined ? new Ctor() : new Ctor(message);
    if (hasCause) Object.defineProperty(err, "cause", { value: cause, writable: true, configurable: true, enumerable: false });
    if (stack !== undefined) internal.hidden(err, "stack", stack);
    else internal.hidden(err, "stack", undefined);
    this._objects.set(id, err);
    return err;
  }
}

internal.ValueSerializer = ValueSerializer;
internal.ValueDeserializer = ValueDeserializer;

// ---------------------------------------------------------------------------
// node:v8 Serializer / Deserializer
// ---------------------------------------------------------------------------

const kSerializer = Symbol("serializer");
const kDeserializer = Symbol("deserializer");

function invalidArgType(name, expected, value) {
  const err = new TypeError(`The "${name}" argument must be ${expected}. Received ${internal.describeReceived(value)}`);
  err.code = "ERR_INVALID_ARG_TYPE";
  return err;
}

class Serializer {
  constructor() {
    const self = this;
    this[kSerializer] = new ValueSerializer({
      get _getDataCloneError() {
        return self._getDataCloneError;
      },
      _isHostObject: (v) => self[kSerializer]._viewsAsHost && ArrayBuffer.isView(v),
      _writeHostObject: (v) => self._writeHostObject(v),
      _getSharedArrayBufferId: (sab) => self._getSharedArrayBufferId(sab),
    });
  }
  writeHeader() {
    this[kSerializer].writeHeader();
  }
  writeValue(value) {
    this[kSerializer].writeValue(value);
    return true;
  }
  releaseBuffer() {
    const bytes = this[kSerializer].release();
    return Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }
  transferArrayBuffer(id, arrayBuffer) {
    if (!(arrayBuffer instanceof ArrayBuffer) && !(typeof SharedArrayBuffer === "function" && arrayBuffer instanceof SharedArrayBuffer)) {
      throw invalidArgType("arrayBuffer", "an instance of ArrayBuffer or SharedArrayBuffer", arrayBuffer);
    }
    this[kSerializer]._transfers.set(arrayBuffer, id >>> 0);
  }
  writeUint32(value) {
    this[kSerializer]._varint(value >>> 0);
  }
  writeUint64(hi, lo) {
    this[kSerializer]._varintBig((BigInt(hi >>> 0) << 32n) | BigInt(lo >>> 0));
  }
  writeDouble(value) {
    this[kSerializer]._double(Number(value));
  }
  writeRawBytes(source) {
    if (!ArrayBuffer.isView(source)) throw invalidArgType("source", "an instance of Buffer, TypedArray, or DataView", source);
    this[kSerializer]._raw(new Uint8Array(source.buffer, source.byteOffset, source.byteLength));
  }
  _writeHostObject(object) {
    throw new Error(`Unserializable host object: ${internal.inspect(object)}`);
  }
  _getDataCloneError(message) {
    return new Error(message);
  }
  _getSharedArrayBufferId() {
    throw new Error("Unserializable SharedArrayBuffer");
  }
  _setTreatArrayBufferViewsAsHostObjects(flag) {
    this[kSerializer]._viewsAsHost = Boolean(flag);
  }
}
Serializer.prototype._getDataCloneError = Error;

class Deserializer {
  constructor(buffer) {
    if (!ArrayBuffer.isView(buffer)) throw invalidArgType("buffer", "an instance of Buffer, TypedArray, or DataView", buffer);
    const bytes = new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength);
    this.buffer = buffer instanceof Buffer ? buffer : Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const self = this;
    this[kDeserializer] = new ValueDeserializer(bytes, {
      _readHostObject: () => self._readHostObject(),
    });
  }
  readHeader() {
    return this[kDeserializer].readHeader();
  }
  readValue() {
    return this[kDeserializer].readValue();
  }
  getWireFormatVersion() {
    return this[kDeserializer]._version;
  }
  transferArrayBuffer(id, arrayBuffer) {
    this[kDeserializer]._transfers.set(id >>> 0, arrayBuffer instanceof ArrayBuffer || ArrayBuffer.isView(arrayBuffer) === false ? arrayBuffer : arrayBuffer.buffer);
  }
  readUint32() {
    return this[kDeserializer]._varint() >>> 0;
  }
  readUint64() {
    const d = this[kDeserializer];
    let result = 0n;
    let shift = 0n;
    for (;;) {
      const b = d._byte();
      result |= BigInt(b & 0x7f) << shift;
      shift += 7n;
      if (!(b & 0x80)) break;
    }
    return [Number(result >> 32n) >>> 0, Number(result & 0xffffffffn) >>> 0];
  }
  readDouble() {
    return this[kDeserializer]._double();
  }
  _readRawBytes(length) {
    const d = this[kDeserializer];
    const offset = d._pos;
    d._raw(length);
    return offset;
  }
  readRawBytes(length) {
    const offset = this._readRawBytes(length);
    return Buffer.from(this.buffer.buffer, this.buffer.byteOffset + offset, length);
  }
  _readHostObject() {
    throw new Error("Unable to deserialize cloned data.");
  }
}

// Node's arrayBufferViewTypes order (index 10 is Buffer).
const arrayBufferViewTypes = [Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, Uint32Array, Float32Array, Float64Array, DataView, Buffer, BigInt64Array, BigUint64Array];
const arrayBufferViewTypeToIndex = new Map(arrayBufferViewTypes.map((t, i) => [t, i]));

class DefaultSerializer extends Serializer {
  constructor() {
    super();
    this._setTreatArrayBufferViewsAsHostObjects(true);
  }
  _writeHostObject(abView) {
    let i = 10;
    if (abView.constructor !== Buffer) {
      i = arrayBufferViewTypeToIndex.get(abView.constructor);
      if (i === undefined) {
        const kind = viewKind(abView);
        i = kind ? arrayBufferViewTypeToIndex.get(kind) : undefined;
      }
      if (i === undefined) throw new this._getDataCloneError(`Unserializable host object: ${internal.inspect(abView)}`);
    }
    this.writeUint32(i);
    this.writeUint32(abView.byteLength);
    this.writeRawBytes(new Uint8Array(abView.buffer, abView.byteOffset, abView.byteLength));
  }
}

class DefaultDeserializer extends Deserializer {
  _readHostObject() {
    const typeIndex = this.readUint32();
    const ctor = arrayBufferViewTypes[typeIndex];
    const byteLength = this.readUint32();
    const byteOffset = this._readRawBytes(byteLength);
    const BYTES_PER_ELEMENT = ctor?.BYTES_PER_ELEMENT ?? 1;
    const offset = this.buffer.byteOffset + byteOffset;
    if (ctor === undefined) throw new Error("Unable to deserialize cloned data.");
    if (offset % BYTES_PER_ELEMENT === 0) {
      return new ctor(this.buffer.buffer, offset, byteLength / BYTES_PER_ELEMENT);
    }
    // Copy to an aligned buffer first.
    const copy = Buffer.allocUnsafe(byteLength);
    Buffer.from(this.buffer.buffer, offset, byteLength).copy(copy);
    return new ctor(copy.buffer, copy.byteOffset, byteLength / BYTES_PER_ELEMENT);
  }
}

// Node's `serialization: 'advanced'` IPC format (lib/internal/child_process/serialization.js):
// host objects carry a type word, 0 for ArrayBufferViews.
class ChildProcessSerializer extends DefaultSerializer {
  _writeHostObject(object) {
    this.writeUint32(0);
    return super._writeHostObject(object);
  }
}
class ChildProcessDeserializer extends DefaultDeserializer {
  _readHostObject() {
    const type = this.readUint32();
    if (type !== 0) throw new Error("Unable to deserialize cloned data.");
    return super._readHostObject();
  }
}
internal.ipcSerialize = (value) => {
  const ser = new ChildProcessSerializer();
  ser.writeHeader();
  ser.writeValue(value);
  return ser.releaseBuffer();
};
internal.ipcDeserialize = (buffer) => {
  const der = new ChildProcessDeserializer(buffer);
  der.readHeader();
  return der.readValue();
};

function serialize(value) {
  const ser = new DefaultSerializer();
  ser.writeHeader();
  ser.writeValue(value);
  return ser.releaseBuffer();
}

function deserialize(buffer) {
  const der = new DefaultDeserializer(buffer);
  der.readHeader();
  return der.readValue();
}

// ---------------------------------------------------------------------------
// Structured clone across runtimes (worker_threads, MessagePort): V8's format with
// transferred ArrayBuffers (kArrayBufferTransfer) and MessagePorts / cloneable platform
// objects as host objects.
// ---------------------------------------------------------------------------

internal.cloneSerialize = (value, transferList = [], hooks = {}) => {
  const ports = [];
  const buffers = [];
  const ser = new ValueSerializer({
    _isHostObject: (v) => Boolean(hooks.isHost?.(v)),
    _writeHostObject: (v) => {
      const index = hooks.writeHost(v, ports);
      ser._varint(index);
    },
  });
  for (const t of transferList) {
    if (t instanceof ArrayBuffer) {
      if (ser._transfers.has(t)) throw new internal.DOMException("Transfer list contains duplicate", "DataCloneError");
      ser._transfers.set(t, buffers.length);
      buffers.push(t);
    }
  }
  ser.writeHeader();
  ser.writeValue(value);
  const bytes = ser.release().slice();
  // Transferring detaches the sender's buffers.
  const transferred = buffers.map((ab) => {
    const copy = ab.slice(0);
    try {
      if (typeof ab.transfer === "function") ab.transfer();
    } catch {}
    return copy;
  });
  return { bytes, ports, buffers: transferred };
};

internal.cloneDeserialize = (bytes, buffers = [], hooks = {}) => {
  const der = new ValueDeserializer(bytes, {
    _readHostObject: () => hooks.readHost(der._varint()),
  });
  buffers.forEach((ab, i) => der._transfers.set(i, ab));
  der.readHeader();
  return der.readValue();
};

// ---------------------------------------------------------------------------
// Module
// ---------------------------------------------------------------------------

function heapStatistics() {
  return { total_heap_size: 0, used_heap_size: 0, heap_size_limit: 0 };
}
const notSupported = (name) => () => {
  throw new Error(`v8.${name} is not supported in pi-jsrt`);
};

const v8 = {
  serialize,
  deserialize,
  Serializer,
  Deserializer,
  DefaultSerializer,
  DefaultDeserializer,
  cachedDataVersionTag: () => 0x5c3f2a17,
  setFlagsFromString(flags) {
    if (typeof flags !== "string") throw invalidArgType("flags", "of type string", flags);
    // `--expose-gc` reaches contexts created afterwards (node:vm), when this runtime has `gc`.
    if (/(^|\s)--expose[-_]gc(\s|$)/.test(flags)) internal.exposeGcInContexts = true;
  },
  setHeapSnapshotNearHeapLimit() {},
  isStringOneByteRepresentation(s) {
    if (typeof s !== "string") throw invalidArgType("content", "of type string", s);
    for (let i = 0; i < s.length; i++) if (s.charCodeAt(i) > 0xff) return false;
    return true;
  },
  getHeapStatistics: () => internal.v8HeapStatistics?.() ?? heapStatistics(),
  getHeapSpaceStatistics: () => internal.v8HeapSpaceStatistics?.() ?? [],
  getHeapCodeStatistics: () => ({ code_and_metadata_size: 0, bytecode_and_metadata_size: 0, external_script_source_size: 0, cpu_profiler_metadata_size: 0 }),
  getHeapSnapshot: (...args) => (internal.v8GetHeapSnapshot ?? notSupported("getHeapSnapshot"))(...args),
  writeHeapSnapshot: (...args) => (internal.v8WriteHeapSnapshot ?? notSupported("writeHeapSnapshot"))(...args),
  takeCoverage() {},
  stopCoverage() {},
  queryObjects() {
    return 0;
  },
  promiseHooks: {
    onInit: () => () => {},
    onSettled: () => () => {},
    onBefore: () => () => {},
    onAfter: () => () => {},
    createHook: () => () => {},
  },
  startupSnapshot: {
    isBuildingSnapshot: () => false,
    addSerializeCallback() {},
    addDeserializeCallback() {},
    setDeserializeMainFunction() {},
  },
  GCProfiler: class GCProfiler {
    start() {}
    stop() {
      return { version: 1, startTime: Date.now(), endTime: Date.now(), statistics: [] };
    }
  },
};
internal.defineBuiltin("v8", () => v8);
